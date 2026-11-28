use globset::GlobSet;
use kagami::graph::CodeGraph;
use kagami::graph::dead::{DeadKinds, DeadOptions, DeadScope, analyze_dead};
use kagami::model::{Confidence, SymbolKind};
use kagami::walker::discover_files;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn run_dead(root: &Path, kinds: DeadKinds, scope: DeadScope) -> Vec<(String, String, SymbolKind)> {
    let files = discover_files(root, &GlobSet::empty(), None).unwrap();
    let graph = CodeGraph::build(root, &files, false).unwrap();
    let report = analyze_dead(
        &graph,
        &DeadOptions {
            scope,
            langs: None,
            allowlist: HashSet::new(),
            include_clusters: true,
            path_filter: None,
            min_confidence: Confidence::Heuristic,
            kinds,
        },
    );
    report
        .symbols
        .into_iter()
        .map(|s| (s.file, s.symbol, s.kind))
        .collect()
}

#[test]
fn test_dead_types_consts_vars_go() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("internal/sessionapi")).unwrap();
    fs::create_dir_all(root.join("internal/typespkg")).unwrap();
    fs::create_dir_all(root.join("cmd/app")).unwrap();

    // Modeled on mekugi PR #94: orphaned file with 0 functions, DTO structs with json tags,
    // unused interface, unused type alias, unused consts, and unused var rpcMethods.
    fs::write(
        root.join("internal/sessionapi/hosted.go"),
        r#"package sessionapi

type OrphanDTO struct {
	ID   string `json:"id"`
	Meta OrphanNested `json:"meta"`
}

type OrphanNested struct {
	Label string `json:"label"`
}

type OrphanInterface interface {
	Handle() error
}

type OrphanAlias = map[string]any

const OrphanConst = "never-used"

var rpcMethods = []string{"session.open", "session.close"}

var _ OrphanInterface = (*OrphanDTO)(nil)

func (OrphanDTO) Handle() error { return nil }
"#,
    )
    .unwrap();

    // Cross-package live and dead types/consts/vars
    fs::write(
        root.join("internal/typespkg/types.go"),
        r#"package typespkg

type CrossPkgField struct {
	Val int
}

type CrossPkgStruct struct {
	Field CrossPkgField
}

type CrossPkgEmbedded struct {
	Base string
}

type CrossPkgIface interface {
	Ping()
}

type GenericTarget struct {
	Name string
}

type AliasTarget string

type IotaKind int

const (
	KindA IotaKind = iota
	KindB
)

const CrossPkgConst = "live-const"
const TestOnlyConst = "test-only"

type TestOnlyStruct struct {
	X int
}

var CrossPkgVar = 10
var TestOnlyVar = 20
"#,
    )
    .unwrap();

    // Test-only references must NOT keep prod types/consts/vars alive
    fs::write(
        root.join("internal/typespkg/types_test.go"),
        r#"package typespkg

import "testing"

func TestOnlyRefs(t *testing.T) {
	_ = TestOnlyStruct{X: TestOnlyVar}
	_ = TestOnlyConst
}
"#,
    )
    .unwrap();

    // Live entrypoint referencing types/consts/vars in all supported Go forms
    fs::write(
        root.join("cmd/app/main.go"),
        r#"package main

import (
	"fmt"
	"reflect"
	"example.com/mod/internal/typespkg"
)

type LocalLive struct {
	typespkg.CrossPkgEmbedded
	S typespkg.CrossPkgStruct
}

type LocalConstraint interface {
	~int | ~string
}

type LocalConverted int

type LiveInitConfig struct{}

func initLoad() *LiveInitConfig { return &LiveInitConfig{} }

var _ = (*LiveInitConfig)(initLoad())

const LocalFlag = 4
const LocalMapKey = "k"
var LocalCounter = 0

func genericFn[T LocalConstraint](v T) T { return v }

func useIface(x any) {
	if _, ok := x.(typespkg.CrossPkgIface); ok {
		fmt.Println("ok")
	}
}

func main() {
	_ = reflect.TypeFor[typespkg.GenericTarget]()
	_ = LocalLive{}
	_ = typespkg.AliasTarget("hi")
	_ = LocalConverted(1)
	_ = genericFn[int](1)
	useIface(nil)
	if typespkg.KindB == 1 && typespkg.CrossPkgVar > 0 {
		LocalCounter++
	}
	m := map[string]int{LocalMapKey: LocalFlag}
	fmt.Println(typespkg.CrossPkgConst, m)
}
"#,
    )
    .unwrap();

    // Default Callables mode must NOT report types/consts/vars
    let callables = run_dead(root, DeadKinds::Callables, DeadScope::Prod);
    assert!(
        !callables
            .iter()
            .any(|(_, s, _)| s == "OrphanDTO" || s == "rpcMethods"),
        "default Callables mode must remain backward-compatible: {:?}",
        callables
    );

    // Types mode must flag orphaned and test-only types/consts/vars, and NOT live ones
    let dead_types = run_dead(root, DeadKinds::Types, DeadScope::Prod);
    let has = |sym: &str| dead_types.iter().any(|(_, s, _)| s == sym);
    let kind_of = |sym: &str| {
        dead_types
            .iter()
            .find(|(_, s, _)| s == sym)
            .map(|(_, _, k)| *k)
    };

    assert_eq!(
        kind_of("OrphanDTO"),
        Some(SymbolKind::Struct),
        "OrphanDTO must be dead Struct: {:?}",
        dead_types
    );
    assert_eq!(
        kind_of("OrphanNested"),
        Some(SymbolKind::Struct),
        "OrphanNested must be dead Struct"
    );
    assert_eq!(
        kind_of("OrphanInterface"),
        Some(SymbolKind::Interface),
        "OrphanInterface must be dead Interface"
    );
    assert_eq!(
        kind_of("OrphanAlias"),
        Some(SymbolKind::Type),
        "OrphanAlias must be dead Type"
    );
    assert_eq!(
        kind_of("OrphanConst"),
        Some(SymbolKind::Const),
        "OrphanConst must be dead Const"
    );
    assert_eq!(
        kind_of("rpcMethods"),
        Some(SymbolKind::Var),
        "rpcMethods must be dead Var"
    );
    assert!(!has("_"), "blank identifier _ must never be reported dead");

    assert!(
        has("TestOnlyStruct"),
        "test-only struct must be dead in prod scope"
    );
    assert!(
        has("TestOnlyConst"),
        "test-only const must be dead in prod scope"
    );
    assert!(
        has("TestOnlyVar"),
        "test-only var must be dead in prod scope"
    );

    for live in [
        "CrossPkgField",
        "CrossPkgStruct",
        "CrossPkgEmbedded",
        "CrossPkgIface",
        "GenericTarget",
        "AliasTarget",
        "IotaKind",
        "KindB",
        "CrossPkgConst",
        "CrossPkgVar",
        "LocalLive",
        "LocalConstraint",
        "LocalConverted",
        "LocalFlag",
        "LocalMapKey",
        "LocalCounter",
    ] {
        assert!(
            !has(live),
            "live Go symbol {} should NOT be flagged dead: {:?}",
            live,
            dead_types
        );
    }

    // All mode includes both dead callables (OrphanDTO.Handle) and dead types (OrphanDTO)
    let dead_all = run_dead(root, DeadKinds::All, DeadScope::Prod);
    assert!(dead_all.iter().any(|(_, s, _)| s == "OrphanDTO"));
    assert!(dead_all.iter().any(|(_, s, _)| s == "OrphanDTO.Handle"));
    assert!(
        !dead_all.iter().any(|(_, s, _)| s == "initLoad"),
        "runtime call inside (*LiveInitConfig)(initLoad()) must stay live: {:?}",
        dead_all
    );
}

