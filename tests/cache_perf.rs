use kagami::cache::{CacheStore, PARSER_VERSION};
use kagami::config::KagamiConfig;
use kagami::graph::CodeGraph;
use kagami::graph::dead::{DeadKinds, DeadOptions, DeadScope, analyze_dead, analyze_dead_snapshot};
use kagami::model::Confidence;
use kagami::walker::discover_files;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn dead_opts(kinds: DeadKinds) -> DeadOptions {
    DeadOptions {
        scope: DeadScope::Prod,
        langs: None,
        allowlist: HashSet::new(),
        include_clusters: true,
        path_filter: None,
        min_confidence: Confidence::Heuristic,
        kinds,
    }
}

fn setup_polyglot_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo_crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn used_helper() -> i32 { 42 }\npub fn unused_helper() -> i32 { 7 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/main.rs"),
        "use demo_crate::used_helper;\nfn main() { let _ = used_helper(); }\n",
    )
    .unwrap();

    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(
        root.join("pkg/app.go"),
        "package main\nfunc main() { liveGo() }\nfunc liveGo() {}\nfunc deadGo() {}\n",
    )
    .unwrap();

    dir
}

fn discover_all(root: &Path) -> Vec<kagami::walker::DiscoveredFile> {
    let cfg = KagamiConfig::load(root).unwrap();
    let ignores = cfg.build_ignore_globset().unwrap();
    discover_files(root, &ignores, None).unwrap()
}

#[test]
fn binary_cache_round_trip_and_legacy_json_cleanup() {
    let dir = setup_polyglot_repo();
    let root = dir.path();
    let files = discover_all(root);

    let kagami_dir = root.join(".kagami");
    fs::create_dir_all(&kagami_dir).unwrap();
    let legacy_json = kagami_dir.join("cache.json");
    fs::write(&legacy_json, b"{\"version\":13,\"files\":{}}").unwrap();
    assert!(legacy_json.exists());

    let uncached = CodeGraph::build(root, &files, false).unwrap();
    let uncached_report = analyze_dead(&uncached, &dead_opts(DeadKinds::All));

    let cached1 = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let store = CacheStore::new(root);
    assert!(store.cache_path().exists());
    assert!(store.manifest_path().exists());
    assert!(store.graph_path().exists());
    assert!(!legacy_json.exists(), "legacy cache.json should be removed");

    let cached2 = CodeGraph::build_for_graph(root, &files, true).unwrap();
    assert!(
        cached2.extracts.is_empty(),
        "graph cache hit should skip loading raw extracts"
    );

    let r1 = serde_json::to_string(&analyze_dead(&cached1, &dead_opts(DeadKinds::All))).unwrap();
    let r2 = serde_json::to_string(&analyze_dead(&cached2, &dead_opts(DeadKinds::All))).unwrap();
    let r0 = serde_json::to_string(&uncached_report).unwrap();
    assert_eq!(r0, r1);
    assert_eq!(r0, r2);

    // Direct snapshot fast-path must match full CodeGraph analysis bit-for-bit
    let snap = CodeGraph::try_load_cached_snapshot(root, &files)
        .expect("cached snapshot should hit after cache population");
    for kinds in [DeadKinds::Callables, DeadKinds::Types, DeadKinds::All] {
        let expected = serde_json::to_string(&analyze_dead(&uncached, &dead_opts(kinds))).unwrap();
        let actual =
            serde_json::to_string(&analyze_dead_snapshot(&snap, &dead_opts(kinds))).unwrap();
        assert_eq!(expected, actual, "snapshot parity mismatch for {kinds:?}");
    }
}

