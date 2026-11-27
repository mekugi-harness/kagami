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