#[test]
fn test_dead_types_consts_vars_rust() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src/sub")).unwrap();

    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "demo"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    fs::write(
        root.join("src/sub/mod.rs"),
        r#"pub struct ReexportedStruct {
    pub inner: TransitiveField,
}

pub struct TransitiveField {
    pub n: i32,
}

pub enum ScopedEnum {
    Ready,
}

pub const REEXPORTED_CONST: usize = 42;
pub static LIVE_STATIC: &str = "live";

pub struct OrphanStruct {
    pub x: i32,
}

pub enum OrphanEnum {
    Unused,
}

pub trait OrphanTrait {
    fn nope(&self);
}

pub type OrphanAlias = Vec<String>;
pub const ORPHAN_CONST: i32 = 99;
pub static ORPHAN_STATIC: i32 = 100;

pub struct TestOnlyRustStruct;
pub const TEST_ONLY_RUST_CONST: i32 = 7;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_in_test() {
        let _ = TestOnlyRustStruct;
        let _ = TEST_ONLY_RUST_CONST;
    }
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("src/main.rs"),
        r#"mod sub;

pub use sub::ReexportedStruct;
pub use sub::REEXPORTED_CONST;

trait LiveTrait {
    type Output;
    fn run(&self) -> Self::Output;
}

struct AssocOutput;

