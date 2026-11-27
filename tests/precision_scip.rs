use kagami::config::KagamiConfig;
use kagami::graph::dead::{analyze_dead, DeadOptions, DeadScope};
use kagami::graph::scip::{apply_scip_overlay, load_scip_file};
use kagami::graph::CodeGraph;
use kagami::ir::normalize::{normalize_func, NormalizeOptions};
use kagami::ir::similar::pair_similarity;
use kagami::ir::IrNode;
use kagami::lang;
use kagami::model::{Confidence, EdgeKind, Language};
use kagami::walker::discover_files;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_go_package_qualified_cobra_and_shadowing() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("cmd/app")).unwrap();
    fs::create_dir_all(root.join("internal/launch")).unwrap();
    fs::create_dir_all(root.join("internal/fmt")).unwrap();
    fs::write(
        root.join("cmd/app/main.go"),
        "package main\n\nimport (\n    \"fmt\"\n    \"proj/internal/launch\"\n)\n\nfunc main() {\n    fmt.Println(\"hi\")\n    launch.Run()\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("internal/fmt/fake.go"),
        "package fmt\n\nfunc Println(s string) {}\n",
    )
    .unwrap();
    fs::write(
        root.join("internal/launch/run.go"),
        "package launch\n\ntype Command struct {\n    RunE func() error\n}\n\nfunc Run() {\n    _ = Command{RunE: runWorker}\n    register(onHook)\n    shadowed := 1\n    useVal(shadowed)\n}\n\nfunc runWorker() error { return nil }\nfunc onHook() {}\nfunc register(fn func()) { fn() }\nfunc useVal(v int) {}\nfunc shadowed() {}\nfunc trulyDead() {}\n",
    )
    .unwrap();

    let cfg = KagamiConfig::load(root).unwrap();
    let ignores = cfg.build_ignore_globset().unwrap();
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
        },
    );
    let dead_names: Vec<&str> = dead.symbols.iter().map(|s| s.symbol.as_str()).collect();
    assert!(!dead_names.contains(&"Run"), "launch.Run should be live via main()");
    assert!(!dead_names.contains(&"runWorker"), "Cobra RunE callback should be live");
    assert!(!dead_names.contains(&"onHook"), "callback arg should be live");
    assert!(dead_names.contains(&"trulyDead"), "trulyDead should be flagged dead");
    assert!(dead_names.contains(&"shadowed"), "local var 'shadowed' must not keep func shadowed() alive");
    assert!(dead_names.contains(&"Println"), "stdlib fmt.Println must not keep local internal/fmt.Println alive");
}

#[test]
fn test_scip_json_overlay_exact_edge_identity() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(
        root.join("a.rs"),
        "fn main() {\n    helper_fn();\n}\nfn helper_fn() {}\n",
    )
    .unwrap();

    let scip_val = serde_json::json!({
        "documents": [{
            "relative_path": "a.rs",
            "occurrences": [
                { "range": [0, 3, 7], "symbol": "scip rust main()", "symbol_roles": 1 },
                { "range": [1, 4, 13], "symbol": "scip rust helper_fn()", "symbol_roles": 0 },
                { "range": [3, 3, 12], "symbol": "scip rust helper_fn()", "symbol_roles": 1 }
            ]
        }]
    });
    let scip_path = root.join("index.scip.json");
    fs::write(&scip_path, serde_json::to_string(&scip_val).unwrap()).unwrap();

    let cfg = KagamiConfig::load(root).unwrap();
    let ignores = cfg.build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let mut graph = CodeGraph::build(root, &files, false).unwrap();

    let idx = load_scip_file(&scip_path).unwrap();
    let updated = apply_scip_overlay(&mut graph, &idx, root);
    assert!(updated >= 1);
    assert!(
        graph.edges.iter().any(|e| {
            e.from == "a.rs::main@1"
                && e.to == "a.rs::helper_fn@4"
                && e.confidence == Confidence::Exact
        }),
        "expected exact edge from a.rs::main@1 to a.rs::helper_fn@4"
    );
}

// Helper to encode protobuf wire format for binary SCIP fixture testing
fn push_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        buf.push(b);
        if v == 0 {
            break;
        }
    }
}

