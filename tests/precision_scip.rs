use kagami::config::KagamiConfig;
use kagami::graph::dead::{analyze_dead, DeadOptions, DeadScope};
use kagami::graph::scip::{apply_scip_overlay, load_scip_file};
use kagami::graph::CodeGraph;
use kagami::model::{Confidence, EdgeKind};
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
