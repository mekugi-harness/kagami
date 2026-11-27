use kagami::config::KagamiConfig;
use kagami::graph::dead::{analyze_dead, DeadOptions, DeadScope};
use kagami::graph::CodeGraph;
use kagami::ir::similar::pair_similarity;
use kagami::ir::IrNode;
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
        format!("{}\nfunction childWork() {{}}\nchildWork();\n{}\n<div>Hi</div>\n", tag_open, tag_close),
    ).unwrap();
    fs::write(
        root.join("src/Orphan.svelte"),
        format!("{}\nfunction orphanWork() {{}}\n{}\n<div>Orphan</div>\n", tag_open, tag_close),
    ).unwrap();
    fs::write(
        root.join("src/lib/setup.ts"),
        "export function bootApp() {}\nexport function deadUtil() {}\n",
    ).unwrap();

    let ignores = KagamiConfig::default().build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();

    let has_resolved_app_edge = graph.edges.iter().any(|e| {
        e.file == "src/main.ts"
            && e.from.contains("<module>")
            && e.to.starts_with("src/App.svelte::App@")
            && e.confidence == Confidence::Resolved
    });
    assert!(has_resolved_app_edge, "expected Confidence::Resolved edge from src/main.ts::<module> to src/App.svelte::App");

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
    assert!(!has("App"), "App.svelte must be live via main.ts mount(App, ...)");
    assert!(!has("Child"), "Child.svelte must be live via App.svelte");
    assert!(!has("handleClick"), "handleClick must be live via App.svelte template");
    assert!(!has("childWork"), "childWork must be live via Child.svelte <module>");
    assert!(!has("bootApp"), "bootApp must be live via aliased relative import start()");
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

    fs::write(root.join(".kagami.toml"), "test_paths = [\"custom_helpers/**\"]\n").unwrap();
    fs::write(
        root.join("internal/hints/hints.go"),
        "package hints\ntype SelectionHints struct{}\nfunc (h *SelectionHints) ActionAt() {}\nfunc (h *SelectionHints) UnusedAction() {}\n",
    ).unwrap();
    fs::write(
        root.join("internal/testsupport/fixture.go"),
        "package testsupport\nfunc MakeFixture() {}\n",
    ).unwrap();
    fs::write(
        root.join("custom_helpers/helper.go"),
        "package custom_helpers\nfunc CustomHelper() {}\n",
    ).unwrap();
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
    assert!(!has("SelectionHints.ActionAt"), "cross-pkg receiver-var call must keep ActionAt alive");
    assert!(has("SelectionHints.UnusedAction"), "uncalled method should be dead");
    assert!(!has("MakeFixture"), "testsupport/ package must be classified as is_test: true");
    assert!(!has("CustomHelper"), "test_paths glob in .kagami.toml must mark custom_helpers as is_test: true");
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
    assert_eq!(all_dead.total_dead, all_dead.unreferenced + all_dead.unreachable_clusters);
    assert!(all_dead.symbols.iter().any(|s| s.symbol == "Svc.DynamicMethod" && s.confidence == Confidence::Heuristic));
    assert!(all_dead.symbols.iter().any(|s| s.symbol == "UnusedFunc" && s.confidence == Confidence::Resolved));

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
    assert_eq!(resolved_only.total_dead, resolved_only.unreferenced + resolved_only.unreachable_clusters);
    assert!(!resolved_only.symbols.iter().any(|s| s.symbol == "Svc.DynamicMethod"));
    assert!(resolved_only.symbols.iter().any(|s| s.symbol == "UnusedFunc"));

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
            IrNode::Branch { cond, then_br, else_br } => {
                has_opaque(cond) || has_opaque(then_br) || else_br.as_deref().map(has_opaque).unwrap_or(false)
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

    assert!(!has_opaque(&ext_a.ir_funcs[0].body), "expected zero Opaque nodes, got {:?}", ext_a.ir_funcs[0].body);
    assert!(!has_opaque(&ext_b.ir_funcs[0].body), "expected zero Opaque nodes, got {:?}", ext_b.ir_funcs[0].body);
    let score = pair_similarity(&ext_a.ir_funcs[0], &ext_b.ir_funcs[0], false);
    assert!(score >= 0.95, "expected high similarity for equivalent TS funcs, got {}", score);
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
    ).unwrap();
    fs::write(
        root.join("src/worker/index.ts"),
        "export function run() {}\n",
    ).unwrap();
    let tag_open = concat!("<", "script lang=\"ts\">");
    let tag_close = concat!("</", "script>");
    fs::write(
        root.join("src/UnusedComp.svelte"),
        format!("{}\nfunction unusedWork() {{}}\n{}\n<div>No</div>\n", tag_open, tag_close),
    ).unwrap();
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
    let has = |file: &str, sym: &str| dead.symbols.iter().any(|s| s.file == file && s.symbol == sym);
    assert!(!has("src/worker.ts", "initialize"), "renamed default import start() must keep initialize alive");
    assert!(has("src/UnusedComp.svelte", "UnusedComp"), "typeof and shadowed param must not keep UnusedComp alive");
    assert!(has("src/worker/index.ts", "run"), "import from ./worker must not falsely link worker/index.ts when worker.ts exists");

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
    fs::write(root.join("src/list_def.ts"), "function listRun() {}\nexport { listRun as default };\n").unwrap();
    fs::write(root.join("src/wrapped.ts"), "function wrappedRun() {}\nexport default (wrappedRun);\n").unwrap();
    fs::write(root.join("src/unused_def.ts"), "export default function unusedLegacy() {}\n").unwrap();
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
    let has = |file: &str, sym: &str| dead.symbols.iter().any(|s| s.file == file && s.symbol == sym);
    assert!(!has("src/esm.ts", "esmRun"), "./esm.js specifier must resolve to src/esm.ts");
    assert!(!has("src/list_def.ts", "listRun"), "export {{ listRun as default }} must keep listRun alive");
    assert!(!has("src/wrapped.ts", "wrappedRun"), "export default (wrappedRun) must keep wrappedRun alive");
    assert!(has("src/unused_def.ts", "unusedLegacy"), "external default import must not keep unrelated local default export alive");
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
type OldTarget struct{}
func (o *OldTarget) Exec() {}

func build() *OldTarget { return &OldTarget{} }

func useShadowedFactory(build func() any) {
	_ = build()
}

// 3. Shadowing local with unsupported type must mask outer receiver type
type OuterWorker struct{}
func (w *OuterWorker) Work() {}

type OuterCfg struct {
	Worker *OuterWorker
}

func useShadowedCfg(cfg *OuterCfg) {
	_ = cfg
	fn := func(cfg struct{ Worker int }) {
		_ = cfg.Worker
	}
	fn(struct{ Worker int }{})
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

func RunAll() {
	var _, worker = pair()
	worker.Run()
	useShadowedFactory(func() any { return nil })
	useShadowedCfg(&OuterCfg{})
	useAnonIface()
	_ = &BlankHolder{}
	w := &WrapperFactory{}
	_ = w.Make()
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
    assert!(!has_resolved_call("useAnonIface", "ImplA.Ping"));
    assert!(has_resolved_call("TestCallsHelper", "Second.TestHelperMethod"));

    for dead_sym in [
        "First.Run",
        "OldTarget.Exec",
        "OuterWorker.Work",
        "BlankHolder.UnusedReload",
        "BaseFactory.Make",
    ] {
        assert!(
            is_dead(dead_sym),
            "Expected {dead_sym} to remain dead; dead={:?}",
            dead.symbols
        );
    }
}