fn push_len_delim(buf: &mut Vec<u8>, field: u64, data: &[u8]) {
    push_varint(buf, (field << 3) | 2);
    push_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn push_varint_field(buf: &mut Vec<u8>, field: u64, v: u64) {
    push_varint(buf, field << 3);
    push_varint(buf, v);
}

fn encode_single_line_range(line: u64, sc: u64, ec: u64) -> Vec<u8> {
    let mut b = Vec::new();
    push_varint_field(&mut b, 1, line);
    push_varint_field(&mut b, 2, sc);
    push_varint_field(&mut b, 3, ec);
    b
}

#[test]
fn test_scip_binary_protobuf_typed_ranges_impl_subproject_and_locals() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // Create root main.rs AND subproject sub/main.rs to verify project_root rebasing
    fs::write(root.join("main.rs"), "fn main() {}\nfn unrelated() {}\n").unwrap();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(
        root.join("sub/main.rs"),
        "trait Runner {}\nstruct Impl {}\nfn main() {\n    let x = 1;\n}\nfn unused() {\n    let x = 2;\n}\n",
    )
    .unwrap();

    // Build binary SCIP index with Metadata.project_root = sub, typed single_line_range (field 8),
    // field 4 implementation relationship (Impl -> Runner), and local 0 in both main() and unused()
    let mut meta = Vec::new();
    let sub_root = format!("file://{}", root.join("sub").display());
    push_len_delim(&mut meta, 3, sub_root.as_bytes()); // Metadata.project_root is field 3

    let mut doc = Vec::new();
    push_len_delim(&mut doc, 1, b"main.rs");

    // Occurrence 1: trait Runner def at line 0 (typed range field 8)
    let mut o1 = Vec::new();
    push_len_delim(&mut o1, 2, b"scip rust Runner#");
    push_varint_field(&mut o1, 3, 1); // Definition
    push_len_delim(&mut o1, 8, &encode_single_line_range(0, 6, 12));
    push_len_delim(&mut doc, 2, &o1);

    // Occurrence 2: struct Impl def at line 1 (typed range field 8)
    let mut o2 = Vec::new();
    push_len_delim(&mut o2, 2, b"scip rust Impl#");
    push_varint_field(&mut o2, 3, 1); // Definition
    push_len_delim(&mut o2, 8, &encode_single_line_range(1, 7, 11));
    push_len_delim(&mut doc, 2, &o2);

    // Occurrence 3 & 4: local 0 def in unused() line 6 and ref in main() line 3 (must be ignored!)
    let mut o3 = Vec::new();
    push_len_delim(&mut o3, 2, b"local 0");
    push_varint_field(&mut o3, 3, 1);
    push_len_delim(&mut o3, 8, &encode_single_line_range(6, 8, 9));
    push_len_delim(&mut doc, 2, &o3);

    let mut o4 = Vec::new();
    push_len_delim(&mut o4, 2, b"local 0");
    push_varint_field(&mut o4, 3, 0);
    push_len_delim(&mut o4, 8, &encode_single_line_range(3, 8, 9));
    push_len_delim(&mut doc, 2, &o4);

    // SymbolInformation for Impl with field 4 relationship is_implementation=true -> Runner
    let mut rel = Vec::new();
    push_len_delim(&mut rel, 1, b"scip rust Runner#");
    push_varint_field(&mut rel, 3, 1); // is_implementation = true
    let mut sinfo = Vec::new();
    push_len_delim(&mut sinfo, 1, b"scip rust Impl#");
    push_len_delim(&mut sinfo, 4, &rel); // field 4!
    push_len_delim(&mut doc, 3, &sinfo);

    let mut index_bytes = Vec::new();
    push_len_delim(&mut index_bytes, 1, &meta);
    push_len_delim(&mut index_bytes, 2, &doc);

    let scip_path = root.join("sub.scip");
    fs::write(&scip_path, &index_bytes).unwrap();

    let cfg = KagamiConfig::load(root).unwrap();
    let ignores = cfg.build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let mut graph = CodeGraph::build(root, &files, false).unwrap();

    let idx = load_scip_file(&scip_path).unwrap();
    let updated = apply_scip_overlay(&mut graph, &idx, root);
    assert!(updated >= 1);

    // Verify sub/main.rs::Impl@2 -> sub/main.rs::Runner@1 Implements edge exists with Exact confidence
    assert!(
        graph.edges.iter().any(|e| {
            e.from == "sub/main.rs::Impl@2"
                && e.to == "sub/main.rs::Runner@1"
                && e.kind == EdgeKind::Implements
                && e.confidence == Confidence::Exact
        }),
        "expected Exact Implements edge from sub/main.rs::Impl@2 to sub/main.rs::Runner@1"
    );

    // Verify local 0 did NOT create a bogus edge from main() to unused()
    assert!(
        !graph
            .edges
            .iter()
            .any(|e| e.from == "sub/main.rs::main@3" && e.to == "sub/main.rs::unused@6"),
        "local 0 must not create bogus cross-function edges"
    );
}