#[test]
fn corrupt_and_stale_version_cache_recovery() {
    let dir = setup_polyglot_repo();
    let root = dir.path();
    let files = discover_all(root);

    let baseline = serde_json::to_string(&analyze_dead(
        &CodeGraph::build(root, &files, false).unwrap(),
        &dead_opts(DeadKinds::All),
    ))
    .unwrap();

    let _ = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let store = CacheStore::new(root);

    fs::write(store.cache_path(), b"NOT_A_VALID_CACHE").unwrap();
    fs::write(store.manifest_path(), b"GARBAGE").unwrap();
    fs::write(store.graph_path(), b"CORRUPT").unwrap();

    let recovered = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let r_corrupt =
        serde_json::to_string(&analyze_dead(&recovered, &dead_opts(DeadKinds::All))).unwrap();
    assert_eq!(baseline, r_corrupt);

    let mut stale_cache = Vec::new();
    stale_cache.extend_from_slice(b"KAGAMI01");
    stale_cache.extend_from_slice(&(PARSER_VERSION - 1).to_le_bytes());
    stale_cache.extend_from_slice(&[0u8; 16]);
    fs::write(store.cache_path(), &stale_cache).unwrap();

    let mut stale_graph = Vec::new();
    stale_graph.extend_from_slice(b"KAGAMIG1");
    stale_graph.extend_from_slice(&(PARSER_VERSION - 1).to_le_bytes());
    stale_graph.extend_from_slice(&[0u8; 16]);
    fs::write(store.graph_path(), &stale_graph).unwrap();

    let recovered_stale = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let r_stale =
        serde_json::to_string(&analyze_dead(&recovered_stale, &dead_opts(DeadKinds::All))).unwrap();
    assert_eq!(baseline, r_stale);

    // 3. Corrupt ONLY graph.bin while manifest.bin stays valid: recovery must repair graph.bin
    // so the subsequent run hits the snapshot cache.
    fs::write(store.graph_path(), b"TRUNCATED").unwrap();
    let recovered_graph_only = CodeGraph::build_for_graph(root, &files, true).unwrap();
    assert_eq!(
        baseline,
        serde_json::to_string(&analyze_dead(
            &recovered_graph_only,
            &dead_opts(DeadKinds::All)
        ))
        .unwrap()
    );
    assert!(
        CodeGraph::try_load_cached_snapshot(root, &files).is_some(),
        "repaired graph.bin should hit snapshot cache on the next run"
    );
}

#[test]
fn unchanged_cache_zero_rewrite() {
    let dir = setup_polyglot_repo();
    let root = dir.path();
    let files = discover_all(root);
    let store = CacheStore::new(root);

    let _ = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let cache_mtime_1 = fs::metadata(store.cache_path())
        .unwrap()
        .modified()
        .unwrap();
    let manifest_mtime_1 = fs::metadata(store.manifest_path())
        .unwrap()
        .modified()
        .unwrap();
    let graph_mtime_1 = fs::metadata(store.graph_path())
        .unwrap()
        .modified()
        .unwrap();

    thread::sleep(Duration::from_millis(25));

    let _ = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let cache_mtime_2 = fs::metadata(store.cache_path())
        .unwrap()
        .modified()
        .unwrap();
    let manifest_mtime_2 = fs::metadata(store.manifest_path())
        .unwrap()
        .modified()
        .unwrap();
    let graph_mtime_2 = fs::metadata(store.graph_path())
        .unwrap()
        .modified()
        .unwrap();

    assert_eq!(
        cache_mtime_1, cache_mtime_2,
        "cache.bin must not be rewritten when unchanged"
    );
    assert_eq!(
        manifest_mtime_1, manifest_mtime_2,
        "manifest.bin must not be rewritten when unchanged"
    );
    assert_eq!(
        graph_mtime_1, graph_mtime_2,
        "graph.bin must not be rewritten when unchanged"
    );

    let _ = CodeGraph::build(root, &files, true).unwrap();
    let cache_mtime_3 = fs::metadata(store.cache_path())
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        cache_mtime_1, cache_mtime_3,
        "CodeGraph::build must not rewrite cache.bin when unchanged"
    );
}

#[test]
fn mtime_fast_path_touch_and_content_change_invalidation() {
    let dir = setup_polyglot_repo();
    let root = dir.path();
    let files = discover_all(root);
    let store = CacheStore::new(root);

    let g1 = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let r1 = analyze_dead(&g1, &dead_opts(DeadKinds::Callables));
    assert!(r1.symbols.iter().any(|s| s.symbol == "deadGo"));

    let graph_mtime_1 = fs::metadata(store.graph_path())
        .unwrap()
        .modified()
        .unwrap();
    thread::sleep(Duration::from_millis(25));

    let go_path = root.join("pkg/app.go");
    let orig_go = fs::read(&go_path).unwrap();
    fs::write(&go_path, &orig_go).unwrap();

    let g2 = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let graph_mtime_2 = fs::metadata(store.graph_path())
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        graph_mtime_1, graph_mtime_2,
        "graph.bin should remain valid when mtime changes but content hash is identical"
    );
    assert_eq!(
        serde_json::to_string(&r1).unwrap(),
        serde_json::to_string(&analyze_dead(&g2, &dead_opts(DeadKinds::Callables))).unwrap()
    );

    thread::sleep(Duration::from_millis(25));
    fs::write(
        &go_path,
        "package main\nfunc main() { liveGo(); deadGo() }\nfunc liveGo() {}\nfunc deadGo() {}\n",
    )
    .unwrap();

    let g3 = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let r3 = analyze_dead(&g3, &dead_opts(DeadKinds::Callables));
    assert!(
        !r3.symbols.iter().any(|s| s.symbol == "deadGo"),
        "deadGo should become live after content edit invalidates cache"
    );

    let uncached = CodeGraph::build(root, &files, false).unwrap();
    assert_eq!(
        serde_json::to_string(&r3).unwrap(),
        serde_json::to_string(&analyze_dead(&uncached, &dead_opts(DeadKinds::Callables))).unwrap()
    );
}