trait LiveBound {}

struct BoundedImpl;
impl LiveBound for BoundedImpl {}

struct LiveHolder {
    item: ReexportedStruct,
}

impl LiveTrait for LiveHolder {
    type Output = AssocOutput;
    fn run(&self) -> Self::Output {
        AssocOutput
    }
}

enum PatEnum {
    Ok(i32),
}

const MACRO_CONST: &str = "hello";

struct ParentOnlyType;

mod inner_scope {
    pub fn use_parent(_: super::ParentOnlyType) {}
}

fn use_bounded<T: LiveBound>(_v: T) {}

fn main() {
    inner_scope::use_parent(ParentOnlyType);
    let h = LiveHolder {
        item: ReexportedStruct {
            inner: sub::TransitiveField { n: 1 },
        },
    };
    let _ = h.run();
    use_bounded(BoundedImpl);
    let _ = sub::ScopedEnum::Ready;
    let p = PatEnum::Ok(REEXPORTED_CONST as i32);
    if let PatEnum::Ok(v) = p {
        println!("{} {} {}", MACRO_CONST, sub::LIVE_STATIC, v);
    }
}
"#,
    )
    .unwrap();

    let dead_types = run_dead(root, DeadKinds::Types, DeadScope::Prod);
    let has = |sym: &str| dead_types.iter().any(|(_, s, _)| s == sym);

    for dead in [
        "OrphanStruct",
        "OrphanEnum",
        "OrphanTrait",
        "OrphanAlias",
        "ORPHAN_CONST",
        "ORPHAN_STATIC",
        "TestOnlyRustStruct",
        "TEST_ONLY_RUST_CONST",
    ] {
        assert!(
            has(dead),
            "dead Rust symbol {} should be flagged: {:?}",
            dead,
            dead_types
        );
    }

    for live in [
        "ReexportedStruct",
        "TransitiveField",
        "ScopedEnum",
        "REEXPORTED_CONST",
        "LIVE_STATIC",
        "LiveTrait",
        "AssocOutput",
        "LiveHolder::Output",
        "LiveBound",
        "BoundedImpl",
        "LiveHolder",
        "PatEnum",
        "MACRO_CONST",
        "ParentOnlyType",
    ] {
        assert!(
            !has(live),
            "live Rust symbol {} should NOT be flagged dead: {:?}",
            live,
            dead_types
        );
    }
}

#[test]
fn test_dead_types_consts_vars_ts_svelte() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src/routes")).unwrap();
    fs::create_dir_all(root.join("src/lib")).unwrap();

    fs::write(
        root.join("src/lib/types.ts"),
        r#"export interface LiveInterface {
    nested: TransitiveType;
}

export type TransitiveType = { ok: boolean };

export type TypeOnlyImported = string;

export type BarrelStarType = number;

export enum LiveEnum {
    Active = "active",
}

export abstract class BaseService {
    abstract id(): string;
}

export const LIVE_CONST = "live";
export const TYPEOF_CONST = { mode: "fast" } as const;
export let liveCounter = 0;

export interface OrphanInterface {
    dead: string;
}

export type OrphanType = number | null;

export enum OrphanEnum {
    Dead = 0,
}

export class OrphanClass {
    x = 1;
}

export const ORPHAN_CONST = "unused";
export let orphanVar = 123;