fn run_prod_dead(root: &std::path::Path) -> Vec<(String, String)> {
    let cfg = KagamiConfig::load(root).unwrap();
    let ignores = cfg.build_ignore_globset().unwrap();
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
        },
    );
    dead.symbols
        .into_iter()
        .map(|s| (s.file, s.symbol))
        .collect()
}

#[test]
fn test_scip_unicode_disambiguators_and_competing_edge_pruning() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::write(
        root.join("main.rs"),
        "fn main() {\n    café(); keep_sibling();\n    dispatch_work();\n}\nfn café() {}\nfn keep_sibling() {}\n",
    )
    .unwrap();
    fs::write(root.join("real.rs"), "pub fn dispatch_work() {}\n").unwrap();
    fs::write(root.join("wrong.rs"), "pub fn dispatch_work() {}\n").unwrap();

    let cfg = KagamiConfig::load(root).unwrap();
    let ignores = cfg.build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let mut graph = CodeGraph::build(root, &files, false).unwrap();

    // Before SCIP: heuristic resolution links main() to BOTH real.rs and wrong.rs dispatch_work()
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.from == "main.rs::main@1" && e.to == "wrong.rs::dispatch_work@1"),
        "expected initial guessed edge to wrong.rs::dispatch_work@1"
    );

    // SCIP index with Unicode identifier café(+1)., disambiguated dispatch_work(+2)().,
    // and a parameter descriptor café(+1).(x) on the same line that must not override café.
    let scip_val = serde_json::json!({
        "documents": [
            {
                "relative_path": "main.rs",
                "occurrences": [
                    { "range": [0, 3, 7], "symbol": "scip rust main().", "symbol_roles": 1 },
                    { "range": [1, 4, 8], "symbol": "scip rust café(+1).", "symbol_roles": 0 },
                    { "range": [2, 4, 17], "symbol": "scip rust real/dispatch_work(+2)().", "symbol_roles": 0 },
                    { "range": [4, 3, 7], "symbol": "scip rust café(+1).", "symbol_roles": 1 },
                    { "range": [4, 8, 9], "symbol": "scip rust café(+1).(x)", "symbol_roles": 1 }
                ]
            },
            {
                "relative_path": "real.rs",
                "occurrences": [
                    { "range": [0, 7, 20], "symbol": "scip rust real/dispatch_work(+2)().", "symbol_roles": 1 }
                ]
            }
        ]
    });
    let scip_path = root.join("index.scip.json");
    fs::write(&scip_path, serde_json::to_string(&scip_val).unwrap()).unwrap();

    let idx = load_scip_file(&scip_path).unwrap();
    let updated = apply_scip_overlay(&mut graph, &idx, root);
    assert!(updated >= 2, "expected at least 2 exact edges, got {}", updated);

    // Unicode + numeric disambiguator matched café
    assert!(
        graph.edges.iter().any(|e| {
            e.from == "main.rs::main@1"
                && e.to == "main.rs::café@5"
                && e.confidence == Confidence::Exact
        }),
        "expected Exact edge to Unicode symbol main.rs::café@5"
    );
    // Disambiguated (+2)(). matched real.rs::dispatch_work and pruned wrong.rs::dispatch_work
    assert!(
        graph.edges.iter().any(|e| {
            e.from == "main.rs::main@1"
                && e.to == "real.rs::dispatch_work@1"
                && e.confidence == Confidence::Exact
        }),
        "expected Exact edge to real.rs::dispatch_work@1"
    );
    assert!(
        !graph
            .edges
            .iter()
            .any(|e| e.from == "main.rs::main@1" && e.to == "wrong.rs::dispatch_work@1"),
        "competing guessed edge to wrong.rs::dispatch_work@1 must be pruned"
    );

    // Dead-code analysis now sees wrong.rs::dispatch_work as dead while real.rs::dispatch_work is live
    let dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: Default::default(),
            include_clusters: true,
            path_filter: None,
        },
    );
    let dead_pairs: Vec<(&str, &str)> = dead
        .symbols
        .iter()
        .map(|s| (s.file.as_str(), s.symbol.as_str()))
        .collect();
    assert!(
        dead_pairs.contains(&("wrong.rs", "dispatch_work")),
        "wrong.rs::dispatch_work should be dead after SCIP pruning, got {:?}",
        dead_pairs
    );
    assert!(
        !dead_pairs.contains(&("real.rs", "dispatch_work")),
        "real.rs::dispatch_work should be live"
    );
    assert!(
        !dead_pairs.contains(&("main.rs", "café")),
        "main.rs::café should be live"
    );
    // Sibling call on the same line as SCIP-resolved café() must NOT be pruned
    assert!(
        !dead_pairs.contains(&("main.rs", "keep_sibling")),
        "sibling call keep_sibling() on the same line must not be erased by SCIP pruning"
    );
}

