use kagami::config::{build_path_filter, load_allowlist, KagamiConfig};
use kagami::graph::blast::{analyze_blast_radius, BlastOptions, Direction};
use kagami::graph::dead::{analyze_dead, DeadOptions, DeadScope};
use kagami::graph::CodeGraph;
use kagami::ir::similar::{find_similar, pair_similarity, SimilarOptions};
use kagami::lang;
use kagami::model::Language;
use kagami::walker::discover_files;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_ir_for_loop_equivalence_go() {
    let src = r#"
package demo

func sumClassic(n int) int {
    total := 0
    for i := 0; i < n; i++ {
        total = total + i
    }
    return total
}

func sumRange(m int) int {
    acc := 0
    for k := range m {
        acc = acc + k
    }
    return acc
}
"#;
    let ext = lang::extract_file("loop.go", Language::Go, src, "h1".into()).unwrap();
    assert_eq!(ext.ir_funcs.len(), 2);
    let score = pair_similarity(&ext.ir_funcs[0], &ext.ir_funcs[1], false);
    assert!(
        (score - 1.0).abs() < 1e-9,
        "expected 1.0 for 'for i := 0; i < n; i++' vs 'for i := range n', got {}",
        score
    );
}

#[test]
fn test_ir_if_else_vs_switch_equivalence_go() {
    let src = r#"
package demo

func classifyIf(x int) int {
    if x == 1 {
        return 10
    } else if x == 2 {
        return 20
    } else {
        return 30
    }
}

func classifySwitch(y int) int {
    switch y {
    case 2:
        return 20
    case 1:
        return 10
    default:
        return 30
    }
}
"#;
    let ext = lang::extract_file("branch.go", Language::Go, src, "h2".into()).unwrap();
    assert_eq!(ext.ir_funcs.len(), 2);
    let score = pair_similarity(&ext.ir_funcs[0], &ext.ir_funcs[1], false);
    assert!(
        (score - 1.0).abs() < 1e-9,
        "expected 1.0 for if-else chain vs switch-case, got {}",
        score
    );
}

#[test]
fn test_ir_cross_lang_loop_go_vs_rust_vs_ts() {
    let go_src = r#"
package demo
func runGo(n int) int {
    s := 0
    for i := 0; i < n; i++ {
        s = s + i
    }
    return s
}
"#;
    let rs_src = r#"
fn run_rs(n: i32) -> i32 {
    let mut s = 0;
    for i in 0..n {
        s = s + i;
    }
    return s;
}
"#;
    let ts_src = r#"
function runTs(n: number): number {
    let s = 0;
    for (let i = 0; i < n; i++) {
        s = s + i;
    }
    return s;
}
"#;
    let go_ext = lang::extract_file("a.go", Language::Go, go_src, "g".into()).unwrap();
    let rs_ext = lang::extract_file("a.rs", Language::Rust, rs_src, "r".into()).unwrap();
    let ts_ext = lang::extract_file("a.ts", Language::TypeScript, ts_src, "t".into()).unwrap();

    let s_go_rs = pair_similarity(&go_ext.ir_funcs[0], &rs_ext.ir_funcs[0], false);
    let s_go_ts = pair_similarity(&go_ext.ir_funcs[0], &ts_ext.ir_funcs[0], false);
    assert!((s_go_rs - 1.0).abs() < 1e-9, "Go vs Rust loop score: {}", s_go_rs);
    assert!((s_go_ts - 1.0).abs() < 1e-9, "Go vs TS loop score: {}", s_go_ts);
}

#[test]
fn test_ir_rust_match_vs_if_else() {
    let src = r#"
fn with_if(code: i32) -> i32 {
    if code == 1 {
        return 100;
    } else if code == 2 {
        return 200;
    } else {
        return 0;
    }
}

fn with_match(v: i32) -> i32 {
    match v {
        2 => return 200,
        1 => return 100,
        _ => return 0,
    }
}
"#;
    let ext = lang::extract_file("m.rs", Language::Rust, src, "m".into()).unwrap();
    let score = pair_similarity(&ext.ir_funcs[0], &ext.ir_funcs[1], false);
    assert!((score - 1.0).abs() < 1e-9, "Rust if-else vs match score: {}", score);
}

