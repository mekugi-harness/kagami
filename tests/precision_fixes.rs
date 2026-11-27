use kagami::config::KagamiConfig;
use kagami::graph::CodeGraph;
use kagami::graph::dead::{DeadOptions, DeadScope, analyze_dead};
use kagami::ir::IrNode;
use kagami::ir::similar::pair_similarity;
use kagami::lang;
use kagami::model::{Confidence, Language};
use kagami::walker::discover_files;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_vite_spa_main_ts_links_svelte_app() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src/lib")).unwrap();

    fs::write(
        root.join("src/main.ts"),
        "import { mount } from \"svelte\";\nimport App from \"./App.svelte\";\nimport { bootApp as start } from \"./lib/setup\";\n\nstart();\nmount(App, { target: rootEl });\n",
    ).unwrap();

    let tag_open = concat!("<", "script lang=\"ts\">");
    let tag_close = concat!("</", "script>");
    fs::write(
        root.join("src/App.svelte"),
        format!("{}\nimport Child from \"./Child.svelte\";\nfunction handleClick() {{}}\n{}\n<Child />\n<div>{{handleClick()}}</div>\n", tag_open, tag_close),
    ).unwrap();
    fs::write(
        root.join("src/Child.svelte"),
        format!(
            "{}\nfunction childWork() {{}}\nchildWork();\n{}\n<div>Hi</div>\n",
            tag_open, tag_close
        ),
    )
    .unwrap();
    fs::write(
        root.join("src/Orphan.svelte"),
        format!(
            "{}\nfunction orphanWork() {{}}\n{}\n<div>Orphan</div>\n",
            tag_open, tag_close
        ),
    )
    .unwrap();
    fs::write(
        root.join("src/lib/setup.ts"),
        "export function bootApp() {}\nexport function deadUtil() {}\n",
    )
    .unwrap();

    let ignores = KagamiConfig::default().build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();

    let has_resolved_app_edge = graph.edges.iter().any(|e| {
        e.file == "src/main.ts"
            && e.from.contains("<module>")
            && e.to.starts_with("src/App.svelte::App@")
            && e.confidence == Confidence::Resolved
    });
    assert!(
        has_resolved_app_edge,
        "expected Confidence::Resolved edge from src/main.ts::<module> to src/App.svelte::App"
    );

    let dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
        },
    );
    let has = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);
    assert!(
        !has("App"),
        "App.svelte must be live via main.ts mount(App, ...)"
    );
    assert!(!has("Child"), "Child.svelte must be live via App.svelte");
    assert!(
        !has("handleClick"),
        "handleClick must be live via App.svelte template"
    );
    assert!(
        !has("childWork"),
        "childWork must be live via Child.svelte <module>"
    );
    assert!(
        !has("bootApp"),
        "bootApp must be live via aliased relative import start()"
    );
    assert!(has("Orphan"), "Orphan.svelte should be dead");
    assert!(has("deadUtil"), "deadUtil should be dead");
}

#[test]
fn test_go_cross_pkg_receiver_var_method_and_testsupport() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("internal/hints")).unwrap();
    fs::create_dir_all(root.join("internal/testsupport")).unwrap();
    fs::create_dir_all(root.join("custom_helpers")).unwrap();
    fs::create_dir_all(root.join("cmd/app")).unwrap();

    fs::write(
        root.join(".kagami.toml"),
        "test_paths = [\"custom_helpers/**\"]\n",
    )
    .unwrap();
    fs::write(
        root.join("internal/hints/hints.go"),
        "package hints\ntype SelectionHints struct{}\nfunc (h *SelectionHints) ActionAt() {}\nfunc (h *SelectionHints) UnusedAction() {}\n",
    ).unwrap();
    fs::write(
        root.join("internal/testsupport/fixture.go"),
        "package testsupport\nfunc MakeFixture() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("custom_helpers/helper.go"),
        "package custom_helpers\nfunc CustomHelper() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("cmd/app/main.go"),
        "package main\nimport \"demo.local/app/internal/hints\"\nfunc main() {\n    var receiverVar *hints.SelectionHints\n    receiverVar.ActionAt()\n}\n",
    ).unwrap();

    let cfg = KagamiConfig::load(root).unwrap();
    let ignores = cfg.build_ignore_globset().unwrap();
    let test_paths = cfg.build_test_paths_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let mut graph = CodeGraph::build(root, &files, false).unwrap();
    if let Some(ref tp) = test_paths {
        graph.apply_test_paths(tp);
    }

    let dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
        },
    );
    let has = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);
    assert!(
        !has("SelectionHints.ActionAt"),
        "cross-pkg receiver-var call must keep ActionAt alive"
    );
    assert!(
        has("SelectionHints.UnusedAction"),
        "uncalled method should be dead"
    );
    assert!(
        !has("MakeFixture"),
        "testsupport/ package must be classified as is_test: true"
    );
    assert!(
        !has("CustomHelper"),
        "test_paths glob in .kagami.toml must mark custom_helpers as is_test: true"
    );
}

#[test]
fn test_dead_min_confidence_filters_heuristic() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(
        root.join("main.go"),
        "package main\ntype Svc struct{}\nfunc (s *Svc) DynamicMethod() {}\nfunc UnusedFunc() {}\nfunc main() {}\n",
    ).unwrap();

    let ignores = KagamiConfig::default().build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();

    let all_dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
        },
    );
    assert_eq!(
        all_dead.total_dead,
        all_dead.unreferenced + all_dead.unreachable_clusters
    );
    assert!(
        all_dead
            .symbols
            .iter()
            .any(|s| s.symbol == "Svc.DynamicMethod" && s.confidence == Confidence::Heuristic)
    );
    assert!(
        all_dead
            .symbols
            .iter()
            .any(|s| s.symbol == "UnusedFunc" && s.confidence == Confidence::Resolved)
    );

    let resolved_only = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Resolved,
        },
    );
    assert_eq!(
        resolved_only.total_dead,
        resolved_only.unreferenced + resolved_only.unreachable_clusters
    );
    assert!(
        !resolved_only
            .symbols
            .iter()
            .any(|s| s.symbol == "Svc.DynamicMethod")
    );
    assert!(
        resolved_only
            .symbols
            .iter()
            .any(|s| s.symbol == "UnusedFunc")
    );

    let exact_only = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Exact,
        },
    );
    assert_eq!(exact_only.total_dead, 0);
}

#[test]
fn test_ts_richer_ir_lowering_and_equivalence() {
    let src_a = "async function processItems(items: number[], flag: boolean) {\n  try {\n    const mapped = mapItems(items, (x) => x + 1);\n    const payload = await fetchData({ mapped, list: [2, 5, ...mapped] });\n    return flag ? payload.ok : false;\n  } catch (err) {\n    throw err;\n  } finally {\n    cleanup();\n  }\n}\n";
    let src_b = "async function handleValues(vals: number[], active: boolean) {\n  try {\n    const mapped = mapItems(vals, (v) => v + 1);\n    const resp = await fetchData({ mapped: mapped, list: [3, 7, ...mapped] });\n    return active ? resp.ok : false;\n  } catch (e) {\n    throw e;\n  } finally {\n    cleanup();\n  }\n}\n";

    let ext_a = lang::extract_file("a.ts", Language::TypeScript, src_a, "a1".into()).unwrap();
    let ext_b = lang::extract_file("b.ts", Language::TypeScript, src_b, "b1".into()).unwrap();
    assert_eq!(ext_a.ir_funcs.len(), 1);
    assert_eq!(ext_b.ir_funcs.len(), 1);

    fn has_opaque(n: &IrNode) -> bool {
        match n {
            IrNode::Opaque(_) => true,
            IrNode::Seq(items) => items.iter().any(has_opaque),
            IrNode::Branch {
                cond,
                then_br,
                else_br,
            } => {
                has_opaque(cond)
                    || has_opaque(then_br)
                    || else_br.as_deref().map(has_opaque).unwrap_or(false)
            }
            IrNode::Call { args, .. } => args.iter().any(has_opaque),
            IrNode::Assign { val, .. }
            | IrNode::PropagateErr(val)
            | IrNode::UnOp { expr: val, .. }
            | IrNode::Member { obj: val, .. } => has_opaque(val),
            IrNode::Return(v) => v.as_deref().map(has_opaque).unwrap_or(false),
            IrNode::BinOp { lhs, rhs, .. } => has_opaque(lhs) || has_opaque(rhs),
            IrNode::Index { obj, idx } => has_opaque(obj) || has_opaque(idx),
            _ => false,
        }
    }

    assert!(
        !has_opaque(&ext_a.ir_funcs[0].body),
        "expected zero Opaque nodes, got {:?}",
        ext_a.ir_funcs[0].body
    );
    assert!(
        !has_opaque(&ext_b.ir_funcs[0].body),
        "expected zero Opaque nodes, got {:?}",
        ext_b.ir_funcs[0].body
    );
    let score = pair_similarity(&ext_a.ir_funcs[0], &ext_b.ir_funcs[0], false);
    assert!(
        score >= 0.95,
        "expected high similarity for equivalent TS funcs, got {}",
        score
    );
}