#[test]
fn cargo_toml_change_invalidates_graph_cache() {
    let dir = setup_polyglot_repo();
    let root = dir.path();
    let files = discover_all(root);

    let g1 = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let r1 = analyze_dead(&g1, &dead_opts(DeadKinds::Callables));
    assert!(!r1.symbols.iter().any(|s| s.symbol == "used_helper"));

    thread::sleep(Duration::from_millis(25));
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"renamed_crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let g2 = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let r2 = analyze_dead(&g2, &dead_opts(DeadKinds::Callables));
    let uncached = CodeGraph::build(root, &files, false).unwrap();
    let r_uncached = analyze_dead(&uncached, &dead_opts(DeadKinds::Callables));
    assert_eq!(
        serde_json::to_string(&r2).unwrap(),
        serde_json::to_string(&r_uncached).unwrap()
    );
}

#[test]
fn scoped_build_preserves_whole_repo_cache() {
    let dir = setup_polyglot_repo();
    let root = dir.path();
    let all_files = discover_all(root);
    let store = CacheStore::new(root);

    // Populate whole-repo cache
    let _ = CodeGraph::build_for_graph(root, &all_files, true).unwrap();
    let orig_count = store.load().files.len();
    assert!(orig_count >= 3);

    // Edit pkg/app.go so the scoped build actually rewrites cache.bin
    thread::sleep(Duration::from_millis(25));
    fs::write(
        root.join("pkg/app.go"),
        "package main\nfunc main() { liveGo(); deadGo() }\nfunc liveGo() {}\nfunc deadGo() {}\n",
    )
    .unwrap();

    let go_only: Vec<_> = all_files
        .iter()
        .filter(|f| f.rel_path.ends_with(".go"))
        .cloned()
        .collect();
    let _ = CodeGraph::build_for_scoped_graph(root, &go_only, true).unwrap();

    // Extract cache rewrite must retain out-of-scope Rust extracts
    let reloaded = store.load();
    assert_eq!(reloaded.files.len(), orig_count);
    assert!(reloaded.files.contains_key("src/lib.rs"));
    assert!(reloaded.files.contains_key("src/main.rs"));
}

#[cfg(unix)]
#[test]
fn same_size_preserved_mtime_edit_invalidates_cache() {
    let dir = setup_polyglot_repo();
    let root = dir.path();
    let files = discover_all(root);
    let go_path = root.join("pkg/app.go");

    let g1 = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let r1 = analyze_dead(&g1, &dead_opts(DeadKinds::Callables));
    assert!(r1.symbols.iter().any(|s| s.symbol == "deadGo"));
    assert!(!r1.symbols.iter().any(|s| s.symbol == "liveGo"));

    // Replace with equal-length content that calls deadGo() instead of liveGo(),
    // then restore the original mtime to verify ctime/ino invalidation.
    let orig_mtime = fs::metadata(&go_path).unwrap().modified().unwrap();
    let orig_bytes = fs::read(&go_path).unwrap();
    let replaced = "package main\nfunc main() { deadGo() }\nfunc liveGo() {}\nfunc deadGo() {}\n";
    assert_eq!(orig_bytes.len(), replaced.len());
    thread::sleep(Duration::from_millis(25));
    fs::write(&go_path, replaced).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&go_path)
        .unwrap()
        .set_modified(orig_mtime)
        .unwrap();

    let g2 = CodeGraph::build_for_graph(root, &files, true).unwrap();
    let r2 = analyze_dead(&g2, &dead_opts(DeadKinds::Callables));
    assert!(
        r2.symbols.iter().any(|s| s.symbol == "liveGo"),
        "liveGo should now be dead after equal-size preserved-mtime edit"
    );
    assert!(
        !r2.symbols.iter().any(|s| s.symbol == "deadGo"),
        "deadGo should now be live after equal-size preserved-mtime edit"
    );
}
