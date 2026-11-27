pub mod bash;
pub mod go;
pub mod justfile;
pub mod rust;
pub mod svelte;
pub mod ts_js;

use crate::ir::{IrNode, LitClass, OpClass, UnOpClass};
use crate::model::{
    BridgeHint, BridgeKind, BridgeRole, Confidence, FileExtract, Language, Span, Symbol,
    SymbolKind, Visibility,
};
use anyhow::Result;
use tree_sitter::Node;

pub fn extract_file(rel_path: &str, lang: Language, src: &str, blake3_hex: String) -> Result<FileExtract> {
    match lang {
        Language::Go => go::extract(rel_path, src, blake3_hex),
        Language::Rust => rust::extract(rel_path, src, blake3_hex),
        Language::TypeScript => ts_js::extract(rel_path, src, blake3_hex, Language::TypeScript, 0),
        Language::JavaScript => ts_js::extract(rel_path, src, blake3_hex, Language::JavaScript, 0),
        Language::Svelte => svelte::extract(rel_path, src, blake3_hex),
        Language::Bash => bash::extract(rel_path, src, blake3_hex, 0, None),
        Language::Just => justfile::extract(rel_path, src, blake3_hex),
        Language::JsonSchema => extract_json_schema(rel_path, src, blake3_hex),
    }
}

pub fn make_symbol_id(file: &str, qualified_name: &str, start_line: usize) -> String {
    format!("{}::{}@{}", file, qualified_name, start_line)
}

pub fn node_text<'a>(node: Node<'a>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

pub fn node_span(node: Node<'_>, line_offset: usize) -> Span {
    Span {
        start_line: node.start_position().row + 1 + line_offset,
        end_line: node.end_position().row + 1 + line_offset,
    }
}

pub fn map_binop(op: &str) -> OpClass {
    match op.trim() {
        "+" => OpClass::Add,
        "-" => OpClass::Sub,
        "*" => OpClass::Mul,
        "/" => OpClass::Div,
        "%" => OpClass::Mod,
        "==" | "===" => OpClass::Eq,
        "!=" | "!==" => OpClass::Neq,
        "<" => OpClass::Lt,
        "<=" => OpClass::Le,
        ">" => OpClass::Gt,
        ">=" => OpClass::Ge,
        "&&" => OpClass::And,
        "||" => OpClass::Or,
        "&" => OpClass::BitAnd,
        "|" => OpClass::BitOr,
        "^" => OpClass::BitXor,
        "<<" => OpClass::Shl,
        ">>" => OpClass::Shr,
        _ => OpClass::Other,
    }
}

pub fn map_unop(op: &str) -> UnOpClass {
    match op.trim() {
        "!" | "not" => UnOpClass::Not,
        "-" => UnOpClass::Neg,
        "*" => UnOpClass::Deref,
        "&" => UnOpClass::Ref,
        _ => UnOpClass::Other,
    }
}

pub fn literal_node(raw: &str) -> IrNode {
    IrNode::Lit(LitClass::Exact(raw.trim().to_string()))
}

/// Detect wire/JSON-RPC method literals like "turn/completed", "thread/start", "item/fileChange/..."
pub fn looks_like_wire_method(s: &str) -> bool {
    let inner = s.trim().trim_matches('"').trim_matches('\'').trim_matches('`');
    if inner.len() < 4 || inner.len() > 80 {
        return false;
    }
    if inner.contains(' ') || inner.contains("://") || inner.starts_with('/') || inner.starts_with('.') {
        return false;
    }
    // Must contain '/' separating alphanumeric/camelCase segments, e.g. "turn/completed" or "account/login/start"
    let parts: Vec<&str> = inner.split('/').collect();
    if parts.len() < 2 || parts.len() > 4 {
        return false;
    }
    // Exclude common MIME types / file paths with extensions
    if parts.last().map_or(false, |p| p.contains('.')) {
        return false;
    }
    let first = parts[0];
    if matches!(
        first,
        "application" | "text" | "image" | "audio" | "video" | "multipart" | "font" | "usr" | "etc" | "var" | "tmp" | "bin" | "dev" | "home" | "proc" | "sys"
    ) {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.chars().next().map_or(false, |c| c.is_ascii_alphabetic())
            && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

pub fn strip_quotes(s: &str) -> String {
    s.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('`')
        .to_string()
}

fn extract_json_schema(rel_path: &str, src: &str, blake3_hex: String) -> Result<FileExtract> {
    let mut symbols = Vec::new();
    let mut bridges = Vec::new();
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(src) {
        if let Some(title) = val.get("title").and_then(|v| v.as_str()) {
            let sym_id = make_symbol_id(rel_path, title, 1);
            symbols.push(Symbol {
                id: sym_id.clone(),
                name: title.to_string(),
                qualified_name: title.to_string(),
                kind: SymbolKind::WireContract,
                lang: Language::JsonSchema,
                file: rel_path.to_string(),
                span: Span { start_line: 1, end_line: 1 },
                visibility: Visibility::Public,
                is_test: false,
                is_entrypoint: true,
                is_dynamic_candidate: false,
                package_or_module: "schema".to_string(),
            });
            bridges.push(BridgeHint {
                from_symbol_id: sym_id.clone(),
                bridge_kind: BridgeKind::WireContract,
                key: format!("type:{}", title),
                role: BridgeRole::Producer,
                line: 1,
                confidence: Confidence::Resolved,
            });
            // Collect any const method strings in JSON schema
            collect_json_wire_methods(&val, &sym_id, &mut bridges);
        }
    }
    Ok(FileExtract {
        file: rel_path.to_string(),
        lang: Language::JsonSchema,
        blake3: blake3_hex,
        package_or_module: "schema".to_string(),
        imports: Vec::new(),
        symbols,
        refs: Vec::new(),
        bridges,
        ir_funcs: Vec::new(),
        interface_methods: std::collections::HashMap::new(),
        embedded_types: std::collections::HashMap::new(),
        struct_fields: std::collections::HashMap::new(),
        func_return_types: std::collections::HashMap::new(),
        trait_impls: Vec::new(),
        impl_macros: Vec::new(),
        cfg_gated_symbols: std::collections::HashMap::new(),
        type_aliases: std::collections::HashMap::new(),
        rust_uses: Vec::new(),
        symbol_mod_scopes: std::collections::HashMap::new(),
    })
}

fn collect_json_wire_methods(val: &serde_json::Value, sym_id: &str, out: &mut Vec<BridgeHint>) {
    match val {
        serde_json::Value::String(s) => {
            if looks_like_wire_method(s) {
                out.push(BridgeHint {
                    from_symbol_id: sym_id.to_string(),
                    bridge_kind: BridgeKind::WireContract,
                    key: format!("rpc:{}", s),
                    role: BridgeRole::Producer,
                    line: 1,
                    confidence: Confidence::Resolved,
                });
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_json_wire_methods(item, sym_id, out);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_json_wire_methods(v, sym_id, out);
            }
        }
        _ => {}
    }
}