#[test]
fn test_review_fixes_default_export_shadowing_typeof_and_computed_key() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src/worker")).unwrap();

    // Renamed default export + shadowed parameter + typeof type query + module path priority
    fs::write(
        root.join("src/worker.ts"),
        "export default function initialize() {}\nexport { run } from \"./impl\";\n",
    )
    .unwrap();
    fs::write(
        root.join("src/worker/index.ts"),
        "export function run() {}\n",
    )
    .unwrap();
    let tag_open = concat!("<", "script lang=\"ts\">");
    let tag_close = concat!("</", "script>");
    fs::write(
        root.join("src/UnusedComp.svelte"),
        format!(
            "{}\nfunction unusedWork() {{}}\n{}\n<div>No</div>\n",
            tag_open, tag_close
        ),
    )
    .unwrap();
    fs::write(
        root.join("src/main.ts"),
        "import start from \"./worker\";\nimport { run as launch } from \"./worker\";\nimport UnusedComp from \"./UnusedComp.svelte\";\ntype CompType = typeof UnusedComp;\nfunction preview(UnusedComp: unknown) { return UnusedComp; }\nstart();\nlaunch();\npreview(null);\n",
    ).unwrap();

    let ignores = KagamiConfig::default().build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();
    let dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
        },
    );
    let has = |file: &str, sym: &str| {
        dead.symbols
            .iter()
            .any(|s| s.file == file && s.symbol == sym)
    };
    assert!(
        !has("src/worker.ts", "initialize"),
        "renamed default import start() must keep initialize alive"
    );
    assert!(
        has("src/UnusedComp.svelte", "UnusedComp"),
        "typeof and shadowed param must not keep UnusedComp alive"
    );
    assert!(
        has("src/worker/index.ts", "run"),
        "import from ./worker must not falsely link worker/index.ts when worker.ts exists"
    );

    // Computed object key rename equivalence + spread vs non-spread distinction
    let c1 = "function f1(k: string, v: number) { return { [k]: v }; }\n";
    let c2 = "function f2(key: string, val: number) { return { [key]: val }; }\n";
    let ec1 = lang::extract_file("c1.ts", Language::TypeScript, c1, "c1".into()).unwrap();
    let ec2 = lang::extract_file("c2.ts", Language::TypeScript, c2, "c2".into()).unwrap();
    assert!(pair_similarity(&ec1.ir_funcs[0], &ec2.ir_funcs[0], false) >= 0.99);

    let s1 = "function g1(xs: number[]) { return send(...xs); }\n";
    let s2 = "function g2(xs: number[]) { return send(xs); }\n";
    let es1 = lang::extract_file("s1.ts", Language::TypeScript, s1, "s1".into()).unwrap();
    let es2 = lang::extract_file("s2.ts", Language::TypeScript, s2, "s2".into()).unwrap();
    assert!(pair_similarity(&es1.ir_funcs[0], &es2.ir_funcs[0], false) < 1.0);
}

#[test]
fn test_review_fixes_js_specifier_default_list_and_external_default() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();

    fs::write(root.join("src/esm.ts"), "export function esmRun() {}\n").unwrap();
    fs::write(
        root.join("src/list_def.ts"),
        "function listRun() {}\nexport { listRun as default };\n",
    )
    .unwrap();
    fs::write(
        root.join("src/wrapped.ts"),
        "function wrappedRun() {}\nexport default (wrappedRun);\n",
    )
    .unwrap();
    fs::write(
        root.join("src/unused_def.ts"),
        "export default function unusedLegacy() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/main.ts"),
        "import extPkg from \"external-pkg\";\nimport { esmRun } from \"./esm.js\";\nimport listFn from \"./list_def\";\nimport wrappedFn from \"./wrapped\";\nextPkg();\nesmRun();\nlistFn();\nwrappedFn();\n",
    ).unwrap();

    let ignores = KagamiConfig::default().build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();
    let dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
        },
    );
    let has = |file: &str, sym: &str| {
        dead.symbols
            .iter()
            .any(|s| s.file == file && s.symbol == sym)
    };
    assert!(
        !has("src/esm.ts", "esmRun"),
        "./esm.js specifier must resolve to src/esm.ts"
    );
    assert!(
        !has("src/list_def.ts", "listRun"),
        "export {{ listRun as default }} must keep listRun alive"
    );
    assert!(
        !has("src/wrapped.ts", "wrappedRun"),
        "export default (wrappedRun) must keep wrappedRun alive"
    );
    assert!(
        has("src/unused_def.ts", "unusedLegacy"),
        "external default import must not keep unrelated local default export alive"
    );
}

#[test]
fn test_go_function_values_not_flagged_dead() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("cmd/app")).unwrap();
    fs::create_dir_all(root.join("internal/execscope")).unwrap();

    fs::write(
        root.join("internal/execscope/handlers.go"),
        r#"package execscope

type Painter struct {
	ActionLink func(string) string
}

func commandRunLink(s string) string { return s }
func readInterpreterFile(p string) string { return p }
func gnuSedArguments(a string) string { return a }
func bsdSedArguments(a string) string { return a }
func varDefaultHandler(a string) string { return a }
func retHandler(a string) string { return a }
func sliceHandlerA(a string) string { return a }
func sliceHandlerB(a string) string { return a }
func neverUsedFn(a string) string { return a }
func shadowedDeadFn(a string) string { return a }

var defaultHandler = varDefaultHandler

func pickReturned() func(string) string {
	return retHandler
}

func Run(useBSD bool) string {
	var p Painter
	p.ActionLink = commandRunLink
	read := readInterpreterFile
	parse := gnuSedArguments
	if useBSD {
		parse = bsdSedArguments
	}
	fns := []func(string) string{sliceHandlerA, sliceHandlerB}
	shadowedDeadFn := "local string, not the function"
	_ = shadowedDeadFn
	r := pickReturned()
	return p.ActionLink(read(parse(defaultHandler(r(fns[0]("x"))))))
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("cmd/app/main.go"),
        r#"package main

import "demo/internal/execscope"

func main() {
	_ = execscope.Run(true)
}
"#,
    )
    .unwrap();

    let ignores = KagamiConfig::default().build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();
    let dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
        },
    );
    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);

    for live_fn in [
        "commandRunLink",
        "readInterpreterFile",
        "gnuSedArguments",
        "bsdSedArguments",
        "varDefaultHandler",
        "retHandler",
        "sliceHandlerA",
        "sliceHandlerB",
    ] {
        assert!(
            !is_dead(live_fn),
            "Go function value {live_fn} must not be flagged dead; dead={:?}",
            dead.symbols
        );
    }
    assert!(
        is_dead("neverUsedFn"),
        "truly unused Go function must still be flagged dead"
    );
    assert!(
        is_dead("shadowedDeadFn"),
        "shadowed local variable must not keep dead function with same name alive"
    );
}

#[test]
fn test_go_interface_and_embedded_overrides_reachability() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("cmd/app")).unwrap();
    fs::create_dir_all(root.join("internal/srv")).unwrap();

    fs::write(
        root.join("internal/srv/srv.go"),
        r#"package srv

import (
	"net/http"
	"sync"
	"extparser/parser"
)

type Connector struct{}

func (c *Connector) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	c.resource()
	refuse(w)
}

func (c *Connector) resource() {}
func refuse(w http.ResponseWriter) {}

type InputError struct{ msg string }
func (e *InputError) Error() string { return formatErr(e.msg) }
func (e *InputError) Unwrap() error { return nil }
func formatErr(s string) string { return s }

type DeadError struct{}
func (e *DeadError) Error() string { return deadErrHelper() }
func deadErrHelper() string { return "" }

type copyQuoteParser struct {
	parser.BlockParser
}

func (p *copyQuoteParser) Continue() int { return quoteHelper() }
func quoteHelper() int { return 1 }

type LockedCache struct {
	sync.Mutex
}

func (c *LockedCache) UnusedCacheMethod() {}

type Source interface {
	ActivityFacts() int
}

type UnusedIface interface {
	Ghost() int
}

type Entry struct{}

func (e Entry) ActivityFacts() int { return entryHelper() }
func (e Entry) Ghost() int { return 0 }
func entryHelper() int { return 42 }

type NativeAnswers struct{}

func (a NativeAnswers) PublicAnswer() string { return "unused" }

type SortItems []int
func (s SortItems) Len() int { return len(s) }
func (s SortItems) Less(i, j int) bool { return s[i] < s[j] }
func (s SortItems) Swap(i, j int) { s[i], s[j] = s[j], s[i] }

func Render(s Source) int { return s.ActivityFacts() }