#[test]
fn test_svelte_live_vs_unused_setup_and_relative_import_alias() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src/routes/ui")).unwrap();
    fs::create_dir_all(root.join("src/legacy")).unwrap();
    fs::create_dir_all(root.join("src/components")).unwrap();

    fs::write(
        root.join("src/routes/+page.svelte"),
        r#"<script lang="ts">
import PrimaryButton from './ui/Button.svelte';
import './ui/Shared.svelte';
import type { Props } from './ui/TypeOnly.svelte';
const ready = liveSetup();
function liveSetup() { return true; }
const unusedArrow = () => deadArrowHelper();
function deadArrowHelper() {}
</script>
<PrimaryButton />
"#,
    )
    .unwrap();
    fs::write(
        root.join("src/routes/ui/Shared.svelte"),
        r#"<script module lang="ts">
sharedModuleInit();
function sharedModuleInit() {}
</script>
<div>Shared</div>
"#,
    )
    .unwrap();
    fs::write(
        root.join("src/routes/ui/TypeOnly.svelte"),
        r#"<script module lang="ts">
export type Props = { label: string };
typeOnlyInit();
function typeOnlyInit() {}
</script>
<div>TypeOnly</div>
"#,
    )
    .unwrap();
    fs::write(
        root.join("src/routes/ui/Button.svelte"),
        r#"<script lang="ts">
buttonInit();
function buttonInit() {}
</script>
<button>OK</button>
"#,
    )
    .unwrap();
    fs::write(
        root.join("src/legacy/Button.svelte"),
        r#"<script lang="ts">
legacyInit();
function legacyInit() {}
</script>
<button>Old</button>
"#,
    )
    .unwrap();
    fs::write(
        root.join("src/components/Orphan.svelte"),
        r#"<script lang="ts">
orphanSetup();
function orphanSetup() {}
</script>
<div>Unused</div>
"#,
    )
    .unwrap();

    let dead = run_prod_dead(root);
    let has = |file: &str, sym: &str| dead.iter().any(|(f, s)| f == file && s == sym);

    assert!(!has("src/routes/+page.svelte", "liveSetup"), "live route setup helper must stay alive");
    assert!(!has("src/routes/ui/Button.svelte", "Button"), "imported relative Button.svelte must stay alive");
    assert!(!has("src/routes/ui/Button.svelte", "buttonInit"), "imported Button setup helper must stay alive");
    assert!(!has("src/routes/ui/Shared.svelte", "sharedModuleInit"), "statically imported <script module> setup helper must stay alive");
    assert!(has("src/routes/ui/TypeOnly.svelte", "typeOnlyInit"), "type-only import must not keep <script module> helper alive");

    assert!(has("src/routes/+page.svelte", "unusedArrow"), "uncalled arrow fn in live component should be dead");
    assert!(has("src/routes/+page.svelte", "deadArrowHelper"), "helper only called by dead arrow fn should be dead");
    assert!(has("src/legacy/Button.svelte", "Button"), "same-named legacy/Button.svelte must not be kept alive by ./ui/Button.svelte");
    assert!(has("src/legacy/Button.svelte", "legacyInit"), "legacy Button setup helper should be dead");
    assert!(has("src/components/Orphan.svelte", "Orphan"), "unused non-route component should be dead");
    assert!(has("src/components/Orphan.svelte", "orphanSetup"), "unused component setup helper should be dead");
}

