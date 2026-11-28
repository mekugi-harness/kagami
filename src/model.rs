use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::ir::IrFunc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Go,
    Rust,
    TypeScript,
    JavaScript,
    Svelte,
    Just,
    Bash,
    JsonSchema,
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Go => "go",
            Language::Rust => "rust",
            Language::TypeScript => "ts",
            Language::JavaScript => "js",
            Language::Svelte => "svelte",
            Language::Just => "just",
            Language::Bash => "sh",
            Language::JsonSchema => "json-schema",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "go" => Some(Language::Go),
            "rs" | "rust" => Some(Language::Rust),
            "ts" | "tsx" | "typescript" => Some(Language::TypeScript),
            "js" | "jsx" | "mjs" | "cjs" | "javascript" => Some(Language::JavaScript),
            "svelte" => Some(Language::Svelte),
            "just" | "justfile" => Some(Language::Just),
            "sh" | "bash" | "shell" => Some(Language::Bash),
            "json" | "json-schema" => Some(Language::JsonSchema),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Type,
    Interface,
    Struct,
    Enum,
    Trait,
    Const,
    Var,
    Recipe,
    Component,
    Script,
    WireContract,
    ExecTarget,
    EnvKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Public,
    Private,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub lang: Language,
    pub file: String,
    pub span: Span,
    pub visibility: Visibility,
    pub is_test: bool,
    pub is_entrypoint: bool,
    pub is_dynamic_candidate: bool,
    pub package_or_module: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Calls,
    Imports,
    TypeRef,
    Implements,
    TemplateRef,
    WireRpc,
    CliExec,
    EnvRef,
    RecipeDep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Heuristic,
    Resolved,
    Exact,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReceiverStep {
    /// Starting named type in the caller file's scope (e.g. "Main", "pkg.Main", or "[]Item").
    Type(String),
    /// Starting function call or type conversion return value in the caller file's scope.
    Call {
        qualifier: Option<String>,
        name: String,
        result_index: usize,
    },
    /// Struct field (or embedded field) access on the current type.
    Field(String),
    /// Method call on the current type returning result at result_index.
    MethodCall {
        name: String,
        result_index: usize,
    },
    /// Element access via indexing (x[i]) or range iteration on a slice/array/map.
    Elem,
    /// Tuple element projection (e.g. `let (a, b) = pair;` or `if let Some((_, setup)) = opt`).
    TupleElem(usize),
    /// Callback parameter type inferred from a callee's Fn/FnMut/FnOnce parameter signature.
    /// When used as the first step, resolves a free or scoped function call; when used after
    /// another step, resolves a method call on the current receiver type.
    ClosureParam {
        qualifier: Option<String>,
        name: String,
        arg_index: usize,
        param_index: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawRef {
    pub from_symbol_id: String,
    pub target_name: String,
    pub qualifier: Option<String>,
    pub kind: EdgeKind,
    pub line: usize,
    /// Optional structured receiver type/field chain for precise method resolution (Go).
    #[serde(default)]
    pub receiver_chain: Option<Vec<ReceiverStep>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeHint {
    pub from_symbol_id: String,
    pub bridge_kind: BridgeKind,
    pub key: String,
    pub role: BridgeRole,
    pub line: usize,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeKind {
    WireContract,
    ExecTarget,
    EnvKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeRole {
    Producer,
    Consumer,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraitImplInfo {
    pub type_name: String,
    pub trait_name: String,
    pub trait_path: String,
    #[serde(default)]
    pub trait_type_args: Vec<String>,
    pub method_ids: Vec<String>,
    pub is_test: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustUseItem {
    /// Local binding name (or "*" for glob use).
    pub local_name: String,
    /// Target path (e.g. "force_terminate_process", "auth::connect", "crate::foo::bar").
    pub target_path: String,
    /// Inline module path where this `use` item was declared (empty for file root).
    #[serde(default)]
    pub mod_scope: String,
    #[serde(default)]
    pub is_pub: bool,
    #[serde(default)]
    pub is_test: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustModPath {
    #[serde(default)]
    pub mod_scope: String,
    pub mod_name: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub is_test: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileExtract {
    pub file: String,
    pub lang: Language,
    pub blake3: String,
    pub package_or_module: String,
    #[serde(default)]
    pub is_test_file: bool,
    pub imports: Vec<String>,
    pub symbols: Vec<Symbol>,
    pub refs: Vec<RawRef>,
    pub bridges: Vec<BridgeHint>,
    pub ir_funcs: Vec<IrFunc>,
    /// Interface symbol ID -> declared method names (e.g. Go interface methods).
    #[serde(default)]
    pub interface_methods: HashMap<String, Vec<String>>,
    /// Struct or interface symbol ID -> embedded type names (e.g. Go embedded fields/interfaces).
    #[serde(default)]
    pub embedded_types: HashMap<String, Vec<String>>,
    /// Struct symbol ID -> field name -> field type name (including embedded fields where
    /// field name is the unqualified base type name; slice/array/map element types prefixed with "[]").
    #[serde(default)]
    pub struct_fields: HashMap<String, HashMap<String, String>>,
    /// Function or method symbol ID -> return type names by result position (empty string if unknown/builtin).
    #[serde(default)]
    pub func_return_types: HashMap<String, Vec<String>>,
    /// Function or method symbol ID -> callback parameter types:
    /// Vec of (arg_index_excluding_self, closure_param_index, normalized_type_name).
    #[serde(default)]
    pub func_callback_params: HashMap<String, Vec<(usize, usize, String)>>,
    /// Explicit trait implementations (e.g. Rust impl Trait for Type).
    #[serde(default)]
    pub trait_impls: Vec<TraitImplInfo>,
    /// Macro invocations inside impl Type blocks: (impl_type_name, macro_name, line).
    #[serde(default)]
    pub impl_macros: Vec<(String, String, usize)>,
    /// Non-test #[cfg(...)]-gated symbol ID -> inline module scope path for sibling propagation.
    #[serde(default)]
    pub cfg_gated_symbols: HashMap<String, String>,
    /// Type alias symbol ID -> normalized target type name (e.g. Rust `type Alias = Arc<Target>`).
    #[serde(default)]
    pub type_aliases: HashMap<String, String>,
    /// Structured Rust `use` items (aliases, imports, and `pub use` re-exports).
    #[serde(default)]
    pub rust_uses: Vec<RustUseItem>,
    /// Rust external `mod name;` declarations (with optional `#[path = "..."]` override).
    #[serde(default)]
    pub rust_mod_paths: Vec<RustModPath>,
    /// Rust `macro_rules! foo` definitions -> literal fn/const/static item names generated in their body.
    #[serde(default)]
    pub rust_macro_defs: HashMap<String, Vec<String>>,
    /// Top-level Rust macro invocations: (invocation_symbol_id, macro_name_with_bang).
    #[serde(default)]
    pub rust_top_level_macros: Vec<(String, String)>,
    /// Symbol ID -> inline module path where the symbol was defined (non-empty only inside inline `mod`).
    #[serde(default)]
    pub symbol_mod_scopes: HashMap<String, String>,
    /// Resolved relative file paths re-exported via TS/JS `export * from '...'` barrels.
    #[serde(default)]
    pub ts_star_reexports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub confidence: Confidence,
    pub file: String,
    pub line: usize,
    pub cross_lang: bool,
    pub via_bridge: Option<String>,
}