#[test]
fn test_cross_language_blast_radius_and_dead_code() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("server")).unwrap();
    fs::write(
        root.join("server/handler.rs"),
        r#"
fn main() {
    emit_turn_completed();
}
fn emit_turn_completed() {
    let method = "turn/completed";
    send(method);
}
fn send(_m: &str) {}
fn totally_unused_helper() {
    let x = 1;
}
fn cycle_a() { cycle_b(); }
fn cycle_b() { cycle_a(); }
"#,
    )
    .unwrap();

    fs::create_dir_all(root.join("tui")).unwrap();
    fs::write(
        root.join("tui/client.go"),
        r#"
package tui

func main() {
    handleNotification()
}

func handleNotification() {
    method := "turn/completed"
    render(method)
}

func render(s string) {}
func deadGoFunc() {}
"#,
    )
    .unwrap();

    fs::create_dir_all(root.join("web/routes")).unwrap();
    fs::write(
        root.join("web/routes/+page.svelte"),
        r#"
<script lang="ts">
function onEvent(m: string) {
    if (m === "turn/completed") {
        refreshView();
    }
}
function refreshView() {}
</script>
<button onclick={onEvent}>Run</button>
"#,
    )
    .unwrap();

    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::write(root.join("scripts/build.sh"), "#!/usr/bin/env bash
echo ok
").unwrap();
    fs::write(
        root.join("justfile"),
        "build:
    ./scripts/build.sh
",
    )
    .unwrap();

    fs::write(root.join(".deadcode-allowlist"), "deadGoFunc
").unwrap();

    let cfg = KagamiConfig::load(root).unwrap();
    let ignores = cfg.build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();

    let report = analyze_blast_radius(
        &graph,
        "server/handler.rs",
        "emit_turn_completed",
        &BlastOptions {
            depth: 2,
            direction: Direction::Both,
            cross_lang: true,
            include_env: false,
            include_tests: true,
        },
    )
    .unwrap();

    assert!(
        report.by_language.contains_key("go"),
        "expected cross-lang impact in Go, got {:?}",
        report.by_language
    );
    assert!(
        report.by_language.contains_key("svelte"),
        "expected cross-lang impact in Svelte, got {:?}",
        report.by_language
    );

    let allow = load_allowlist(root, None);
    let dead = analyze_dead(
        &graph,
        &DeadOptions {
            scope: DeadScope::Prod,
            langs: None,
            allowlist: allow,
            include_clusters: true,
            path_filter: build_path_filter(&[]).unwrap(),
        },
    );

    let dead_names: Vec<&str> = dead.symbols.iter().map(|s| s.symbol.as_str()).collect();
    assert!(dead_names.contains(&"totally_unused_helper"), "expected totally_unused_helper in {:?}", dead_names);
    assert!(dead_names.contains(&"cycle_a"), "expected cycle_a in {:?}", dead_names);
    assert!(dead_names.contains(&"cycle_b"), "expected cycle_b in {:?}", dead_names);
    assert!(!dead_names.contains(&"deadGoFunc"), "allowlisted deadGoFunc should not be reported");
    assert!(!dead_names.contains(&"emit_turn_completed"), "live Rust fn should not be dead");

    let all_funcs: Vec<_> = graph.extracts.iter().flat_map(|e| e.ir_funcs.iter().cloned()).collect();
    let _pairs = find_similar(
        &all_funcs,
        &SimilarOptions {
            min_similarity: 0.85,
            min_nodes: 4,
            cross_lang: false,
            strict_literals: false,
            limit: 50,
        },
    );
}

#[test]
fn test_gitignore_and_ignore_respected() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".gitignore"), "ignored_dir/
").unwrap();
    fs::write(root.join(".ignore"), "secret.go
").unwrap();
    fs::create_dir_all(root.join("ignored_dir")).unwrap();
    fs::write(root.join("ignored_dir/a.go"), "package a
func A() {}
").unwrap();
    fs::write(root.join("secret.go"), "package s
func S() {}
").unwrap();
    fs::write(root.join("keep.go"), "package k
func K() {}
").unwrap();

    let cfg = KagamiConfig::load(root).unwrap();
    let ignores = cfg.build_ignore_globset().unwrap();
    let files = discover_files(root, &ignores, None).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(paths, vec!["keep.go"]);
}