#[test]
fn test_ts_js_exported_and_non_function_module_initializers() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::write(
        root.join("app.ts"),
        r#"export const app = initApp();
const cfg = loadCfg();
const { port = pickPort() } = {};
const runParen = (() => parenHelper());
runParen();
const withDef = (x = defaultArgHelper()) => x;
withDef();
class Widget { render() {} }
const widgets = { svelte: new Widget() };
widgets.svelte.render();
const unusedFn = () => deadFromClosure();
export const unusedExported = () => deadFromExportedClosure();
function initApp() { return 1; }
function loadCfg() { return 2; }
function pickPort() { return 8080; }
function parenHelper() {}
function defaultArgHelper() { return 1; }
function deadFromClosure() {}
function deadFromExportedClosure() {}
"#,
    )
    .unwrap();

    let dead = run_prod_dead(root);
    let has = |sym: &str| dead.iter().any(|(_, s)| s == sym);

    assert!(!has("initApp"), "exported non-function initializer must keep initApp alive");
    assert!(!has("loadCfg"), "top-level non-function initializer must keep loadCfg alive");
    assert!(!has("pickPort"), "destructuring default call must keep pickPort alive");
    assert!(!has("parenHelper"), "called parenthesized arrow fn must keep parenHelper alive");
    assert!(!has("defaultArgHelper"), "default parameter call must keep defaultArgHelper alive");
    assert!(!has("Widget.render"), "widgets.svelte.render() must resolve to Widget.render");
    assert!(has("unusedFn"), "uncalled arrow function should be dead");
    assert!(has("unusedExported"), "uncalled exported arrow function should be dead");
    assert!(has("deadFromClosure"), "helper inside uncalled arrow function must be dead");
    assert!(has("deadFromExportedClosure"), "helper inside uncalled exported arrow function must be dead");
}

#[test]
fn test_go_package_var_closure_vs_init_call() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::write(
        root.join("main.go"),
        r#"package main

type Hooks struct {
    Run func()
}

var boot = initFoo()
var hooks = Hooks{Run: serveHook}
var lazyHandler = func() {
    deadInClosure()
}

func initFoo() int { return 1 }
func serveHook() {}
func deadInClosure() {}
func main() {
    hooks.Run()
}
"#,
    )
    .unwrap();

    let dead = run_prod_dead(root);
    let has = |sym: &str| dead.iter().any(|(_, s)| s == sym);

    assert!(!has("initFoo"), "package var with init call must keep initFoo alive");
    assert!(!has("serveHook"), "callback in referenced package var hooks.Run() must stay alive");
    assert!(has("deadInClosure"), "uncalled package var closure must not keep deadInClosure alive");
}

#[test]
fn test_ir_go_bare_error_return_vs_fallback() {
    let src = r#"package demo

func bareErr() (res int, err error) {
    res, err = work()
    if err != nil {
        return
    }
    return res, nil
}

func explicitErr() (int, error) {
    res, err := work()
    if err != nil {
        return 0, err
    }
    return res, nil
}

func fallbackErr(fallback int) int {
    res, err := work()
    if err != nil {
        return fallback
    }
    return res
}
"#;
    let ext = lang::extract_file("err.go", Language::Go, src, "e1".into()).unwrap();
    assert_eq!(ext.ir_funcs.len(), 3);

    let norm_bare = normalize_func(&ext.ir_funcs[0], NormalizeOptions::default());
    let norm_fallback = normalize_func(&ext.ir_funcs[2], NormalizeOptions::default());

    fn has_propagate_err(n: &IrNode) -> bool {
        match n {
            IrNode::PropagateErr(_) => true,
            IrNode::Seq(items) => items.iter().any(has_propagate_err),
            IrNode::Branch { cond, then_br, else_br } => {
                has_propagate_err(cond)
                    || has_propagate_err(then_br)
                    || else_br.as_deref().map(has_propagate_err).unwrap_or(false)
            }
            _ => false,
        }
    }

    assert!(
        has_propagate_err(&norm_bare),
        "bare return in if err != nil {{ return }} should normalize to PropagateErr, got {:?}",
        norm_bare
    );
    assert!(
        !has_propagate_err(&norm_fallback),
        "fallback return in if err != nil {{ return fallback }} must NOT normalize to PropagateErr, got {:?}",
        norm_fallback
    );

    let score = pair_similarity(&ext.ir_funcs[0], &ext.ir_funcs[1], false);
    assert!(
        score >= 0.9,
        "expected high similarity between Go bare error return and explicit error return, got {}",
        score
    );

    // TS/JS early exit (if (err != null) return;) must NOT be treated as Go named-return PropagateErr
    let ts_src = "function earlyExit(err: any) { if (err != null) { return; } work(); }\n";
    let ts_ext = lang::extract_file("exit.ts", Language::TypeScript, ts_src, "t1".into()).unwrap();
    let norm_ts = normalize_func(&ts_ext.ir_funcs[0], NormalizeOptions::default());
    assert!(
        !has_propagate_err(&norm_ts),
        "TS bare return must NOT normalize to PropagateErr, got {:?}",
        norm_ts
    );
}