func Start() {
	var _ http.Handler = &Connector{}
	_ = &InputError{msg: "bad"}
	_ = &copyQuoteParser{}
	_ = &LockedCache{}
	_ = NativeAnswers{}
	_ = SortItems{1, 2}
	_ = Render(Entry{})
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("cmd/app/main.go"),
        r#"package main

import "demo/internal/srv"

func main() {
	srv.Start()
}
"#,
    )
    .unwrap();

    let ignores = KagamiConfig::default().build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();
    let dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
        },
    );
    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);

    for live_sym in [
        "Connector.ServeHTTP",
        "Connector.resource",
        "refuse",
        "InputError.Error",
        "InputError.Unwrap",
        "formatErr",
        "copyQuoteParser.Continue",
        "quoteHelper",
        "Entry.ActivityFacts",
        "entryHelper",
        "SortItems.Len",
        "SortItems.Less",
        "SortItems.Swap",
    ] {
        assert!(
            !is_dead(live_sym),
            "Expected {live_sym} to be live via interface/embedded reachability; dead={:?}",
            dead.symbols
        );
    }

    for dead_sym in [
        "DeadError.Error",
        "deadErrHelper",
        "LockedCache.UnusedCacheMethod",
        "Entry.Ghost",
        "NativeAnswers.PublicAnswer",
    ] {
        assert!(
            is_dead(dead_sym),
            "Expected {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}

#[test]
fn test_go_review_edge_cases_scoping_generics_promoted_and_grouped_vars() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("cmd/app")).unwrap();
    fs::create_dir_all(root.join("internal/edge")).unwrap();

    fs::write(
        root.join("internal/edge/edge.go"),
        r#"package edge

import (
	locks "sync"
)

func v() {}
func callback() {}
func selfShadowed() {}
func groupedCb() {}

var (
	groupedHandler = groupedCb
)

type Box[T any] struct{ Val T }
func (b Box[T]) MarshalJSON() ([]byte, error) { return boxHelper(), nil }
func boxHelper() []byte { return nil }

type AliasedLock struct {
	locks.Mutex
}
func (a *AliasedLock) DeadAfterAliasedMutex() {}

type Service interface {
	Start()
	Stop()
}

type Base struct{}
func (b *Base) Start() { baseStartHelper() }
func baseStartHelper() {}

type Worker struct {
	Base
}
func (w *Worker) Stop() {}

type ProdPartial struct{}
func (p *ProdPartial) PartialA() {}

type PartialIface interface {
	PartialA()
	PartialB()
}

func useService(s Service) { _ = s }
func usePartial(p PartialIface) { _ = p }

func Run(items []int) {
	callbacks := map[string]func(){"run": v}
	callbacks["run"]()
	for callback := range items {
		_ = callback
	}
	fn := callback
	fn()
	selfShadowed := selfShadowed
	selfShadowed()
	groupedHandler()
	_ = Box[int]{Val: 1}
	_ = &AliasedLock{}
	useService(&Worker{})
	usePartial(nil)
	_ = &ProdPartial{}
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("internal/edge/edge_test.go"),
        r#"package edge

func (p *ProdPartial) PartialB() {}
"#,
    )
    .unwrap();

    fs::write(
        root.join("cmd/app/main.go"),
        r#"package main

import "demo/internal/edge"

func main() {
	edge.Run([]int{1})
}
"#,
    )
    .unwrap();

    let ignores = KagamiConfig::default().build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();
    let dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
        },
    );
    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);

    for live_sym in [
        "v",
        "callback",
        "selfShadowed",
        "groupedCb",
        "Box.MarshalJSON",
        "boxHelper",
        "Base.Start",
        "baseStartHelper",
        "Worker.Stop",
    ] {
        assert!(
            !is_dead(live_sym),
            "Expected {live_sym} to be live; dead={:?}",
            dead.symbols
        );
    }
    for dead_sym in ["AliasedLock.DeadAfterAliasedMutex", "ProdPartial.PartialA"] {
        assert!(
            is_dead(dead_sym),
            "Expected {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}

#[test]
fn test_go_chained_struct_embedded_and_cross_pkg_field_method_calls() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("internal/ui")).unwrap();
    fs::create_dir_all(root.join("internal/launch")).unwrap();
    fs::create_dir_all(root.join("cmd/app")).unwrap();

    fs::write(root.join("go.mod"), "module demo\n\ngo 1.24\n").unwrap();

    // Structs with overloaded method names (Update, Key, Render, Mouse)
    // across single-hop, multi-hop, embedded field, slice/map, and return-value chains.
    fs::write(
        root.join("internal/ui/components.go"),
        r#"package ui

type BTWDock struct{}
func (b *BTWDock) Update() { btwUpdateHelper() }
func (b *BTWDock) Key()    { btwKeyHelper() }
func (b *BTWDock) Render() { btwRenderHelper() }
func btwUpdateHelper() {}
func btwKeyHelper()    {}
func btwRenderHelper() {}

type Menu struct{}
func (m *Menu) Update() { menuUpdateHelper() }
func menuUpdateHelper() {}

type Dialog struct{}
func (d *Dialog) Key()   { dialogKeyHelper() }
func (d *Dialog) Mouse() { dialogMouseHelper() }
func dialogKeyHelper()   {}
func dialogMouseHelper() {}

type ContentDialog struct {
	Dialog
}
func (d *ContentDialog) Key()   { d.Dialog.Key() }
func (d *ContentDialog) Mouse() { d.Dialog.Mouse() }

type Item struct{}
func (i *Item) RenderIndexed() { itemIndexedHelper() }
func (i *Item) RenderRanged()  { itemRangedHelper() }
func itemIndexedHelper() {}
func itemRangedHelper()  {}

type Transcript struct{}
func (t *Transcript) Render() { transcriptRenderHelper() }
func transcriptRenderHelper() {}

type Main struct {
	BTW   *BTWDock
	Menu  *Menu
	Items []*Item
}
func (m *Main) Update() {
	m.Menu.Update()
	m.BTW.Update()
}
func (m *Main) Render() {
	m.BTW.Render()
	m.Items[0].RenderIndexed()
	for _, it := range m.Items {
		it.RenderRanged()
	}
}

type Attachment struct {
	Main *Main
}

type FrameInput struct {
	Main   *Main
	Dialog *ContentDialog
}
func (in FrameInput) displayedTranscript() *Transcript {
	return &Transcript{}
}
func (in FrameInput) Handle(anyVal any) {
	// Single-hop field calls
	in.Dialog.Key()
	in.Dialog.Mouse()
	// Local var initialized from struct field, then chained field call
	main, dlg := in.Main, in.Dialog
	main.BTW.Key()
	_ = dlg
	// Direct field call on receiver
	in.Main.Render()
	// Method-return chain
	in.displayedTranscript().Render()
	// Type assertion chain
	if cd, ok := anyVal.(*ContentDialog); ok {
		cd.Dialog.Key()
	}
}

func NewAttachment() *Attachment {
	return &Attachment{Main: &Main{BTW: &BTWDock{}, Menu: &Menu{}}}
}

// Truly unused structs/methods (including overloaded names) MUST remain dead.
type UnusedDock struct{}
func (u *UnusedDock) Update() {}
func (u *UnusedDock) Key()    {}
func (u *UnusedDock) Render() {}

type ComposerOperation struct{}
func (c *ComposerOperation) ClientLocal() {}

type NativeAnswers struct{}
func (n *NativeAnswers) PublicAnswer() {}
"#,
    )
    .unwrap();

    // Cross-package struct field chain: r.terminal (type *ui.Attachment) -> u.Main.Update()
    fs::write(
        root.join("internal/launch/renderer.go"),
        r#"package launch

import "demo/internal/ui"

type Renderer struct {
	terminal *ui.Attachment
}

func (r *Renderer) Step() {
	u := r.terminal
	u.Main.Update()
	// Function-return chain across packages
	ui.NewAttachment().Main.Render()
	in := ui.FrameInput{Main: u.Main, Dialog: &ui.ContentDialog{}}
	in.Handle(nil)
}

func Run() {
	r := &Renderer{terminal: ui.NewAttachment()}
	r.Step()
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("cmd/app/main.go"),
        r#"package main

import "demo/internal/launch"

func main() {
	launch.Run()
}
"#,
    )
    .unwrap();

    let ignores = KagamiConfig::default().build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();
    let dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
        },
    );
    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);
    let has_resolved_call = |from_qname: &str, to_qname: &str| {
        graph.edges.iter().any(|e| {
            e.kind == kagami::model::EdgeKind::Calls
                && e.confidence == Confidence::Resolved
                && graph
                    .symbols
                    .get(&e.from)
                    .map(|s| s.qualified_name.as_str())
                    == Some(from_qname)
                && graph.symbols.get(&e.to).map(|s| s.qualified_name.as_str()) == Some(to_qname)
        })
    };

    for (caller, callee) in [
        ("Main.Update", "BTWDock.Update"),
        ("Main.Update", "Menu.Update"),
        ("Main.Render", "BTWDock.Render"),
        ("Main.Render", "Item.RenderIndexed"),
        ("Main.Render", "Item.RenderRanged"),
        ("FrameInput.Handle", "ContentDialog.Key"),
        ("FrameInput.Handle", "ContentDialog.Mouse"),
        ("FrameInput.Handle", "BTWDock.Key"),
        ("FrameInput.Handle", "Main.Render"),
        ("FrameInput.Handle", "Transcript.Render"),
        ("ContentDialog.Key", "Dialog.Key"),
        ("ContentDialog.Mouse", "Dialog.Mouse"),
        ("Renderer.Step", "Main.Update"),
        ("Renderer.Step", "Main.Render"),
    ] {
        assert!(
            has_resolved_call(caller, callee),
            "Expected Resolved call edge {caller} -> {callee}"
        );
    }

    for live_sym in [
        "BTWDock.Update",
        "BTWDock.Key",
        "BTWDock.Render",
        "btwUpdateHelper",
        "btwKeyHelper",
        "btwRenderHelper",
        "Menu.Update",
        "menuUpdateHelper",
        "ContentDialog.Key",
        "ContentDialog.Mouse",
        "Dialog.Key",
        "Dialog.Mouse",
        "dialogKeyHelper",
        "dialogMouseHelper",
        "Main.Update",
        "Main.Render",
        "Item.RenderIndexed",
        "Item.RenderRanged",
        "itemIndexedHelper",
        "itemRangedHelper",
        "Transcript.Render",
        "transcriptRenderHelper",
    ] {
        assert!(
            !is_dead(live_sym),
            "Expected {live_sym} to be resolved and live; dead={:?}",
            dead.symbols
        );
    }

    for dead_sym in [
        "UnusedDock.Update",
        "UnusedDock.Key",
        "UnusedDock.Render",
        "ComposerOperation.ClientLocal",
        "NativeAnswers.PublicAnswer",
    ] {
        assert!(
            is_dead(dead_sym),
            "Expected truly unused {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}

#[test]
fn test_go_receiver_chain_review_edge_cases() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("internal/cases")).unwrap();
    fs::create_dir_all(root.join("cmd/app")).unwrap();

    fs::write(root.join("go.mod"), "module demo\n\ngo 1.24\n").unwrap();

    fs::write(
        root.join("internal/cases/cases.go"),
        r#"package cases

import "time"

// 1. var _, worker = pair() must preserve result index 1 for worker
type First struct{}
func (f *First) Run() {}

type Second struct{}
func (s *Second) Run() {}

func pair() (*First, *Second) { return &First{}, &Second{} }

// 2. Shadowed factory parameter must not resolve to package build()
func build() *OldTarget { return &OldTarget{} }

func useShadowedFactory(build func() interface{ Exec() }) {
	build().Exec()
}

// 3. Shadowing local with unsupported type must mask outer receiver type
type OuterCfg struct {
	Worker *OuterWorker
}

func useShadowedCfg(cfg *OuterCfg) {
	_ = cfg
	fn := func(cfg struct{ Worker interface{ Work() } }) {
		cfg.Worker.Work()
	}
	fn(struct{ Worker interface{ Work() } }{})
}

// 4. Explicit anonymous interface var must not lock to initializer's concrete type
func useAnonIface() {
	var runner interface{ Ping() } = ImplA{}
	runner = ImplB{}
	runner.Ping()
}

// 5. Blank field _ time.Time must not act as an external embed
type BlankHolder struct {
	_ time.Time
}
func (b *BlankHolder) UnusedReload() {}

// 6. Callable field shadowing promoted method must block Base.Make
type BaseFactory struct{}
func (b *BaseFactory) Make() *First { return &First{} }

type WrapperFactory struct {
	BaseFactory
	Make func() *Second
}

// 7. Range assignment (=) and same-block reused local (:=) preserve existing receiver types
type Job struct{}
func (j *Job) Process() {}

func useRangeAndChan(jobs chan *Job) {
	var job *Job
	for job = range jobs {
		job.Process()
	}
	job, ok := <-jobs
	if ok {
		job.Process()
	}
}

func RunAll() {
	var _, worker = pair()
	worker.Run()
	useShadowedFactory(func() interface{ Exec() } { return nil })
	useShadowedCfg(&OuterCfg{})
	useAnonIface()
	_ = &BlankHolder{}
	w := &WrapperFactory{}
	_ = w.Make()
	useRangeAndChan(nil)
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("internal/cases/impls.go"),
        r#"package cases

type ImplA struct{}
func (a ImplA) Ping() {}

type ImplB struct{}
func (b ImplB) Ping() {}

type OldTarget struct{}
func (o *OldTarget) Exec() {}

type NewTarget struct{}
func (n *NewTarget) Exec() {}

type OuterWorker struct{}
func (w *OuterWorker) Work() {}

type InnerWorker struct{}
func (w *InnerWorker) Work() {}
"#,
    )
    .unwrap();

    fs::write(
        root.join("internal/cases/cases_test.go"),
        r#"package cases

func (s *Second) TestHelperMethod() {}

func TestCallsHelper() {
	s := &Second{}
	s.TestHelperMethod()
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("cmd/app/main.go"),
        r#"package main

import "demo/internal/cases"

func main() {
	cases.RunAll()
}
"#,
    )
    .unwrap();

    let ignores = KagamiConfig::default().build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();
    let dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
        },
    );
    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);
    let has_resolved_call = |from_qname: &str, to_qname: &str| {
        graph.edges.iter().any(|e| {
            e.kind == kagami::model::EdgeKind::Calls
                && e.confidence == Confidence::Resolved
                && graph
                    .symbols
                    .get(&e.from)
                    .map(|s| s.qualified_name.as_str())
                    == Some(from_qname)
                && graph.symbols.get(&e.to).map(|s| s.qualified_name.as_str()) == Some(to_qname)
        })
    };

    assert!(has_resolved_call("RunAll", "Second.Run"));
    assert!(!has_resolved_call("RunAll", "First.Run"));
    assert!(!has_resolved_call("RunAll", "BaseFactory.Make"));
    assert!(!has_resolved_call("useShadowedFactory", "OldTarget.Exec"));
    assert!(!has_resolved_call("useShadowedCfg", "OuterWorker.Work"));
    assert!(!has_resolved_call("useAnonIface", "ImplA.Ping"));
    assert!(has_resolved_call("useRangeAndChan", "Job.Process"));
    assert!(has_resolved_call(
        "TestCallsHelper",
        "Second.TestHelperMethod"
    ));

    for dead_sym in ["First.Run", "BlankHolder.UnusedReload", "BaseFactory.Make"] {
        assert!(
            is_dead(dead_sym),
            "Expected {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}

fn analyze_rust_dead(root: &std::path::Path) -> kagami::graph::dead::DeadReport {
    let cfg = KagamiConfig::default();
    let ignore = cfg.build_ignore_globset().unwrap();
    let files = kagami::walker::discover_files(root, &ignore, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();
    analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: Some(std::collections::HashSet::from([Language::Rust])),
            allowlist: std::collections::HashSet::new(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
        },
    )
}

#[test]
fn test_rust_function_value_refs_and_shadowing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let src_dir = root.join("crate-a/src");
    fs::create_dir_all(&src_dir).unwrap();

    fs::write(
        src_dir.join("main.rs"),
        r#"
fn internal_error(e: String) -> String { e }
fn is_retryable(x: &i32) -> bool { *x > 0 }
fn serialize_enum(v: i32) -> Option<String> { Some(v.to_string()) }
fn valid_row(r: &i32) -> bool { *r != 0 }
fn helper_val(x: i32) -> i32 { x + 1 }
fn arr_cb() {}
fn tup_cb() {}
fn field_cb() {}
fn parse_item<T>(x: T) -> T { x }
fn default_port() -> u16 { 8080 }
fn init_global() -> i32 { 42 }
fn inner_shadow() {}

fn shadowed_fn() {}
fn never_used() {}
fn only_used_in_test() {}
fn only_used_in_tokio_test() {}
fn only_used_in_cfg_test_mod() {}

#[derive(serde::Deserialize)]
struct Config {
    #[serde(default = "default_port")]
    port: u16,
}

struct Handler {
    cb: fn(),
}

static GLOBAL: LazyLock<i32> = LazyLock::new(init_global);

fn use_cb(f: fn()) { f(); }

fn main() {
    let _ = Err::<i32, String>("e".into()).map_err(internal_error);
    let nums = vec![1, 2, 3];
    let _ = nums.iter().any(is_retryable);
    let _ = Some(1).and_then(serialize_enum);
    let mut rows = vec![1, 0, 2];
    rows.retain(valid_row);
    let _ = rows.into_iter().filter(valid_row).map(parse_item::<i32>).count();
    let f = helper_val;
    let _ = f(1);
    let _arr: [fn(); 1] = [arr_cb];
    let _tup = (tup_cb,);
    let _h = Handler { cb: field_cb };
    let _c = Config { port: 0 };
    let _g = *GLOBAL;

    let shadowed_fn = 123;
    let _ = shadowed_fn;

    {
        let inner_shadow = 99;
        let _ = inner_shadow;
    }
    use_cb(inner_shadow);
}

#[test]
fn unit_test_only() {
    let _ = Some(1).map(|_| only_used_in_test);
}

#[tokio::test(flavor = "multi_thread")]
async fn async_test_only() {
    only_used_in_tokio_test();
}

#[cfg(test)]
mod tests {
    use super::*;
    fn helper() {
        only_used_in_cfg_test_mod();
    }
}
"#,
    )
    .unwrap();

    let dead = analyze_rust_dead(root);

    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);
    for live_sym in [
        "internal_error",
        "is_retryable",
        "serialize_enum",
        "valid_row",
        "helper_val",
        "arr_cb",
        "tup_cb",
        "field_cb",
        "parse_item",
        "default_port",
        "init_global",
        "inner_shadow",
    ] {
        assert!(
            !is_dead(live_sym),
            "Expected {live_sym} to be live; dead={:?}",
            dead.symbols
        );
    }
    for dead_sym in [
        "shadowed_fn",
        "never_used",
        "only_used_in_test",
        "only_used_in_tokio_test",
        "only_used_in_cfg_test_mod",
    ] {
        assert!(
            is_dead(dead_sym),
            "Expected {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}

#[test]
fn test_rust_macro_invocations_and_macro_rules_token_trees() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let src_dir = root.join("crate-m/src");
    fs::create_dir_all(&src_dir).unwrap();

    fs::write(
        src_dir.join("main.rs"),
        r#"
struct Inner;
impl Inner {
    fn poll_step(&self) {}
}

struct Stats {
    inner: Inner,
}
impl Stats {
    fn new() -> Self { Self { inner: Inner } }
    fn saved_percent(&self) -> f64 { 50.0 }
    fn report(&self) {
        tracing::debug!(saved = self.saved_percent(), "stats");
        tokio::select! {
            _ = self.inner.poll_step() => handle_select(cb_in_macro),
        }
    }
}

fn handle_select(cb: fn()) { cb(); }
fn cb_in_macro() {}
fn dead_in_test_macro() {}
fn dead_macro_target() {}

struct RemoteBoard;
impl RemoteBoard {
    fn new() -> Self { Self }
    fn call(&self) {}
}

macro_rules! operation {
    ($m:ident) => {
        fn $m(&self) {
            self.call();
        }
    };
}

trait Board {
    fn create(&self);
}

impl Board for RemoteBoard {
    operation!(create);
}

macro_rules! test_only_macro {
    () => {
        dead_macro_target();
    };
}

fn main() {
    let s = Stats::new();
    s.report();
    let _b = RemoteBoard::new();
}

#[test]
fn macro_test_only() {
    tracing::info!(val = dead_in_test_macro());
    test_only_macro!();
}
"#,
    )
    .unwrap();

    let dead = analyze_rust_dead(root);

    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);
    for live_sym in [
        "Stats::saved_percent",
        "Inner::poll_step",
        "handle_select",
        "cb_in_macro",
        "RemoteBoard::call",
    ] {
        assert!(
            !is_dead(live_sym),
            "Expected {live_sym} to be live; dead={:?}",
            dead.symbols
        );
    }
    for dead_sym in ["dead_in_test_macro", "dead_macro_target"] {
        assert!(
            is_dead(dead_sym),
            "Expected {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}

#[test]
fn test_rust_cfg_sibling_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let src_dir = root.join("crate-cfg/src");
    fs::create_dir_all(&src_dir).unwrap();

    fs::write(
        src_dir.join("main.rs"),
        r#"
struct Daemon;
impl Daemon {
    fn new() -> Self { Self }

    #[cfg(any(unix, windows))]
    fn managed_version(&self) -> Option<String> { Some("1.0".into()) }

    #[cfg(not(any(unix, windows)))]
    fn managed_version(&self) -> Option<String> { None }
}

#[cfg(unix)]
fn platform_open() {}

#[cfg(windows)]
fn platform_open() {}

#[cfg(unix)]
fn unused_cfg() {}

#[cfg(not(unix))]
fn unused_cfg() {}

#[cfg(unix)]
fn test_only_cfg() {}

#[cfg(not(unix))]
fn test_only_cfg() {}

fn main() {
    let d = Daemon::new();
    let _ = d.managed_version();
    platform_open();
}

#[test]
fn call_cfg_in_test() {
    test_only_cfg();
}
"#,
    )
    .unwrap();

    let dead = analyze_rust_dead(root);

    let dead_count = |sym: &str| dead.symbols.iter().filter(|s| s.symbol == sym).count();
    assert_eq!(
        dead_count("Daemon::managed_version"),
        0,
        "dead={:?}",
        dead.symbols
    );
    assert_eq!(dead_count("platform_open"), 0, "dead={:?}", dead.symbols);
    assert_eq!(
        dead_count("unused_cfg"),
        2,
        "both unused cfg siblings should be dead"
    );
    assert_eq!(
        dead_count("test_only_cfg"),
        2,
        "both test-only cfg siblings should be dead"
    );
}

#[test]
fn test_rust_receiver_chains_traits_and_smart_pointers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let src_dir = root.join("crate-r/src");
    fs::create_dir_all(&src_dir).unwrap();

    fs::write(
        src_dir.join("auth.rs"),
        r#"
pub fn connect() {}
pub fn unused_auth() {}
"#,
    )
    .unwrap();

    fs::write(
        src_dir.join("widget_ext.rs"),
        r#"
impl super::SplitWidget {
    pub fn part_b(&self) {
        self.part_a();
    }
}
"#,
    )
    .unwrap();

    fs::write(
        src_dir.join("main.rs"),
        r#"
mod auth;
mod widget_ext;

struct UnloadingState;
impl UnloadingState {
    fn new() -> Self { Self }
    fn should_unload_now(&mut self) -> bool { true }
}

struct ProcessExecManager;
impl ProcessExecManager {
    fn connection_closed(&self) {}
}

struct ProcessExecProcessor {
    mgr: Arc<ProcessExecManager>,
}
impl ProcessExecProcessor {
    fn connection_closed(&self) {
        self.mgr.connection_closed();
    }
}

struct MessageProcessor {
    exec: Arc<ProcessExecProcessor>,
}
impl MessageProcessor {
    fn new() -> Self { Self { exec: Arc::new(ProcessExecProcessor { mgr: Arc::new(ProcessExecManager) }) } }
    fn connection_closed(&self) {
        self.exec.connection_closed();
    }
}

struct UnrelatedProcessor;
impl UnrelatedProcessor {
    fn connection_closed(&self) {}
}

struct LiveStruct;
impl LiveStruct {
    fn new() -> Self { Self }
    fn helper() {}
}
impl Drop for LiveStruct {
    fn drop(&mut self) {
        Self::helper();
    }
}
impl std::fmt::Display for LiveStruct {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }
}

struct DeadStruct;
impl Drop for DeadStruct {
    fn drop(&mut self) {}
}

trait LiveTrait {
    fn execute(&self);
}
trait DeadTrait {
    fn never_called(&self);
}
impl LiveTrait for LiveStruct {
    fn execute(&self) {}
}
impl LiveTrait for DeadStruct {
    fn execute(&self) {}
}
impl DeadTrait for LiveStruct {
    fn never_called(&self) {}
}

struct InnerTarget;
impl InnerTarget {
    fn deref_method(&self) {}
}
struct Wrapper(InnerTarget);
impl Wrapper {
    fn new() -> Self { Self(InnerTarget) }
}
impl std::ops::Deref for Wrapper {
    type Target = InnerTarget;
    fn deref(&self) -> &Self::Target { &self.0 }
}

pub struct SplitWidget;
impl SplitWidget {
    fn new() -> Self { Self }
    pub fn part_a(&self) {}
}

fn use_trait(t: &dyn LiveTrait) {
    t.execute();
}

async fn run() {
    let state = Arc::new(Mutex::new(UnloadingState::new()));
    let mut guard = state.lock().await;
    let _ = guard.should_unload_now();

    let processor = Arc::new(MessageProcessor::new());
    processor.connection_closed();

    let live = LiveStruct::new();
    use_trait(&live);

    let w = Wrapper::new();
    w.deref_method();

    let sw = SplitWidget::new();
    sw.part_b();

    auth::connect();
}

fn main() {
    let _ = run();
}
"#,
    )
    .unwrap();

    let dead = analyze_rust_dead(root);

    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);
    for live_sym in [
        "UnloadingState::should_unload_now",
        "MessageProcessor::connection_closed",
        "ProcessExecProcessor::connection_closed",
        "ProcessExecManager::connection_closed",
        "LiveStruct::drop",
        "LiveStruct::helper",
        "LiveStruct::fmt",
        "LiveStruct::execute",
        "InnerTarget::deref_method",
        "SplitWidget::part_a",
        "SplitWidget::part_b",
        "connect",
    ] {
        assert!(
            !is_dead(live_sym),
            "Expected {live_sym} to be live; dead={:?}",
            dead.symbols
        );
    }
    for dead_sym in [
        "UnrelatedProcessor::connection_closed",
        "DeadStruct::drop",
        "DeadStruct::execute",
        "LiveStruct::never_called",
        "unused_auth",
    ] {
        assert!(
            is_dead(dead_sym),
            "Expected {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}

#[test]
fn test_rust_review_edge_cases() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let src_dir = root.join("src");
    let tests_dir = root.join("tests");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&tests_dir).unwrap();

    fs::write(
        src_dir.join("remote.rs"),
        r#"
pub struct Worker;
impl Worker {
    pub fn remote_run(&self) {}
}
"#,
    )
    .unwrap();

    fs::write(
        tests_dir.join("smoke.rs"),
        r#"
fn smoke() {
    root_test_only_fn();
}
"#,
    )
    .unwrap();

    fs::write(
        src_dir.join("main.rs"),
        r#"
mod remote;

fn accepts(_x: i32) -> bool { true }
fn arm_shadowed() {}
fn mut_shadowed() {}
fn root_test_only_fn() {}
fn cli_feature_fn() {}
fn test_named_feature_fn() {}
const fn buf_capacity() -> usize { 8 }
fn rep_helper() {}

struct Registry<T>(std::marker::PhantomData<T>);
impl<T> Registry<T> {
    fn install() {}
}

type Buffer = [u8; buf_capacity()];

struct Worker;
impl Worker {
    fn new() -> Self { Self }
    fn run(&self) {}
}

trait UnusedTask {
    fn run(&self);
}
impl UnusedTask for Worker {
    fn run(&self) {}
}

struct Job;
impl Job {
    fn exec(&self) {}
}
struct Loader;
impl Loader {
    fn new() -> Self { Self }
    fn load(&self) -> Job { Job }
}

trait Display {}
struct Status;
impl Status {
    fn new() -> Self { Self }
}
impl std::fmt::Display for Status {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }
}

macro_rules! call_many {
    ($($x:expr),*) => {
        $(
            let _ = $x;
            rep_helper();
        )*
    };
}

#[cfg(any(test, feature = "cli"))]
fn run_cli_entry() {
    cli_feature_fn();
}

#[cfg(feature = "test")]
fn run_test_feature_entry() {
    test_named_feature_fn();
}

#[cfg(test,)]
fn trailing_comma_test_only() {}

mod mod_a {
    #[cfg(unix)]
    pub fn scoped_cfg() {}
}
mod mod_b {
    #[cfg(unix)]
    pub fn scoped_cfg() {}
}

fn start_remote(w: remote::Worker) {
    w.remote_run();
}

fn main() {
    match 1 {
        arm_shadowed if accepts(arm_shadowed) => {
            let _ = arm_shadowed;
        }
        _ => {}
    }
    let (mut mut_shadowed,) = (1,);
    let _ = mut_shadowed;
    Registry::<i32>::install();
    let _b: Buffer = [0; 8];
    let w = Worker::new();
    w.run();
    let loader = Loader::new();
    loader.load().exec();
    let _s = Status::new();
    call_many!(1, 2);
    run_cli_entry();
    run_test_feature_entry();
    mod_a::scoped_cfg();
    start_remote(remote::Worker);
}
"#,
    )
    .unwrap();

    let dead = analyze_rust_dead(root);
    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);

    for live_sym in [
        "accepts",
        "Registry::install",
        "buf_capacity",
        "Job::exec",
        "Status::fmt",
        "rep_helper",
        "cli_feature_fn",
        "test_named_feature_fn",
        "Worker::remote_run",
        "trailing_comma_test_only",
    ] {
        assert!(
            !is_dead(live_sym),
            "Expected {live_sym} to be live; dead={:?}",
            dead.symbols
        );
    }
    for dead_sym in ["arm_shadowed", "mut_shadowed", "root_test_only_fn"] {
        assert!(
            is_dead(dead_sym),
            "Expected {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
    // Inherent Worker::run is live, while UnusedTask::run on Worker must remain dead.
    let dead_worker_runs: Vec<_> = dead
        .symbols
        .iter()
        .filter(|s| s.symbol == "Worker::run")
        .collect();
    assert_eq!(
        dead_worker_runs.len(),
        1,
        "Only trait impl Worker::run should be dead; got {:?}",
        dead_worker_runs
    );
    // mod_a::scoped_cfg is live; mod_b::scoped_cfg must remain dead (not grouped across inline mods).
    let dead_scoped_cfgs: Vec<_> = dead
        .symbols
        .iter()
        .filter(|s| s.symbol == "scoped_cfg")
        .collect();
    assert_eq!(
        dead_scoped_cfgs.len(),
        1,
        "Only mod_b::scoped_cfg should be dead; got {:?}",
        dead_scoped_cfgs
    );
}

#[test]
fn test_rust_bare_call_method_shadowing_and_aliased_use() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        r#"
struct PidBackend;

impl PidBackend {
    fn terminate_process(&self, pid: u32) {
        terminate_process(pid);
    }

    fn dead_method(&self) {}
}

#[cfg(unix)]
fn terminate_process(_pid: u32) {}

#[cfg(not(any(unix, windows)))]
fn terminate_process(_pid: u32) {}

#[cfg(windows)]
fn force_terminate_process(_pid: u32) {}

#[cfg(windows)]
use force_terminate_process as terminate_process;

struct ConfigManager;

impl ConfigManager {
    fn apply_runtime(&self) {
        apply_runtime();
    }
}

fn apply_runtime() {}
fn never_called() {}
fn prod_test_only_fn() {}
fn prod_test_only_aliased() {}

#[cfg(test)]
mod tests {
    use super::prod_test_only_aliased as test_alias;
    use super::prod_test_only_fn;

    #[test]
    fn exercises_prod_only_in_tests() {
        prod_test_only_fn();
        test_alias();
    }
}

fn main() {
    let b = PidBackend;
    b.terminate_process(42);
    let c = ConfigManager;
    c.apply_runtime();
}
"#,
    )
    .unwrap();

    let dead = analyze_rust_dead(root);
    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);

    for live_sym in [
        "PidBackend::terminate_process",
        "terminate_process",
        "force_terminate_process",
        "ConfigManager::apply_runtime",
        "apply_runtime",
    ] {
        assert!(
            !is_dead(live_sym),
            "Expected {live_sym} to be live; dead={:?}",
            dead.symbols
        );
    }
    for dead_sym in [
        "PidBackend::dead_method",
        "never_called",
        "prod_test_only_fn",
        "prod_test_only_aliased",
    ] {
        assert!(
            is_dead(dead_sym),
            "Expected {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}

#[test]
fn test_rust_cargo_crate_name_mapping_and_pub_use_reexports() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("crates/app-server-daemon/src/backend")).unwrap();
    fs::create_dir_all(root.join("crates/cli/src")).unwrap();
    fs::create_dir_all(root.join("crates/cli/tests")).unwrap();

    fs::write(
        root.join("crates/app-server-daemon/Cargo.toml"),
        r#"
[package]
name = "codex-app-server-daemon"
version = "0.1.0"

[lib]
name = "codex_app_server_daemon"
path = "src/lib.rs"
"#,
    )
    .unwrap();
    fs::write(
        root.join("crates/app-server-daemon/src/lib.rs"),
        r#"
pub mod backend;
pub mod launch;
pub mod transport;

pub use backend::stderr_log::stderr_writer;
pub use launch::start_with_features;
pub use launch::unused_reexported_fn;
pub use transport::start_control_socket_acceptor;

pub fn run() {}
"#,
    )
    .unwrap();
    fs::write(
        root.join("crates/app-server-daemon/src/launch.rs"),
        r#"
pub fn start_with_features() {}
pub fn unused_reexported_fn() {}
"#,
    )
    .unwrap();
    fs::write(
        root.join("crates/app-server-daemon/src/backend/mod.rs"),
        "pub mod stderr_log;\n",
    )
    .unwrap();
    fs::write(
        root.join("crates/app-server-daemon/src/backend/stderr_log.rs"),
        "pub fn stderr_writer() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("crates/app-server-daemon/src/transport.rs"),
        "pub fn start_control_socket_acceptor() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("crates/cli/Cargo.toml"),
        "[package]\nname = \"codex-cli\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("crates/cli/src/main.rs"),
        r#"
use codex_app_server_daemon::stderr_writer;

fn main() {
    codex_app_server_daemon::run();
    codex_app_server_daemon::start_with_features();
    stderr_writer();
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("crates/cli/tests/integration_test.rs"),
        r#"
#[test]
fn only_tests_call_acceptor() {
    codex_app_server_daemon::start_control_socket_acceptor();
}
"#,
    )
    .unwrap();

    let dead = analyze_rust_dead(root);
    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);

    for live_sym in ["run", "start_with_features", "stderr_writer"] {
        assert!(
            !is_dead(live_sym),
            "Expected {live_sym} to be live; dead={:?}",
            dead.symbols
        );
    }
    for dead_sym in ["start_control_socket_acceptor", "unused_reexported_fn"] {
        assert!(
            is_dead(dead_sym),
            "Expected {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}

#[test]
fn test_rust_for_loop_mut_ref_and_iterator_closure_inference() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        r#"
struct SelectionAccumulator<'a> {
    tag: &'a str,
}

impl<'a> SelectionAccumulator<'a> {
    fn new(tag: &'a str) -> Self {
        Self { tag }
    }

    fn wants(&self) -> bool {
        !self.tag.is_empty()
    }

    fn accept(&mut self) {}

    fn finish(self) {}

    fn unused_method(&self) {}
}

fn maybe_acc() -> Option<SelectionAccumulator<'static>> {
    Some(SelectionAccumulator::new("x"))
}

fn main() {
    let mut states: Vec<SelectionAccumulator<'_>> = vec![SelectionAccumulator::new("a")];
    if states.iter().any(|state| state.wants()) {
        for state in &mut states {
            state.accept();
        }
    }
    let _done: Vec<()> = states.into_iter().map(|state| state.finish()).collect();
    if let Some(mut acc) = maybe_acc() {
        acc.accept();
    }
}
"#,
    )
    .unwrap();

    let dead = analyze_rust_dead(root);
    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);

    for live_sym in [
        "SelectionAccumulator::new",
        "SelectionAccumulator::wants",
        "SelectionAccumulator::accept",
        "SelectionAccumulator::finish",
    ] {
        assert!(
            !is_dead(live_sym),
            "Expected {live_sym} to be live; dead={:?}",
            dead.symbols
        );
    }
    assert!(
        is_dead("SelectionAccumulator::unused_method"),
        "Expected unused_method to remain dead; dead={:?}",
        dead.symbols
    );
}

