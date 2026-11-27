use kagami::config::KagamiConfig;
use kagami::graph::dead::{analyze_dead, DeadOptions, DeadScope};
use kagami::graph::scip::{apply_scip_overlay, load_scip_file};
use kagami::graph::CodeGraph;
use kagami::model::Confidence;
use kagami::walker::discover_files;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_go_package_qualified_and_cobra_callbacks() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("cmd/app")).unwrap();
    fs::create_dir_all(root.join("internal/launch")).unwrap();
    fs::write(
        root.join("cmd/app/main.go"),
        "package main\n\nimport \"proj/internal/launch\"\n\nfunc main() {\n    launch.Run()\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("internal/launch/run.go"),
        "package launch\n\ntype Command struct {\n    RunE func() error\n}\n\nfunc Run() {\n    _ = Command{RunE: runWorker}\n    register(onHook)\n}\n\nfunc runWorker() error { return nil }\nfunc onHook() {}\nfunc register(fn func()) { fn() }\nfunc trulyDead() {}\n",
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
}

#[test]
fn test_scip_overlay_upgrades_edges_to_exact() {
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
    let updated = apply_scip_overlay(&mut graph, &idx);
    assert!(updated >= 1, "expected SCIP overlay to upgrade at least 1 edge");
    assert!(
        graph.edges.iter().any(|e| e.confidence == Confidence::Exact),
        "expected an Exact confidence edge after SCIP overlay"
    );
}