export interface TestOnlyInterface {
    t: boolean;
}
export const TEST_ONLY_CONST = "test-only";
"#,
    )
    .unwrap();

    fs::write(
        root.join("src/lib/barrel.ts"),
        r#"export type { TypeOnlyImported } from "./types";
export * from "./types";
"#,
    )
    .unwrap();

    fs::write(
        root.join("src/lib/types.test.ts"),
        r#"import type { TestOnlyInterface } from "./types";
import { TEST_ONLY_CONST } from "./types";

export function check(x: TestOnlyInterface) {
    return x.t ? TEST_ONLY_CONST : "";
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("src/routes/+page.ts"),
        r#"export const prerender = true;
"#,
    )
    .unwrap();

    fs::write(
        root.join("src/routes/+page.svelte"),
        r#"<script lang="ts">
import type { TypeOnlyImported, BarrelStarType } from "../lib/barrel";
import {
    BaseService,
    LiveEnum,
    LIVE_CONST,
    TYPEOF_CONST,
    liveCounter,
    type LiveInterface,
} from "../lib/types";

class LiveService extends BaseService {
    data: LiveInterface | null = null;
    id(): string { return LiveEnum.Active; }
}

type Mode = typeof TYPEOF_CONST;
type SatisfiesTarget = { label: string };

const svc = new LiveService();
const tag: TypeOnlyImported = LIVE_CONST;
const count: BarrelStarType = liveCounter;
const cfg = { label: tag } satisfies SatisfiesTarget;
const modeRef = {} as Mode;
</script>

<div>{svc.id()} {count} {cfg.label} {modeRef.mode}</div>
"#,
    )
    .unwrap();

    let dead_types = run_dead(root, DeadKinds::Types, DeadScope::Prod);
    let has = |sym: &str| dead_types.iter().any(|(_, s, _)| s == sym);

    for dead in [
        "OrphanInterface",
        "OrphanType",
        "OrphanEnum",
        "OrphanClass",
        "ORPHAN_CONST",
        "orphanVar",
        "TestOnlyInterface",
        "TEST_ONLY_CONST",
    ] {
        assert!(
            has(dead),
            "dead TS symbol {} should be flagged: {:?}",
            dead,
            dead_types
        );
    }

    for live in [
        "LiveInterface",
        "TransitiveType",
        "TypeOnlyImported",
        "BarrelStarType",
        "LiveEnum",
        "BaseService",
        "LiveService",
        "LIVE_CONST",
        "TYPEOF_CONST",
        "liveCounter",
        "Mode",
        "SatisfiesTarget",
        "prerender",
    ] {
        assert!(
            !has(live),
            "live TS/Svelte symbol {} should NOT be flagged dead: {:?}",
            live,
            dead_types
        );
    }
}