#[test]
fn test_rust_type_alias_wrapper_receiver_chains() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        r#"
use std::sync::Arc;

struct EnrollmentState;

impl EnrollmentState {
    fn new() -> Self {
        Self
    }

    fn lock_for_request(&self) {}

    fn record_retry_after(&self) {}

    fn ping(&self) {}

    fn unused_method(&self) {}

    fn test_only_method(&self) {}
}

type CurrentEnrollment = Arc<EnrollmentState>;
type StateList = Vec<EnrollmentState>;

struct Session {
    current_enrollment: CurrentEnrollment,
}

impl Session {
    fn start_pairing(&self) {
        self.current_enrollment.lock_for_request();
    }
}

fn connect(current_enrollment: &CurrentEnrollment) {
    current_enrollment.record_retry_after();
}

fn ping_all(mut list: StateList) {
    for s in &mut list {
        s.ping();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_test_only_method() {
        let e: CurrentEnrollment = Arc::new(EnrollmentState::new());
        e.test_only_method();
    }
}

fn main() {
    let e: CurrentEnrollment = Arc::new(EnrollmentState::new());
    connect(&e);
    let s = Session { current_enrollment: e };
    s.start_pairing();
    ping_all(vec![EnrollmentState::new()]);
}
"#,
    )
    .unwrap();

    let dead = analyze_rust_dead(root);
    let is_dead = |sym: &str| dead.symbols.iter().any(|s| s.symbol == sym);

    for live_sym in [
        "EnrollmentState::new",
        "EnrollmentState::lock_for_request",
        "EnrollmentState::record_retry_after",
        "EnrollmentState::ping",
        "Session::start_pairing",
    ] {
        assert!(
            !is_dead(live_sym),
            "Expected {live_sym} to be live; dead={:?}",
            dead.symbols
        );
    }
    for dead_sym in [
        "EnrollmentState::unused_method",
        "EnrollmentState::test_only_method",
    ] {
        assert!(
            is_dead(dead_sym),
            "Expected {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}

#[test]
fn test_rust_batch2_review_edge_cases() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("crates/custom-lib/src")).unwrap();
    fs::create_dir_all(root.join("crates/app/src")).unwrap();

    // Custom [lib].path = "src/api.rs" + sibling src/main.rs
    fs::write(
        root.join("crates/custom-lib/Cargo.toml"),
        r#"
[package]
name = "custom-lib"
version = "0.1.0"

[lib]
name = "custom_lib"
path = "src/api.rs"
"#,
    )
    .unwrap();
    fs::write(
        root.join("crates/custom-lib/src/api.rs"),
        r#"
pub mod backend;
pub use backend::start as run;
"#,
    )
    .unwrap();
    fs::write(
        root.join("crates/custom-lib/src/backend.rs"),
        "pub fn start() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("crates/custom-lib/src/main.rs"),
        "fn run() {}\nfn main() {}\n",
    )
    .unwrap();

    fs::write(
        root.join("crates/app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("crates/app/src/spare.rs"), "pub fn run() {}\n").unwrap();
    fs::write(root.join("crates/app/src/real.rs"), "pub fn run() {}\n").unwrap();
    fs::write(root.join("crates/app/src/unix.rs"), "pub fn init() {}\n").unwrap();
    fs::write(
        root.join("crates/app/src/portable.rs"),
        "pub fn init() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("crates/app/src/factory.rs"),
        r#"
pub struct MadeWorker;
impl MadeWorker {
    pub fn do_work(&self) {}
    pub fn inline_work(&self) {}
}
pub fn make_worker() -> MadeWorker {
    MadeWorker
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("crates/app/src/main.rs"),
        r#"
use std::sync::Arc;
mod factory;
mod portable;
mod real;
mod spare;
mod unix;

mod unused_inline {
    use crate::spare as backend;
    fn dead_inline() {
        backend::run();
    }
}

use crate::factory::make_worker;
use crate::real as backend;

#[cfg(unix)]
use crate::unix as platform;
#[cfg(not(unix))]
use crate::portable as platform;

struct Output;
struct Decoy;
impl Decoy {
    fn finish(&self) {}
}
impl Output {
    fn finish(&self) {}
}
struct Failure;
impl Failure {
    fn report(&self) {}
}

fn compute() -> Result<Output, Failure> {
    Ok(Output)
}

struct InnerWorker;
impl InnerWorker {
    fn new() -> Self {
        Self
    }
}
type SharedWorker = Arc<InnerWorker>;

fn collide() {}
macro_rules! collide {
    () => {
        macro_only_helper()
    };
}
fn macro_only_helper() {}

fn collide_m() {}
macro_rules! collide_m {
    () => {
        macro_called_helper()
    };
}
fn macro_called_helper() {}

macro_rules! inner_m {
    () => {
        nested_macro_helper()
    };
}
macro_rules! outer_m {
    () => {
        inner_m!()
    };
}
fn nested_macro_helper() {}

macro_rules! alias_target_m {
    () => {
        aliased_macro_helper()
    };
}
fn aliased_macro_helper() {}
use alias_target_m as aliased_m;

fn root_reimport_helper() {}

mod inline_tools {
    use crate::factory::MadeWorker as LocalWorker;
    use self::inline_helper as inline_alias;
    use super::root_reimport_helper;
    use self::root_reimport_helper as reimport_alias;
    fn inline_helper() {}
    pub fn run_inline(w: LocalWorker) {
        inline_alias();
        reimport_alias();
        w.inline_work();
    }
}

fn main() {
    custom_lib::run();
    backend::run();
    platform::init();
    make_worker().do_work();
    let _ = compute().map_or_else(|err| err.report(), /* handle success */ |ok| ok.finish());
    let _w = SharedWorker::new(InnerWorker);
    collide();
    collide_m!();
    outer_m!();
    aliased_m!();
    inline_tools::run_inline(make_worker());
}
"#,
    )
    .unwrap();

    let dead = analyze_rust_dead(root);
    let is_dead_in = |file_suffix: &str, sym: &str| {
        dead.symbols
            .iter()
            .any(|s| s.file.ends_with(file_suffix) && s.symbol == sym)
    };

    for (file, sym) in [
        ("custom-lib/src/backend.rs", "start"),
        ("app/src/real.rs", "run"),
        ("app/src/unix.rs", "init"),
        ("app/src/portable.rs", "init"),
        ("app/src/factory.rs", "MadeWorker::do_work"),
        ("app/src/factory.rs", "MadeWorker::inline_work"),
        ("app/src/main.rs", "Output::finish"),
        ("app/src/main.rs", "Failure::report"),
        ("app/src/main.rs", "collide"),
        ("app/src/main.rs", "macro_called_helper"),
        ("app/src/main.rs", "nested_macro_helper"),
        ("app/src/main.rs", "aliased_macro_helper"),
        ("app/src/main.rs", "root_reimport_helper"),
        ("app/src/main.rs", "inline_helper"),
    ] {
        assert!(
            !is_dead_in(file, sym),
            "Expected {file}::{sym} to be live; dead={:?}",
            dead.symbols
        );
    }
    for (file, sym) in [
        ("custom-lib/src/main.rs", "run"),
        ("app/src/spare.rs", "run"),
        ("app/src/main.rs", "InnerWorker::new"),
        ("app/src/main.rs", "macro_only_helper"),
        ("app/src/main.rs", "collide_m"),
        ("app/src/main.rs", "Decoy::finish"),
    ] {
        assert!(
            is_dead_in(file, sym),
            "Expected {file}::{sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}

#[test]
fn test_rust_batch3_enum_patterns_transparent_wrappers_and_closure_params() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("src")).unwrap();

    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "batch3-demo"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    fs::write(
        root.join("src/helpers.rs"),
        r#"use std::collections::HashMap;

pub struct RemoteState;
impl RemoteState {
    pub fn touch_generic(&mut self) {}
    pub fn touch_impl_fn(&mut self) {}
    pub fn unused_remote(&mut self) {}
}

pub struct RemoteItem;
impl RemoteItem {
    pub fn mark(&self) {}
}

pub fn with_remote_pair<F: FnMut(&mut HashMap<String, RemoteState>, &RemoteItem)>(mut f: F) {
    let mut map = HashMap::new();
    let item = RemoteItem;
    f(&mut map, &item);
}

pub fn with_impl_fn(f: impl Fn(&mut RemoteState)) {
    let mut s = RemoteState;
    f(&mut s);
}

pub struct Weak;
impl Weak {
    pub fn refresh(&self) {}
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("src/main.rs"),
        r#"mod helpers;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use helpers::{RemoteState, with_impl_fn, with_remote_pair};

// --- Category 1: Enum variant fields & pattern bindings ---
struct Projector;
impl Projector {
    fn push(&mut self) {}
    fn finish(&mut self) {}
    fn reset(&mut self) {}
    fn tuple_ping(&mut self) {}
    fn mutex_touch(&mut self) {}
    fn map_touch(&mut self) {}
    fn dead_projector_method(&mut self) {}
}

struct Decoy;
impl Decoy {
    fn push(&mut self) {}
    fn terminate(&self) {}
    fn upsert_thread(&mut self) {}
    fn work(&self) {}
    fn refresh(&self) {}
    fn slot_tuple_finish(&self) {}
    fn slot_closure_finish(&self) {}
}

struct FirstSlot;
impl FirstSlot {
    fn slot_tuple_finish(&self) {}
    fn slot_closure_finish(&self) {}
}

struct SecondSlot;
impl SecondSlot {
    fn slot_tuple_finish(&self) {}
    fn slot_closure_finish(&self) {}
}

struct SlotPair(FirstSlot, SecondSlot);

fn with_slot_pair(f: impl Fn(FirstSlot, SecondSlot)) {
    f(FirstSlot, SecondSlot);
}

struct AliasWorker;
impl AliasWorker {
    fn work(&self) {}
}

mod api {
    use super::AliasWorker as CallbackWorker;
    pub fn with_aliased_worker(f: impl Fn(CallbackWorker)) {
        f(CallbackWorker);
    }
}

enum Mode {
    Heredoc { py: Projector },
    Inline { py: Projector },
    Wrapped { inner: Option<Projector> },
}

struct Runner {
    mode: Mode,
}
impl Runner {
    fn step(&mut self) {
        match &mut self.mode {
            Mode::Heredoc { py } => {
                py.push();
                py.finish();
            }
            Mode::Inline { py, .. } => py.finish(),
            Mode::Wrapped { inner: Some(p) } => p.reset(),
            Mode::Wrapped { inner: None } => {}
        }
    }
}

struct Terminator;
impl Terminator {
    #[cfg(unix)]
    fn terminate(&self) {}
    #[cfg(windows)]
    fn terminate(&self) {}
    #[cfg(not(any(unix, windows)))]
    fn terminate(&self) {}

    fn terminate_discovery(&self) -> Result<(), ()> {
        Ok(())
    }
    fn dead_terminator_method(&self) {}
}

enum ProcessKind {
    Local(Option<Terminator>),
    Empty,
}

struct ProcessHandle {
    kind: ProcessKind,
}
impl ProcessHandle {
    fn terminate_discovery(&self) -> Result<(), ()> {
        let ProcessKind::Local(Some(terminator)) = &self.kind else {
            return Err(());
        };
        terminator
            .terminate_discovery()
            .inspect_err(|_| terminator.terminate())
    }
    fn terminate(&self) {
        if let ProcessKind::Local(Some(terminator)) = &self.kind {
            terminator.terminate();
        }
    }
    fn dead_handle_method(&self) {}
}

struct TupleWrap(Projector);

// --- Category 2: Transparent Option/Result/Weak/Mutex unwrapping ---
struct Cleanup {
    process: Weak<ProcessHandle>,
    cache: Mutex<Projector>,
    map: HashMap<String, Projector>,
}
impl Cleanup {
    fn run(&mut self) -> Result<(), ()> {
        let process = self.process.upgrade();
        process.ok_or_else(|| ())?.terminate_discovery()?;
        if let Some(p) = self.process.upgrade() {
            p.terminate();
        }
        if let Ok(p) = self.cache.get_mut() {
            p.mutex_touch();
        }
        if let Some(p) = self.map.get_mut("k") {
            p.map_touch();
        }
        Ok(())
    }
}

// --- Category 3: Closure parameter type inference from callee Fn* signatures ---
struct WatchState;
impl WatchState {
    fn upsert_thread(&mut self) {}
    fn remove_thread(&mut self) {}
    fn explicit_self_call(&mut self) {}
    fn ufcs_call(&mut self) {}
    fn dead_watch_method(&mut self) {}
    fn test_only_prod_method(&mut self) {}
}

struct Watcher {
    state: WatchState,
}
impl Watcher {
    fn mutate_and_publish<F>(&mut self, mutate: F)
    where
        F: FnOnce(&mut WatchState) -> Option<()>,
    {
        let _ = mutate(&mut self.state);
    }
    fn with_arc_self(self: Arc<Self>, f: impl FnOnce(&mut WatchState)) {
        let mut s = WatchState;
        f(&mut s);
    }
    fn with_ufcs_self(&self, f: impl FnOnce(&mut WatchState)) {
        let mut s = WatchState;
        f(&mut s);
    }
    fn upsert(&mut self) {
        self.mutate_and_publish(move |state| {
            state.upsert_thread();
            None
        });
    }
    fn remove(&mut self) {
        self.mutate_and_publish(|state| {
            state.remove_thread();
            None
        });
    }
}

fn test_only_free_fn() {}

fn main() {
    let mut r = Runner {
        mode: Mode::Heredoc { py: Projector },
    };
    r.step();
    let TupleWrap(mut inner) = TupleWrap(Projector);
    inner.tuple_ping();

    let SlotPair(_, second) = SlotPair(FirstSlot, SecondSlot);
    second.slot_tuple_finish();
    with_slot_pair(|/* skip */ _, s| s.slot_closure_finish());
    api::with_aliased_worker(|w| w.work());
    let local_weak: helpers::Weak = helpers::Weak;
    local_weak.refresh();

    let h = Arc::new(ProcessHandle {
        kind: ProcessKind::Local(Some(Terminator)),
    });
    let mut c = Cleanup {
        process: Arc::downgrade(&h),
        cache: Mutex::new(Projector),
        map: HashMap::new(),
    };
    let _ = c.run();

    let mut w = Watcher { state: WatchState };
    w.upsert();
    w.remove();
    let arc_w = Arc::new(Watcher { state: WatchState });
    arc_w.with_arc_self(|st| st.explicit_self_call());
    Watcher::with_ufcs_self(&w, |st| st.ufcs_call());

    with_remote_pair(|map, item| {
        if let Some(st) = map.get_mut("a") {
            st.touch_generic();
        }
        item.mark();
    });
    with_impl_fn(|st| st.touch_impl_fn());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_tests_call_these() {
        let mut ws = WatchState;
        ws.test_only_prod_method();
        test_only_free_fn();
    }
}
"#,
    )
    .unwrap();

    let dead = analyze_rust_dead(root);
    let is_dead_in = |file_suffix: &str, sym: &str| {
        dead.symbols
            .iter()
            .any(|s| s.file.ends_with(file_suffix) && s.symbol == sym)
    };

    for (file, sym) in [
        ("src/main.rs", "Projector::push"),
        ("src/main.rs", "Projector::finish"),
        ("src/main.rs", "Projector::reset"),
        ("src/main.rs", "Projector::tuple_ping"),
        ("src/main.rs", "Projector::mutex_touch"),
        ("src/main.rs", "Projector::map_touch"),
        ("src/main.rs", "Terminator::terminate"),
        ("src/main.rs", "Terminator::terminate_discovery"),
        ("src/main.rs", "ProcessHandle::terminate_discovery"),
        ("src/main.rs", "ProcessHandle::terminate"),
        ("src/main.rs", "WatchState::upsert_thread"),
        ("src/main.rs", "WatchState::remove_thread"),
        ("src/main.rs", "WatchState::explicit_self_call"),
        ("src/main.rs", "WatchState::ufcs_call"),
        ("src/main.rs", "SecondSlot::slot_tuple_finish"),
        ("src/main.rs", "SecondSlot::slot_closure_finish"),
        ("src/main.rs", "AliasWorker::work"),
        ("src/helpers.rs", "RemoteState::touch_generic"),
        ("src/helpers.rs", "RemoteState::touch_impl_fn"),
        ("src/helpers.rs", "RemoteItem::mark"),
        ("src/helpers.rs", "Weak::refresh"),
    ] {
        assert!(
            !is_dead_in(file, sym),
            "Expected {file}::{sym} to be live; dead={:?}",
            dead.symbols
        );
    }

    for (file, sym) in [
        ("src/main.rs", "Projector::dead_projector_method"),
        ("src/main.rs", "Terminator::dead_terminator_method"),
        ("src/main.rs", "ProcessHandle::dead_handle_method"),
        ("src/main.rs", "WatchState::dead_watch_method"),
        ("src/main.rs", "WatchState::test_only_prod_method"),
        ("src/main.rs", "test_only_free_fn"),
        ("src/main.rs", "FirstSlot::slot_tuple_finish"),
        ("src/main.rs", "FirstSlot::slot_closure_finish"),
        ("src/main.rs", "Decoy::push"),
        ("src/main.rs", "Decoy::terminate"),
        ("src/main.rs", "Decoy::upsert_thread"),
        ("src/main.rs", "Decoy::work"),
        ("src/main.rs", "Decoy::refresh"),
        ("src/main.rs", "Decoy::slot_tuple_finish"),
        ("src/main.rs", "Decoy::slot_closure_finish"),
        ("src/helpers.rs", "RemoteState::unused_remote"),
    ] {
        assert!(
            is_dead_in(file, sym),
            "Expected {file}::{sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}