#[test]
fn test_cli_dead_kinds_flag() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(
        root.join("main.go"),
        r#"package main

type DeadStruct struct { X int }
var deadVar = []string{"a"}
func deadFunc() {}
func main() {}
"#,
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_kagami");
    let run = |kinds: &str| -> serde_json::Value {
        let out = std::process::Command::new(bin)
            .args([
                "--root",
                root.to_str().unwrap(),
                "--no-cache",
                "--format",
                "json",
                "dead",
                "--kinds",
                kinds,
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let names = |v: &serde_json::Value| -> Vec<String> {
        v["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["symbol"].as_str().unwrap().to_string())
            .collect()
    };

    let callables = names(&run("callables"));
    assert_eq!(callables, vec!["deadFunc"]);

    let types = names(&run("types"));
    assert!(types.contains(&"DeadStruct".to_string()));
    assert!(types.contains(&"deadVar".to_string()));
    assert!(!types.contains(&"deadFunc".to_string()));

    let all = names(&run("all"));
    assert!(all.contains(&"DeadStruct".to_string()));
    assert!(all.contains(&"deadVar".to_string()));
    assert!(all.contains(&"deadFunc".to_string()));
}

#[test]
fn test_go_array_type_length_refs() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(
        root.join("go.mod"),
        "module example.com/arrdemo\n\ngo 1.22\n",
    )
    .unwrap();

    fs::write(
        root.join("pkg/sizes.go"),
        r#"package pkg

const CrossPkgN = 3
const UnusedCrossPkgN = 9

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("main.go"),
        r#"package main

import "example.com/arrdemo/pkg"

const SigParamN = 3
const SigRetN = 4
const BodyVarN = 5
const SliceElemN = 6
const ExprN = 2
const StructFieldN = 7
const LenTarget = "abcd"
const MinBound = 4
const max = 64
const TestOnlyN = 8
const UnusedLocalN = 9

type Holder struct {
	buf [StructFieldN]byte
}

func sigRetOnly() (out [SigRetN]string) {
	return out
}

func consumeRows(
	p *[SigParamN]int,
	rows [][SliceElemN]string,
	cross *[pkg.CrossPkgN]int,
	expr *[ExprN + 1]int,
	sized *[len(LenTarget)]byte,
	clamped *[min(MinBound, 2)]byte,
	capped *[max]byte,
) int {
	var parts [BodyVarN]string
	_, _, _, _, _, _ = p, cross, expr, sized, clamped, capped
	return len(rows) + len(parts)
}

func main() {
	var h Holder
	_ = h
	_ = sigRetOnly()
	_ = consumeRows(nil, nil, nil, nil, nil, nil, nil)
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("main_test.go"),
        r#"package main

import "testing"

func TestArrayLengthConst(t *testing.T) {
	var onlyInTest [TestOnlyN]string
	_ = onlyInTest
}
"#,
    )
    .unwrap();

    let dead = run_dead(root, DeadKinds::Types, DeadScope::Prod);
    let has = |name: &str| dead.iter().any(|(_, s, _)| s == name);

    for live in [
        "SigParamN",
        "SigRetN",
        "BodyVarN",
        "SliceElemN",
        "CrossPkgN",
        "ExprN",
        "StructFieldN",
        "LenTarget",
        "MinBound",
        "max",
    ] {
        assert!(
            !has(live),
            "Go constant {} used in array_type length must stay live: {:?}",
            live,
            dead
        );
    }

    assert!(
        has("UnusedLocalN"),
        "unused local Go const must be dead: {:?}",
        dead
    );
    assert!(
        has("UnusedCrossPkgN"),
        "unused cross-package Go const must be dead: {:?}",
        dead
    );
    assert!(
        has("TestOnlyN"),
        "test-only Go const used in [TestOnlyN]T in *_test.go must stay dead: {:?}",
        dead
    );

    let dead_callables = run_dead(root, DeadKinds::Callables, DeadScope::Prod);
    assert!(
        dead_callables.iter().any(|(_, s, _)| s == "min"),
        "predeclared min() in array_type length must not keep unrelated pkg.min alive: {:?}",
        dead_callables
    );
}

#[test]
fn test_rust_array_type_length_refs() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"arrdemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/main.rs"),
        r#"const LIVE_LEN: usize = 4;
const EXPR_LEN: usize = 2;
const UNUSED_LEN: usize = 9;
const TEST_ONLY_LEN: usize = 3;

fn accept(a: [u8; LIVE_LEN], b: [u8; EXPR_LEN + 1]) -> usize {
    a.len() + b.len()
}

fn main() {
    let a: [u8; LIVE_LEN] = [0; 4];
    let b: [u8; EXPR_LEN + 1] = [0; 3];
    let _ = accept(a, b);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uses_test_only() {
        let _arr: [u8; TEST_ONLY_LEN] = [0; TEST_ONLY_LEN];
    }
}
"#,
    )
    .unwrap();

    let dead = run_dead(root, DeadKinds::Types, DeadScope::Prod);
    let has = |name: &str| dead.iter().any(|(_, s, _)| s == name);

    assert!(!has("LIVE_LEN"), "LIVE_LEN must stay live: {:?}", dead);
    assert!(!has("EXPR_LEN"), "EXPR_LEN must stay live: {:?}", dead);
    assert!(has("UNUSED_LEN"), "UNUSED_LEN must be dead: {:?}", dead);
    assert!(
        has("TEST_ONLY_LEN"),
        "TEST_ONLY_LEN used only in #[cfg(test)] must be dead: {:?}",
        dead
    );
}
