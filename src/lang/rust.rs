use crate::ir::{IrFunc, IrNode, LitClass, LoopKind, OpClass};
use crate::lang::{
    literal_node, looks_like_wire_method, make_symbol_id, map_binop, map_unop, node_span,
    node_text, strip_quotes,
};
use crate::model::{
    BridgeHint, BridgeKind, BridgeRole, Confidence, EdgeKind, FileExtract, Language, RawRef,
    ReceiverStep, RustModPath, RustUseItem, Symbol, SymbolKind, TraitImplInfo, Visibility,
};
use anyhow::Result;
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

struct RustExtractCtx<'a> {
    src: &'a str,
    rel_path: &'a str,
    pkg: &'a str,
    imports: Vec<String>,
    symbols: Vec<Symbol>,
    refs: Vec<RawRef>,
    bridges: Vec<BridgeHint>,
    ir_funcs: Vec<IrFunc>,
    interface_methods: HashMap<String, Vec<String>>,
    embedded_types: HashMap<String, Vec<String>>,
    struct_fields: HashMap<String, HashMap<String, String>>,
    func_return_types: HashMap<String, Vec<String>>,
    func_callback_params: HashMap<String, Vec<(usize, usize, String)>>,
    trait_impls: Vec<TraitImplInfo>,
    impl_macros: Vec<(String, String, usize)>,
    cfg_gated_symbols: HashMap<String, String>,
    type_aliases: HashMap<String, String>,
    rust_uses: Vec<RustUseItem>,
    rust_mod_paths: Vec<RustModPath>,
    rust_macro_defs: HashMap<String, Vec<String>>,
    rust_top_level_macros: Vec<(String, String)>,
    symbol_mod_scopes: HashMap<String, String>,
}

/// Lexical scope stack for Rust local variables and inferred receiver chains.
#[derive(Clone)]
struct RustLocalEnv {
    scopes: Vec<HashMap<String, Option<Vec<ReceiverStep>>>>,
}

impl RustLocalEnv {
    fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    fn bind(&mut self, name: String, chain: Option<Vec<ReceiverStep>>) {
        if name.is_empty() || name == "_" {
            return;
        }
        if let Some(top) = self.scopes.last_mut() {
            top.insert(name, chain);
        }
    }

    fn is_shadowed(&self, name: &str) -> bool {
        self.scopes.iter().rev().any(|s| s.contains_key(name))
    }

    fn lookup_chain(&self, name: &str) -> Option<Vec<ReceiverStep>> {
        for s in self.scopes.iter().rev() {
            if let Some(entry) = s.get(name) {
                return entry.clone();
            }
        }
        None
    }
}

pub fn extract(rel_path: &str, src: &str, blake3_hex: String) -> Result<FileExtract> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("failed to parse Rust file {}", rel_path))?;

    let is_test_file = rel_path.starts_with("tests/")
        || rel_path.contains("/tests/")
        || rel_path.starts_with("benches/")
        || rel_path.contains("/benches/")
        || rel_path.ends_with("_test.rs")
        || rel_path.ends_with("_tests.rs")
        || rel_path.ends_with("tests.rs")
        || has_inner_test_attr(tree.root_node(), src);
    let pkg = rel_path.split('/').take(2).collect::<Vec<_>>().join("/");

    let mut ctx = RustExtractCtx {
        src,
        rel_path,
        pkg: &pkg,
        imports: Vec::new(),
        symbols: Vec::new(),
        refs: Vec::new(),
        bridges: Vec::new(),
        ir_funcs: Vec::new(),
        interface_methods: HashMap::new(),
        embedded_types: HashMap::new(),
        struct_fields: HashMap::new(),
        func_return_types: HashMap::new(),
        func_callback_params: HashMap::new(),
        trait_impls: Vec::new(),
        impl_macros: Vec::new(),
        cfg_gated_symbols: HashMap::new(),
        type_aliases: HashMap::new(),
        rust_uses: Vec::new(),
        rust_mod_paths: Vec::new(),
        rust_macro_defs: HashMap::new(),
        rust_top_level_macros: Vec::new(),
        symbol_mod_scopes: HashMap::new(),
    };

    let mut top_methods = Vec::new();
    walk_rust_items(
        tree.root_node(),
        &mut ctx,
        None,
        "",
        is_test_file,
        false,
        false,
        &mut top_methods,
    );

    Ok(FileExtract {
        file: rel_path.to_string(),
        lang: Language::Rust,
        blake3: blake3_hex,
        is_test_file,
        imports: ctx.imports,
        symbols: ctx.symbols,
        refs: ctx.refs,
        bridges: ctx.bridges,
        ir_funcs: ctx.ir_funcs,
        interface_methods: ctx.interface_methods,
        embedded_types: ctx.embedded_types,
        struct_fields: ctx.struct_fields,
        func_return_types: ctx.func_return_types,
        func_callback_params: ctx.func_callback_params,
        trait_impls: ctx.trait_impls,
        impl_macros: ctx.impl_macros,
        cfg_gated_symbols: ctx.cfg_gated_symbols,
        type_aliases: ctx.type_aliases,
        rust_uses: ctx.rust_uses,
        rust_mod_paths: ctx.rust_mod_paths,
        rust_macro_defs: ctx.rust_macro_defs,
        rust_top_level_macros: ctx.rust_top_level_macros,
        symbol_mod_scopes: ctx.symbol_mod_scopes,
        ts_star_reexports: Vec::new(),
        package_or_module: pkg,
    })
}

fn has_inner_test_attr(parent: Node<'_>, src: &str) -> bool {
    let mut c = parent.walk();
    for ch in parent.children(&mut c) {
        if ch.kind() == "inner_attribute_item" && attr_is_test(node_text(ch, src)) {
            return true;
        }
    }
    false
}

fn attr_path(attr: &str) -> &str {
    let s = attr.trim();
    let inner = s
        .strip_prefix("#![")
        .or_else(|| s.strip_prefix("#["))
        .unwrap_or(s)
        .trim_start();
    let end = inner
        .find(|c: char| c.is_ascii_whitespace() || matches!(c, '(' | '=' | ']'))
        .unwrap_or(inner.len());
    inner[..end].trim()
}

fn attr_is_test(attr: &str) -> bool {
    let path = attr_path(attr);
    let last = path.rsplit("::").next().unwrap_or(path);
    if matches!(
        last,
        "test" | "bench" | "rstest" | "test_case" | "traced_test"
    ) || path == "criterion"
        || path.starts_with("criterion::")
    {
        return true;
    }
    if (path == "cfg" || path.ends_with("::cfg"))
        && let Some(idx) = attr.find("cfg(")
    {
        let rest = &attr[idx + 4..];
        if let Some(pred) = extract_balanced_parens(rest) {
            // Strictly test-only iff the predicate CANNOT hold when `test` is false.
            return !cfg_may_hold_without_test(pred);
        }
    }
    false
}

fn extract_balanced_parens(s: &str) -> Option<&str> {
    let mut depth = 1usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in s.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_cfg_args(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut start = 0usize;
    for (i, ch) in s.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let p = s[start..i].trim();
                if !p.is_empty() {
                    parts.push(p);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

/// Three-valued evaluation of a Rust `cfg(...)` predicate with `test = false`:
/// returns `(may_be_true, may_be_false)` when `test` is false and all other flags are unknown.
fn eval_cfg_without_test(pred: &str) -> (bool, bool) {
    let p = pred.trim().trim_end_matches(',').trim();
    if p == "test" {
        return (false, true);
    }
    if let Some(rest) = p.strip_prefix("not")
        && let Some(inner) = rest.trim_start().strip_prefix('(')
        && let Some(arg) = extract_balanced_parens(inner)
    {
        let (t, f) = eval_cfg_without_test(arg);
        return (f, t);
    }
    if let Some(rest) = p.strip_prefix("all")
        && let Some(inner) = rest.trim_start().strip_prefix('(')
        && let Some(args_str) = extract_balanced_parens(inner)
    {
        let mut may_true = true;
        let mut may_false = false;
        for arg in split_top_level_cfg_args(args_str) {
            let (t, f) = eval_cfg_without_test(arg);
            may_true &= t;
            may_false |= f;
        }
        return (may_true, may_false);
    }
    if let Some(rest) = p.strip_prefix("any")
        && let Some(inner) = rest.trim_start().strip_prefix('(')
        && let Some(args_str) = extract_balanced_parens(inner)
    {
        let mut may_true = false;
        let mut may_false = true;
        for arg in split_top_level_cfg_args(args_str) {
            let (t, f) = eval_cfg_without_test(arg);
            may_true |= t;
            may_false &= f;
        }
        return (may_true, may_false);
    }
    // Any other predicate (feature = "...", unix, windows, etc.) can be either true or false.
    (true, true)
}

fn cfg_may_hold_without_test(pred: &str) -> bool {
    eval_cfg_without_test(pred).0
}

fn attr_is_non_test_cfg(attr: &str) -> bool {
    attr.contains("cfg(") && !attr_is_test(attr)
}

fn attr_is_export(attr: &str) -> bool {
    attr.contains("no_mangle")
        || attr.contains("export_name")
        || attr.contains("proc_macro")
        || attr.contains("wasm_bindgen")
        || attr.contains("napi")
        || attr.contains("pyfunction")
        || attr.contains("pymethods")
        || attr.contains("ctor::ctor")
        || attr.contains("#[ctor]")
        || attr.contains("dtor::dtor")
        || attr.contains("#[dtor]")
        || attr.contains("distributed_slice")
}

fn extract_rust_attr_path_overrides(attrs: &[Node<'_>], src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for &attr_item in attrs {
        let mut c = attr_item.walk();
        for ch in attr_item.children(&mut c) {
            if ch.kind() != "attribute" {
                continue;
            }
            let mut ac = ch.walk();
            let mut attr_name = None;
            let mut value_str = None;
            let mut args_tree = None;
            for ach in ch.children(&mut ac) {
                if attr_name.is_none() && matches!(ach.kind(), "identifier" | "scoped_identifier") {
                    attr_name = Some(node_text(ach, src).trim());
                } else if matches!(ach.kind(), "string_literal" | "raw_string_literal") {
                    value_str = Some(strip_quotes(node_text(ach, src)));
                } else if ach.kind() == "token_tree" {
                    args_tree = Some(ach);
                }
            }
            if attr_name == Some("path")
                && let Some(p) = value_str
                && !p.is_empty()
                && !out.contains(&p)
            {
                out.push(p);
            }
            if attr_name == Some("cfg_attr")
                && let Some(tree) = args_tree
            {
                for entry in split_token_tree_comma_entries(tree).into_iter().skip(1) {
                    if entry.len() >= 3
                        && node_text(entry[0], src) == "path"
                        && entry[1].kind() == "="
                        && matches!(entry[2].kind(), "string_literal" | "raw_string_literal")
                    {
                        let p = strip_quotes(node_text(entry[2], src));
                        if !p.is_empty() && !out.contains(&p) {
                            out.push(p);
                        }
                    }
                }
            }
        }
    }
    out
}

fn split_token_tree_comma_entries<'a>(tree: Node<'a>) -> Vec<Vec<Node<'a>>> {
    let mut raw: Vec<Node<'a>> = Vec::new();
    let mut c = tree.walk();
    for ch in tree.children(&mut c) {
        if matches!(ch.kind(), "line_comment" | "block_comment") {
            continue;
        }
        raw.push(ch);
    }
    // Strip outer delimiters '('..')', '['..']', or '{'..'}' of the token_tree.
    let inner = if raw.len() >= 2
        && matches!(
            (raw.first().map(|n| n.kind()), raw.last().map(|n| n.kind())),
            (Some("("), Some(")")) | (Some("["), Some("]")) | (Some("{"), Some("}"))
        ) {
        &raw[1..raw.len() - 1]
    } else {
        &raw[..]
    };

    let mut entries = Vec::new();
    let mut curr = Vec::new();
    let mut angle_depth = 0usize;
    for &tok in inner {
        match tok.kind() {
            "<" => {
                angle_depth += 1;
                curr.push(tok);
            }
            ">" => {
                angle_depth = angle_depth.saturating_sub(1);
                curr.push(tok);
            }
            "," if angle_depth == 0 => {
                if !curr.is_empty() {
                    entries.push(std::mem::take(&mut curr));
                }
            }
            _ => curr.push(tok),
        }
    }
    if !curr.is_empty() {
        entries.push(curr);
    }
    entries
}

fn emit_path_generic_type_args(
    path: &str,
    from_id: &str,
    impl_type: Option<&str>,
    line: usize,
    refs: &mut Vec<RawRef>,
) {
    let Some(lt) = path.find('<') else {
        return;
    };
    let Some(gt) = path.rfind('>') else {
        return;
    };
    if gt <= lt + 1 {
        return;
    }
    let inside = &path[lt + 1..gt];
    for arg in split_top_level_rust_type_list(inside) {
        let trimmed = arg.trim();
        if trimmed.is_empty() || trimmed.starts_with('\'') {
            continue;
        }
        if let Some((lhs, rhs)) = trimmed.split_once(" as ") {
            for side in [lhs.trim(), rhs.trim()] {
                let clean = strip_rust_type_prefixes(side);
                if !clean.is_empty() {
                    emit_scoped_type_ref(clean, from_id, impl_type, line, refs);
                    emit_path_generic_type_args(clean, from_id, impl_type, line, refs);
                }
            }
            continue;
        }
        let clean = strip_rust_type_prefixes(trimmed);
        if !clean.is_empty() {
            emit_scoped_type_ref(clean, from_id, impl_type, line, refs);
            emit_path_generic_type_args(clean, from_id, impl_type, line, refs);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_rust_attr_args_refs(
    attr_name: &str,
    args_tree: Node<'_>,
    src: &str,
    from_id: &str,
    impl_type: Option<&str>,
    env: &RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let entries = split_token_tree_comma_entries(args_tree);
    match attr_name {
        "cfg_attr" => {
            // #[cfg_attr(predicate, attr1(...), attr2(...))]
            for entry in entries.into_iter().skip(1) {
                if let Some((&last, prefix)) = entry.split_last()
                    && last.kind() == "token_tree"
                    && !prefix.is_empty()
                {
                    let inner_name = prefix
                        .iter()
                        .rev()
                        .find(|n| n.kind() == "identifier")
                        .map(|n| node_text(*n, src).trim())
                        .unwrap_or("");
                    if !inner_name.is_empty() {
                        extract_rust_attr_args_refs(
                            inner_name, last, src, from_id, impl_type, env, refs, bridges,
                        );
                    }
                }
            }
        }
        "serde" => {
            for entry in entries {
                if entry.len() >= 3
                    && entry[1].kind() == "="
                    && matches!(entry[2].kind(), "string_literal" | "raw_string_literal")
                {
                    let key = node_text(entry[0], src);
                    let line = entry[2].start_position().row + 1;
                    let path = strip_quotes(node_text(entry[2], src));
                    if path.is_empty() {
                        continue;
                    }
                    match key {
                        "with" => {
                            let mod_name = path.rsplit("::").next().unwrap_or(&path).trim();
                            if !mod_name.is_empty() {
                                for m in ["serialize", "deserialize"] {
                                    refs.push(RawRef {
                                        from_symbol_id: from_id.to_string(),
                                        target_name: m.to_string(),
                                        qualifier: Some(mod_name.to_string()),
                                        kind: EdgeKind::Calls,
                                        line,
                                        receiver_chain: None,
                                    });
                                }
                            }
                        }
                        "default"
                        | "serialize_with"
                        | "deserialize_with"
                        | "skip_serializing_if" => {
                            emit_scoped_call_ref(from_id, &path, impl_type, line, refs);
                            emit_path_generic_type_args(&path, from_id, impl_type, line, refs);
                        }
                        "from" | "try_from" | "into" => {
                            emit_scoped_type_ref(&path, from_id, impl_type, line, refs);
                            emit_path_generic_type_args(&path, from_id, impl_type, line, refs);
                        }
                        _ => {}
                    }
                }
            }
        }
        "schemars" => {
            for entry in entries {
                if entry.len() >= 3 && entry[1].kind() == "=" {
                    let key = node_text(entry[0], src);
                    let line = entry[2].start_position().row + 1;
                    if matches!(entry[2].kind(), "string_literal" | "raw_string_literal") {
                        let path = strip_quotes(node_text(entry[2], src));
                        if path.is_empty() {
                            continue;
                        }
                        match key {
                            "schema_with" | "transform" => {
                                emit_scoped_call_ref(from_id, &path, impl_type, line, refs);
                                emit_path_generic_type_args(&path, from_id, impl_type, line, refs);
                            }
                            "with" => {
                                emit_scoped_type_ref(&path, from_id, impl_type, line, refs);
                                emit_path_generic_type_args(&path, from_id, impl_type, line, refs);
                            }
                            _ => {}
                        }
                    } else if key == "transform" {
                        scan_rust_token_slice(
                            &entry[2..],
                            src,
                            from_id,
                            impl_type,
                            env,
                            refs,
                            bridges,
                        );
                    }
                }
            }
        }
        "arg" | "command" | "clap" | "structopt" => {
            for entry in entries {
                if entry.len() >= 3 && entry[1].kind() == "=" {
                    let key = node_text(entry[0], src);
                    if matches!(
                        key,
                        "value_parser"
                            | "default_value_t"
                            | "default_value_os_t"
                            | "default_values_t"
                            | "default_value"
                            | "default_missing_value_t"
                            | "default_missing_values_t"
                            | "value_hint"
                            | "about"
                            | "long_about"
                            | "before_help"
                            | "after_help"
                            | "before_long_help"
                            | "after_long_help"
                            | "version"
                            | "author"
                    ) {
                        scan_rust_token_slice(
                            &entry[2..],
                            src,
                            from_id,
                            impl_type,
                            env,
                            refs,
                            bridges,
                        );
                    }
                } else if entry.len() >= 2 && entry[1].kind() == "token_tree" {
                    let key = node_text(entry[0], src);
                    if matches!(key, "defer" | "value_parser") {
                        scan_rust_token_tree(entry[1], src, from_id, impl_type, env, refs, bridges);
                    }
                }
            }
        }
        "instrument" => {
            for entry in entries {
                if entry.len() >= 2
                    && node_text(entry[0], src) == "fields"
                    && entry[1].kind() == "token_tree"
                {
                    for fentry in split_token_tree_comma_entries(entry[1]) {
                        if let Some(eq_idx) = fentry.iter().position(|n| n.kind() == "=") {
                            scan_rust_token_slice(
                                &fentry[eq_idx + 1..],
                                src,
                                from_id,
                                impl_type,
                                env,
                                refs,
                                bridges,
                            );
                        }
                    }
                } else if entry.len() >= 3
                    && node_text(entry[0], src) == "target"
                    && entry[1].kind() == "="
                {
                    scan_rust_token_slice(&entry[2..], src, from_id, impl_type, env, refs, bridges);
                }
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_rust_attr_node_refs(
    attr_item: Node<'_>,
    src: &str,
    from_id: &str,
    impl_type: Option<&str>,
    env: &RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let mut c = attr_item.walk();
    for ch in attr_item.children(&mut c) {
        if ch.kind() != "attribute" {
            continue;
        }
        let mut ac = ch.walk();
        let mut attr_path = None;
        let mut args_tree = None;
        for ach in ch.children(&mut ac) {
            if attr_path.is_none() && matches!(ach.kind(), "identifier" | "scoped_identifier") {
                attr_path = Some(node_text(ach, src).trim());
            } else if ach.kind() == "token_tree" {
                args_tree = Some(ach);
            }
        }
        if let (Some(path), Some(tree)) = (attr_path, args_tree) {
            let attr_name = path.rsplit("::").next().unwrap_or(path).trim();
            extract_rust_attr_args_refs(
                attr_name, tree, src, from_id, impl_type, env, refs, bridges,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_rust_attr_nodes_refs(
    attrs: &[Node<'_>],
    src: &str,
    from_id: &str,
    impl_type: Option<&str>,
    env: &RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    for &attr_item in attrs {
        extract_rust_attr_node_refs(attr_item, src, from_id, impl_type, env, refs, bridges);
    }
}

fn strip_balanced_generics(path: &str) -> String {
    let trimmed = path.trim();
    // Handle `<Type as Trait>::method` -> keep `Type::method`
    let mut out = String::with_capacity(trimmed.len());
    if let Some(rest) = trimmed.strip_prefix('<') {
        let mut depth = 1usize;
        let mut end_idx = None;
        for (i, ch) in rest.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        end_idx = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(end) = end_idx {
            let inside = &rest[..end];
            let after = &rest[end + 1..];
            let target_part = inside.split(" as ").next().unwrap_or(inside).trim();
            let base = extract_rust_base_type_name(target_part);
            if !base.is_empty() {
                out.push_str(&base);
            }
            out.push_str(&strip_balanced_generics(after));
            return out;
        }
    }
    let mut depth = 0usize;
    for ch in trimmed.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

fn emit_scoped_call_ref(
    from_id: &str,
    path: &str,
    impl_type: Option<&str>,
    line: usize,
    refs: &mut Vec<RawRef>,
) {
    if path.contains('<') {
        emit_path_generic_type_args(path, from_id, impl_type, line, refs);
    }
    let clean = strip_balanced_generics(path);
    let parts: Vec<&str> = clean.split("::").filter(|p| !p.is_empty()).collect();
    let Some(&last) = parts.last() else {
        return;
    };
    let prefix_parts: Vec<&str> = parts[..parts.len() - 1]
        .iter()
        .copied()
        .filter(|p| !matches!(*p, "crate" | "super" | "self"))
        .collect();
    let qual = if prefix_parts.is_empty() {
        None
    } else if prefix_parts.last() == Some(&"Self") {
        impl_type.map(|t| t.to_string())
    } else {
        Some(prefix_parts.join("::"))
    };
    if let Some(&type_seg) = prefix_parts.last()
        && type_seg != "Self"
        && type_seg
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
        && !is_rust_builtin_type(type_seg)
    {
        let type_qual = if prefix_parts.len() >= 2 {
            Some(prefix_parts[..prefix_parts.len() - 1].join("::"))
        } else {
            None
        };
        refs.push(RawRef {
            from_symbol_id: from_id.to_string(),
            target_name: type_seg.to_string(),
            qualifier: type_qual,
            kind: EdgeKind::TypeRef,
            line,
            receiver_chain: None,
        });
    } else if prefix_parts.last() == Some(&"Self")
        && let Some(t) = impl_type
        && !is_rust_builtin_type(t)
    {
        refs.push(RawRef {
            from_symbol_id: from_id.to_string(),
            target_name: t.to_string(),
            qualifier: None,
            kind: EdgeKind::TypeRef,
            line,
            receiver_chain: None,
        });
    }
    refs.push(RawRef {
        from_symbol_id: from_id.to_string(),
        target_name: last.to_string(),
        qualifier: qual,
        kind: EdgeKind::Calls,
        line,
        receiver_chain: None,
    });
}

fn emit_scoped_type_ref(
    path: &str,
    from_id: &str,
    impl_type: Option<&str>,
    line: usize,
    refs: &mut Vec<RawRef>,
) {
    let clean = strip_balanced_generics(path);
    let parts: Vec<&str> = clean.split("::").filter(|p| !p.is_empty()).collect();
    let Some(&last) = parts.last() else {
        return;
    };
    let prefix_parts: Vec<&str> = parts[..parts.len() - 1]
        .iter()
        .copied()
        .filter(|p| *p != "self")
        .collect();
    if prefix_parts.is_empty() {
        if !is_rust_builtin_type(last) {
            refs.push(RawRef {
                from_symbol_id: from_id.to_string(),
                target_name: last.to_string(),
                qualifier: None,
                kind: EdgeKind::TypeRef,
                line,
                receiver_chain: None,
            });
        }
        return;
    }
    if prefix_parts.last() == Some(&"Self") {
        if let Some(t) = impl_type
            && !is_rust_builtin_type(t)
        {
            refs.push(RawRef {
                from_symbol_id: from_id.to_string(),
                target_name: t.to_string(),
                qualifier: None,
                kind: EdgeKind::TypeRef,
                line,
                receiver_chain: None,
            });
        }
        return;
    }
    if let Some(&prev) = prefix_parts.last()
        && prev.chars().next().is_some_and(|c| c.is_ascii_uppercase())
    {
        if !is_rust_builtin_type(prev) {
            let qual = if prefix_parts.len() >= 2 {
                Some(prefix_parts[..prefix_parts.len() - 1].join("::"))
            } else {
                None
            };
            refs.push(RawRef {
                from_symbol_id: from_id.to_string(),
                target_name: prev.to_string(),
                qualifier: qual,
                kind: EdgeKind::TypeRef,
                line,
                receiver_chain: None,
            });
        }
        return;
    }
    if !is_rust_builtin_type(last) {
        refs.push(RawRef {
            from_symbol_id: from_id.to_string(),
            target_name: last.to_string(),
            qualifier: Some(prefix_parts.join("::")),
            kind: EdgeKind::TypeRef,
            line,
            receiver_chain: None,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_rust_items(
    parent: Node<'_>,
    ctx: &mut RustExtractCtx<'_>,
    impl_type: Option<&str>,
    mod_path: &str,
    in_test_mod: bool,
    in_trait_impl: bool,
    in_cfg_item: bool,
    collected_methods: &mut Vec<String>,
) {
    let mut cursor = parent.walk();
    let mut pending_attrs: Vec<Node<'_>> = Vec::new();
    let in_test_mod = in_test_mod || has_inner_test_attr(parent, ctx.src);

    for child in parent.children(&mut cursor) {
        if child.kind() == "attribute_item" {
            pending_attrs.push(child);
            continue;
        }
        if matches!(
            child.kind(),
            "line_comment" | "block_comment" | "inner_attribute_item"
        ) {
            continue;
        }
        let attrs = std::mem::take(&mut pending_attrs);
        let has_test_attr = attrs.iter().any(|&a| attr_is_test(node_text(a, ctx.src)));
        let has_cfg_attr = attrs
            .iter()
            .any(|&a| attr_is_non_test_cfg(node_text(a, ctx.src)));
        let has_export_attr = attrs.iter().any(|&a| attr_is_export(node_text(a, ctx.src)));
        let has_serde_attr = attrs.iter().any(|&a| {
            let t = node_text(a, ctx.src);
            t.contains("serde") || t.contains("Serialize") || t.contains("Deserialize")
        });

        match child.kind() {
            "use_declaration" => {
                ctx.imports.push(node_text(child, ctx.src).to_string());
                let is_pub = rust_visibility(child, ctx.src) == Visibility::Public;
                let is_test = in_test_mod || has_test_attr;
                if let Some(arg) = child.child_by_field_name("argument") {
                    extract_rust_use_items(
                        arg,
                        ctx.src,
                        "",
                        mod_path,
                        is_pub,
                        is_test,
                        &mut ctx.rust_uses,
                    );
                }
            }
            "mod_item" => {
                let sub_test = in_test_mod || has_test_attr;
                let sub_cfg = in_cfg_item || has_cfg_attr;
                let mod_name = child
                    .child_by_field_name("name")
                    .map(|n| node_text(n, ctx.src))
                    .unwrap_or("");
                let next_mod = if mod_path.is_empty() {
                    mod_name.to_string()
                } else if mod_name.is_empty() {
                    mod_path.to_string()
                } else {
                    format!("{}::{}", mod_path, mod_name)
                };
                if let Some(body) = child.child_by_field_name("body") {
                    let mut dummy = Vec::new();
                    walk_rust_items(
                        body,
                        ctx,
                        impl_type,
                        &next_mod,
                        sub_test,
                        in_trait_impl,
                        sub_cfg,
                        &mut dummy,
                    );
                } else if !mod_name.is_empty() {
                    let path_overrides = extract_rust_attr_path_overrides(&attrs, ctx.src);
                    if path_overrides.is_empty() {
                        ctx.rust_mod_paths.push(RustModPath {
                            mod_scope: mod_path.to_string(),
                            mod_name: mod_name.to_string(),
                            path: None,
                            is_test: sub_test,
                        });
                    } else {
                        for p in path_overrides {
                            ctx.rust_mod_paths.push(RustModPath {
                                mod_scope: mod_path.to_string(),
                                mod_name: mod_name.to_string(),
                                path: Some(p),
                                is_test: sub_test,
                            });
                        }
                    }
                }
            }
            "impl_item" => {
                let ty = child
                    .child_by_field_name("type")
                    .map(|t| extract_rust_base_type_name(node_text(t, ctx.src)))
                    .filter(|s| !s.is_empty());
                let trait_node = child.child_by_field_name("trait");
                let is_trait_impl = trait_node.is_some();
                let impl_is_test = in_test_mod || has_test_attr;
                let impl_is_cfg = in_cfg_item || has_cfg_attr;
                let mut impl_methods = Vec::new();

                if let Some(body) = child.child_by_field_name("body") {
                    if let (Some(tnode), Some(type_name)) = (trait_node, ty.as_deref()) {
                        let tname = extract_rust_base_type_name(node_text(tnode, ctx.src));
                        if matches!(tname.as_str(), "Deref" | "DerefMut") {
                            let mut bc = body.walk();
                            for item in body.children(&mut bc) {
                                if item.kind() == "type_item"
                                    && item
                                        .child_by_field_name("name")
                                        .map(|n| node_text(n, ctx.src) == "Target")
                                        .unwrap_or(false)
                                    && let Some(target_ty) = item.child_by_field_name("type")
                                {
                                    let norm = normalize_rust_type_text(
                                        node_text(target_ty, ctx.src),
                                        Some(type_name),
                                    );
                                    let clean = norm.trim_start_matches("[]");
                                    if !clean.is_empty() {
                                        ctx.embedded_types
                                            .entry(type_name.to_string())
                                            .or_default()
                                            .push(clean.to_string());
                                    }
                                }
                            }
                        }
                    }

                    walk_rust_items(
                        body,
                        ctx,
                        ty.as_deref(),
                        mod_path,
                        impl_is_test,
                        in_trait_impl || is_trait_impl,
                        impl_is_cfg,
                        &mut impl_methods,
                    );
                }
                // Propagate generic bounds / where-clause type refs on impl blocks to their methods
                if !impl_methods.is_empty() {
                    let mut ic = child.walk();
                    for ch in child.children(&mut ic) {
                        if matches!(ch.kind(), "type_parameters" | "where_clause") {
                            for mid in &impl_methods {
                                collect_type_refs(
                                    ch,
                                    ctx.src,
                                    mid,
                                    ty.as_deref(),
                                    &mut ctx.refs,
                                    &mut ctx.bridges,
                                );
                            }
                        }
                    }
                }

                if let (Some(tnode), Some(type_name)) = (trait_node, ty) {
                    let raw_trait = node_text(tnode, ctx.src).trim().to_string();
                    let trait_path = raw_trait
                        .split('<')
                        .next()
                        .unwrap_or(&raw_trait)
                        .trim()
                        .to_string();
                    let trait_name = trait_path
                        .rsplit("::")
                        .next()
                        .unwrap_or(&trait_path)
                        .trim()
                        .to_string();
                    let trait_type_args =
                        extract_generic_type_arg_names(&raw_trait, Some(&type_name));
                    ctx.trait_impls.push(TraitImplInfo {
                        type_name,
                        trait_name,
                        trait_path,
                        trait_type_args,
                        method_ids: impl_methods,
                        is_test: impl_is_test,
                    });
                }
            }
            "macro_definition" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let macro_sym_name = format!("{}!", node_text(name_node, ctx.src));
                    let span = node_span(child, 0);
                    let id = make_symbol_id(ctx.rel_path, &macro_sym_name, span.start_line);
                    let is_test = in_test_mod || has_test_attr;
                    if !mod_path.is_empty() {
                        ctx.symbol_mod_scopes
                            .insert(id.clone(), mod_path.to_string());
                    }
                    ctx.symbols.push(Symbol {
                        id: id.clone(),
                        name: macro_sym_name.clone(),
                        qualified_name: macro_sym_name.clone(),
                        kind: SymbolKind::Const,
                        lang: Language::Rust,
                        file: ctx.rel_path.to_string(),
                        span,
                        visibility: Visibility::Public,
                        is_test,
                        is_entrypoint: false,
                        is_dynamic_candidate: false,
                        package_or_module: ctx.pkg.to_string(),
                    });
                    let empty_env = RustLocalEnv::new();
                    let mut generated_items = Vec::new();
                    let mut mc = child.walk();
                    for rule in child.children(&mut mc) {
                        if rule.kind() == "macro_rule"
                            && let Some(right) = rule.child_by_field_name("right")
                        {
                            scan_rust_token_tree(
                                right,
                                ctx.src,
                                &id,
                                impl_type,
                                &empty_env,
                                &mut ctx.refs,
                                &mut ctx.bridges,
                            );
                            extract_rust_macro_defined_items(right, ctx.src, &mut generated_items);
                        }
                    }
                    if !generated_items.is_empty() {
                        ctx.rust_macro_defs
                            .entry(macro_sym_name)
                            .or_default()
                            .extend(generated_items);
                    }
                }
            }
            "macro_invocation" | "expression_statement" => {
                let macro_node = if child.kind() == "macro_invocation" {
                    Some(child)
                } else {
                    child
                        .named_child(0)
                        .filter(|n| n.kind() == "macro_invocation")
                };
                if let Some(mcall) = macro_node {
                    if let Some(ty) = impl_type {
                        if !in_test_mod
                            && !has_test_attr
                            && let Some(mnode) = mcall.child_by_field_name("macro")
                        {
                            let mtxt = node_text(mnode, ctx.src);
                            let mname = mtxt.rsplit("::").next().unwrap_or(mtxt).trim();
                            if !mname.is_empty() {
                                ctx.impl_macros.push((
                                    ty.to_string(),
                                    format!("{}!", mname),
                                    mcall.start_position().row + 1,
                                ));
                            }
                        }
                    } else if let Some(mnode) = mcall.child_by_field_name("macro") {
                        let mtxt = node_text(mnode, ctx.src);
                        let mname = mtxt.rsplit("::").next().unwrap_or(mtxt).trim();
                        if !mname.is_empty() {
                            let span = node_span(mcall, 0);
                            let is_test = in_test_mod || has_test_attr;
                            let macro_target = format!("{}!", mname);
                            let inv_name = format!("{}#{}!", mname, span.start_line);
                            let inv_id = make_symbol_id(ctx.rel_path, &inv_name, span.start_line);
                            if !mod_path.is_empty() {
                                ctx.symbol_mod_scopes
                                    .insert(inv_id.clone(), mod_path.to_string());
                            }
                            ctx.symbols.push(Symbol {
                                id: inv_id.clone(),
                                name: inv_name.clone(),
                                qualified_name: inv_name.clone(),
                                kind: SymbolKind::Const,
                                lang: Language::Rust,
                                file: ctx.rel_path.to_string(),
                                span,
                                visibility: Visibility::Public,
                                is_test,
                                is_entrypoint: false,
                                is_dynamic_candidate: false,
                                package_or_module: ctx.pkg.to_string(),
                            });
                            ctx.refs.push(RawRef {
                                from_symbol_id: inv_id.clone(),
                                target_name: macro_target.clone(),
                                qualifier: None,
                                kind: EdgeKind::Calls,
                                line: span.start_line,
                                receiver_chain: None,
                            });
                            let empty_env = RustLocalEnv::new();
                            let mut inv_generated_items = Vec::new();
                            let mut tc = mcall.walk();
                            for ch in mcall.children(&mut tc) {
                                if ch.kind() == "token_tree" {
                                    scan_rust_token_tree(
                                        ch,
                                        ctx.src,
                                        &inv_id,
                                        None,
                                        &empty_env,
                                        &mut ctx.refs,
                                        &mut ctx.bridges,
                                    );
                                    extract_rust_macro_defined_items(
                                        ch,
                                        ctx.src,
                                        &mut inv_generated_items,
                                    );
                                }
                            }
                            if !inv_generated_items.is_empty() {
                                ctx.rust_macro_defs
                                    .entry(inv_name)
                                    .or_default()
                                    .extend(inv_generated_items);
                            }
                            ctx.rust_top_level_macros.push((inv_id, macro_target));
                        }
                    }
                }
            }
            "const_item" | "static_item" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, ctx.src).to_string();
                    if name != "_" {
                        let qname = match impl_type {
                            Some(t) if !t.is_empty() => format!("{}::{}", t, name),
                            _ => name.clone(),
                        };
                        let span = node_span(child, 0);
                        let id = make_symbol_id(ctx.rel_path, &qname, span.start_line);
                        let is_test = in_test_mod || has_test_attr;
                        if in_trait_impl {
                            collected_methods.push(id.clone());
                        }
                        if !mod_path.is_empty() {
                            ctx.symbol_mod_scopes
                                .insert(id.clone(), mod_path.to_string());
                        }
                        ctx.symbols.push(Symbol {
                            id: id.clone(),
                            name,
                            qualified_name: qname,
                            kind: if child.kind() == "static_item" {
                                SymbolKind::Var
                            } else {
                                SymbolKind::Const
                            },
                            lang: Language::Rust,
                            file: ctx.rel_path.to_string(),
                            span,
                            visibility: rust_visibility(child, ctx.src),
                            is_test,
                            is_entrypoint: has_export_attr,
                            is_dynamic_candidate: false,
                            package_or_module: ctx.pkg.to_string(),
                        });
                        let mut env = RustLocalEnv::new();
                        extract_rust_attr_nodes_refs(
                            &attrs,
                            ctx.src,
                            &id,
                            impl_type,
                            &env,
                            &mut ctx.refs,
                            &mut ctx.bridges,
                        );
                        if let Some(ty_node) = child.child_by_field_name("type") {
                            collect_type_refs(
                                ty_node,
                                ctx.src,
                                &id,
                                impl_type,
                                &mut ctx.refs,
                                &mut ctx.bridges,
                            );
                            let norm =
                                normalize_rust_type_text(node_text(ty_node, ctx.src), impl_type);
                            if !norm.is_empty() {
                                ctx.func_return_types.insert(id.clone(), vec![norm]);
                            }
                        }
                        if let Some(val_node) = child.child_by_field_name("value") {
                            walk_rust_expr(
                                val_node,
                                ctx.src,
                                &id,
                                impl_type,
                                &mut env,
                                &mut ctx.refs,
                                &mut ctx.bridges,
                            );
                        }
                    }
                }
            }
            "function_item" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, ctx.src).to_string();
                    let qname = match impl_type {
                        Some(t) if !t.is_empty() => format!("{}::{}", t, name),
                        _ => name.clone(),
                    };
                    let span = node_span(child, 0);
                    let id = make_symbol_id(ctx.rel_path, &qname, span.start_line);
                    let vis = rust_visibility(child, ctx.src);
                    let is_test = in_test_mod || has_test_attr;
                    let is_cfg = (in_cfg_item || has_cfg_attr) && !is_test;
                    let is_entrypoint = name == "main" || has_export_attr;
                    if !mod_path.is_empty() {
                        ctx.symbol_mod_scopes
                            .insert(id.clone(), mod_path.to_string());
                    }
                    if is_cfg {
                        ctx.cfg_gated_symbols
                            .insert(id.clone(), mod_path.to_string());
                    }
                    collected_methods.push(id.clone());
                    ctx.symbols.push(Symbol {
                        id: id.clone(),
                        name: name.clone(),
                        qualified_name: qname.clone(),
                        kind: if impl_type.is_some() {
                            SymbolKind::Method
                        } else {
                            SymbolKind::Function
                        },
                        lang: Language::Rust,
                        file: ctx.rel_path.to_string(),
                        span,
                        visibility: vis,
                        is_test,
                        is_entrypoint,
                        is_dynamic_candidate: in_trait_impl
                            || (impl_type.is_some() && vis == Visibility::Public),
                        package_or_module: ctx.pkg.to_string(),
                    });

                    if let Some(ty) = impl_type
                        && !in_trait_impl
                        && !is_rust_builtin_type(ty)
                    {
                        ctx.refs.push(RawRef {
                            from_symbol_id: id.clone(),
                            target_name: ty.to_string(),
                            qualifier: None,
                            kind: EdgeKind::TypeRef,
                            line: span.start_line,
                            receiver_chain: None,
                        });
                    }

                    if let Some(ret_node) = child.child_by_field_name("return_type") {
                        collect_type_refs(
                            ret_node,
                            ctx.src,
                            &id,
                            impl_type,
                            &mut ctx.refs,
                            &mut ctx.bridges,
                        );
                        let ret_tys =
                            normalize_rust_return_types(node_text(ret_node, ctx.src), impl_type);
                        if !ret_tys.is_empty() {
                            ctx.func_return_types.insert(id.clone(), ret_tys);
                        }
                    }
                    for field in ["type_parameters", "where_clause"] {
                        if let Some(n) = child.child_by_field_name(field) {
                            collect_type_refs(
                                n,
                                ctx.src,
                                &id,
                                impl_type,
                                &mut ctx.refs,
                                &mut ctx.bridges,
                            );
                        }
                    }
                    let mut fc = child.walk();
                    for ch in child.children(&mut fc) {
                        if ch.kind() == "where_clause" {
                            collect_type_refs(
                                ch,
                                ctx.src,
                                &id,
                                impl_type,
                                &mut ctx.refs,
                                &mut ctx.bridges,
                            );
                        }
                    }

                    let cb_params = extract_rust_callback_params(child, ctx.src, impl_type);
                    if !cb_params.is_empty() {
                        ctx.func_callback_params.insert(id.clone(), cb_params);
                    }

                    let mut env = RustLocalEnv::new();
                    let params = extract_rust_params_into_env(
                        child,
                        ctx.src,
                        impl_type,
                        &id,
                        &mut env,
                        &mut ctx.refs,
                        &mut ctx.bridges,
                    );
                    extract_rust_attr_nodes_refs(
                        &attrs,
                        ctx.src,
                        &id,
                        impl_type,
                        &env,
                        &mut ctx.refs,
                        &mut ctx.bridges,
                    );

                    if let Some(body) = child.child_by_field_name("body") {
                        walk_rust_expr(
                            body,
                            ctx.src,
                            &id,
                            impl_type,
                            &mut env,
                            &mut ctx.refs,
                            &mut ctx.bridges,
                        );
                        let ir_body = lower_rust_node(body, ctx.src);
                        ctx.ir_funcs.push(IrFunc {
                            symbol_id: id,
                            name: qname,
                            file: ctx.rel_path.to_string(),
                            lang: Language::Rust,
                            span,
                            params,
                            body: ir_body,
                        });
                    }
                }
            }
            "struct_item" | "enum_item" | "trait_item" | "type_item" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, ctx.src).to_string();
                    let qname = if child.kind() == "type_item"
                        && let Some(t) = impl_type
                        && !t.is_empty()
                    {
                        format!("{}::{}", t, name)
                    } else {
                        name.clone()
                    };
                    let span = node_span(child, 0);
                    let id = make_symbol_id(ctx.rel_path, &qname, span.start_line);
                    let kind = match child.kind() {
                        "struct_item" => SymbolKind::Struct,
                        "enum_item" => SymbolKind::Enum,
                        "trait_item" => SymbolKind::Trait,
                        _ => SymbolKind::Type,
                    };
                    let is_test = in_test_mod || has_test_attr;
                    if child.kind() == "type_item" && in_trait_impl {
                        collected_methods.push(id.clone());
                    }
                    if !mod_path.is_empty() {
                        ctx.symbol_mod_scopes
                            .insert(id.clone(), mod_path.to_string());
                    }
                    ctx.symbols.push(Symbol {
                        id: id.clone(),
                        name: name.clone(),
                        qualified_name: qname,
                        kind,
                        lang: Language::Rust,
                        file: ctx.rel_path.to_string(),
                        span,
                        visibility: rust_visibility(child, ctx.src),
                        is_test,
                        is_entrypoint: false,
                        is_dynamic_candidate: kind == SymbolKind::Trait || has_serde_attr,
                        package_or_module: ctx.pkg.to_string(),
                    });
                    let empty_env = RustLocalEnv::new();
                    extract_rust_attr_nodes_refs(
                        &attrs,
                        ctx.src,
                        &id,
                        impl_type,
                        &empty_env,
                        &mut ctx.refs,
                        &mut ctx.bridges,
                    );
                    if has_serde_attr || ctx.rel_path.contains("protocol") {
                        ctx.bridges.push(BridgeHint {
                            from_symbol_id: id.clone(),
                            bridge_kind: BridgeKind::WireContract,
                            key: format!("type:{}", name),
                            role: BridgeRole::Both,
                            line: span.start_line,
                            confidence: Confidence::Heuristic,
                        });
                    }

                    if kind == SymbolKind::Struct {
                        let fields = extract_rust_struct_fields(
                            child,
                            ctx.src,
                            &id,
                            &mut ctx.refs,
                            &mut ctx.bridges,
                        );
                        if !fields.is_empty() {
                            ctx.struct_fields.insert(id.clone(), fields);
                        }
                    } else if kind == SymbolKind::Enum {
                        extract_rust_enum_attrs(
                            child,
                            ctx.src,
                            &id,
                            &mut ctx.refs,
                            &mut ctx.bridges,
                        );
                        let efields = extract_rust_enum_fields(child, ctx.src);
                        if !efields.is_empty() {
                            ctx.struct_fields.insert(id.clone(), efields);
                        }
                    } else if kind == SymbolKind::Trait {
                        let trait_methods = extract_rust_trait_methods(
                            child,
                            ctx.src,
                            &id,
                            &mut ctx.refs,
                            &mut ctx.bridges,
                        );
                        if !trait_methods.is_empty() {
                            ctx.interface_methods.insert(id.clone(), trait_methods);
                        }
                    } else if kind == SymbolKind::Type
                        && !in_trait_impl
                        && impl_type.is_none()
                        && let Some(rhs) = child.child_by_field_name("type")
                    {
                        let raw_rhs = node_text(rhs, ctx.src);
                        let norm = normalize_rust_type_text(raw_rhs, impl_type);
                        if !norm.is_empty() && norm != name {
                            let wrapped = rust_type_is_wrapper(raw_rhs);
                            let stored = if wrapped && !norm.starts_with("[]") {
                                format!("*{}", norm)
                            } else {
                                norm
                            };
                            ctx.type_aliases.insert(id.clone(), stored);
                        }
                    }
                    collect_type_refs(
                        child,
                        ctx.src,
                        &id,
                        impl_type,
                        &mut ctx.refs,
                        &mut ctx.bridges,
                    );
                }
            }
            _ => {}
        }
    }
}

fn merge_rust_field_type(fields: &mut HashMap<String, String>, key: String, norm: String) {
    fields
        .entry(key)
        .and_modify(|existing| {
            if !existing.split('|').any(|p| p == norm) {
                existing.push('|');
                existing.push_str(&norm);
            }
        })
        .or_insert(norm);
}

fn extract_rust_struct_fields(
    struct_node: Node<'_>,
    src: &str,
    struct_id: &str,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let Some(body) = struct_node.child_by_field_name("body") else {
        return fields;
    };
    let empty_env = RustLocalEnv::new();
    match body.kind() {
        "field_declaration_list" => {
            let mut pending_attrs: Vec<Node<'_>> = Vec::new();
            let mut c = body.walk();
            for ch in body.children(&mut c) {
                if ch.kind() == "attribute_item" {
                    pending_attrs.push(ch);
                    continue;
                }
                if matches!(ch.kind(), "line_comment" | "block_comment") {
                    continue;
                }
                if ch.kind() == "field_declaration" {
                    let attrs = std::mem::take(&mut pending_attrs);
                    extract_rust_attr_nodes_refs(
                        &attrs, src, struct_id, None, &empty_env, refs, bridges,
                    );
                    if let (Some(fname), Some(fty)) = (
                        ch.child_by_field_name("name"),
                        ch.child_by_field_name("type"),
                    ) {
                        let norm = normalize_rust_type_text(node_text(fty, src), None);
                        if !norm.is_empty() {
                            merge_rust_field_type(
                                &mut fields,
                                node_text(fname, src).to_string(),
                                norm,
                            );
                        }
                    }
                }
            }
        }
        "ordered_field_declaration_list" => {
            let mut c = body.walk();
            for ch in body.children(&mut c) {
                if ch.kind() == "attribute_item" {
                    extract_rust_attr_node_refs(
                        ch, src, struct_id, None, &empty_env, refs, bridges,
                    );
                }
            }
            extract_ordered_fields(body, src, None, &mut fields);
        }
        _ => {}
    }
    fields
}

fn extract_ordered_fields(
    body: Node<'_>,
    src: &str,
    variant_prefix: Option<&str>,
    out: &mut HashMap<String, String>,
) {
    let mut idx = 0usize;
    let mut c = body.walk();
    for ch in body.children(&mut c) {
        if !ch.is_named()
            || matches!(
                ch.kind(),
                "attribute_item" | "visibility_modifier" | "line_comment" | "block_comment"
            )
        {
            continue;
        }
        let norm = normalize_rust_type_text(node_text(ch, src), None);
        if !norm.is_empty() {
            let key = match variant_prefix {
                Some(v) => format!("{v}::{idx}"),
                None => idx.to_string(),
            };
            merge_rust_field_type(out, key, norm);
        }
        idx += 1;
    }
}

fn extract_rust_enum_fields(enum_node: Node<'_>, src: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let Some(body) = enum_node.child_by_field_name("body") else {
        return fields;
    };
    let mut c = body.walk();
    for variant in body.children(&mut c) {
        if variant.kind() != "enum_variant" {
            continue;
        }
        let Some(vname_node) = variant.child_by_field_name("name") else {
            continue;
        };
        let vname = node_text(vname_node, src);
        let Some(vbody) = variant.child_by_field_name("body") else {
            continue;
        };
        match vbody.kind() {
            "field_declaration_list" => {
                let mut fc = vbody.walk();
                for ch in vbody.children(&mut fc) {
                    if ch.kind() == "field_declaration"
                        && let (Some(fname), Some(fty)) = (
                            ch.child_by_field_name("name"),
                            ch.child_by_field_name("type"),
                        )
                    {
                        let norm = normalize_rust_type_text(node_text(fty, src), None);
                        if !norm.is_empty() {
                            fields.insert(format!("{}::{}", vname, node_text(fname, src)), norm);
                        }
                    }
                }
            }
            "ordered_field_declaration_list" => {
                extract_ordered_fields(vbody, src, Some(vname), &mut fields);
            }
            _ => {}
        }
    }
    fields
}

fn split_top_level_rust_type_list(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut paren = 0usize;
    let mut angle = 0usize;
    let mut bracket = 0usize;
    let mut start = 0usize;
    let bytes = s.as_bytes();
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '<' => angle += 1,
            '>' => {
                if i > 0 && bytes[i - 1] == b'-' {
                    // Skip '->' return arrow inside nested Fn(...) -> Ret types.
                } else {
                    angle = angle.saturating_sub(1);
                }
            }
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            ',' if paren == 0 && angle == 0 && bracket == 0 => {
                let p = s[start..i].trim();
                if !p.is_empty() {
                    parts.push(p);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

fn extract_fn_trait_param_types(text: &str, impl_type: Option<&str>) -> Option<Vec<String>> {
    for kw in ["FnOnce", "FnMut", "Fn"] {
        let mut search_from = 0usize;
        while let Some(rel) = text[search_from..].find(kw) {
            let pos = search_from + rel;
            let prev_ok = pos == 0
                || !text[..pos]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            let after = &text[pos + kw.len()..];
            let after_trim = after.trim_start();
            if prev_ok
                && let Some(rest) = after_trim.strip_prefix('(')
                && let Some(inner) = extract_balanced_parens(rest)
            {
                let parts = split_top_level_rust_type_list(inner);
                let norms: Vec<String> = parts
                    .into_iter()
                    .map(|p| normalize_rust_type_text(p, impl_type))
                    .collect();
                if norms.iter().any(|s| !s.is_empty()) {
                    return Some(norms);
                }
            }
            search_from = pos + kw.len();
        }
    }
    None
}

fn extract_rust_callback_params(
    func_node: Node<'_>,
    src: &str,
    impl_type: Option<&str>,
) -> Vec<(usize, usize, String)> {
    let mut generic_fn_bounds: HashMap<String, Vec<String>> = HashMap::new();

    if let Some(tparams) = func_node.child_by_field_name("type_parameters") {
        let mut c = tparams.walk();
        for ch in tparams.children(&mut c) {
            if matches!(ch.kind(), "constrained_type_parameter" | "type_parameter") {
                let left = ch
                    .child_by_field_name("left")
                    .or_else(|| ch.child_by_field_name("name"));
                if let (Some(left), Some(bounds)) = (left, ch.child_by_field_name("bounds"))
                    && let Some(tys) =
                        extract_fn_trait_param_types(node_text(bounds, src), impl_type)
                {
                    generic_fn_bounds.insert(node_text(left, src).trim().to_string(), tys);
                }
            }
        }
    }

    let mut fc = func_node.walk();
    for ch in func_node.children(&mut fc) {
        if ch.kind() == "where_clause" {
            let mut wc = ch.walk();
            for pred in ch.children(&mut wc) {
                if pred.kind() == "where_predicate"
                    && let (Some(left), Some(bounds)) = (
                        pred.child_by_field_name("left"),
                        pred.child_by_field_name("bounds"),
                    )
                    && let Some(tys) =
                        extract_fn_trait_param_types(node_text(bounds, src), impl_type)
                {
                    generic_fn_bounds.insert(node_text(left, src).trim().to_string(), tys);
                }
            }
        }
    }

    let mut out = Vec::new();
    let Some(plist) = func_node.child_by_field_name("parameters") else {
        return out;
    };
    let mut arg_idx = 0usize;
    let mut pc = plist.walk();
    for ch in plist.children(&mut pc) {
        if ch.kind() != "parameter" {
            continue;
        }
        // Skip explicit typed receivers like self: Arc<Self> or mut self: Pin<&mut Self>.
        if arg_idx == 0
            && ch.child_by_field_name("pattern").is_some_and(|p| {
                node_text(p, src).trim().trim_start_matches("mut ").trim() == "self"
            })
        {
            continue;
        }
        if let Some(ty_node) = ch.child_by_field_name("type") {
            let ty_text = node_text(ty_node, src);
            let resolved = extract_fn_trait_param_types(ty_text, impl_type).or_else(|| {
                let stripped = strip_rust_type_prefixes(ty_text);
                let base = stripped.split('<').next().unwrap_or(stripped).trim();
                if let Some(tys) = generic_fn_bounds.get(base) {
                    return Some(tys.clone());
                }
                let short_base = base.rsplit("::").next().unwrap_or(base).trim();
                if matches!(short_base, "Option" | "Box" | "Arc" | "Rc") {
                    for ga in split_top_level_generic_args(stripped) {
                        let gbase = strip_rust_type_prefixes(ga)
                            .split('<')
                            .next()
                            .unwrap_or("")
                            .trim();
                        if let Some(tys) = generic_fn_bounds.get(gbase) {
                            return Some(tys.clone());
                        }
                    }
                }
                None
            });
            if let Some(param_tys) = resolved {
                for (pidx, pty) in param_tys.into_iter().enumerate() {
                    if !pty.is_empty() {
                        out.push((arg_idx, pidx, pty));
                    }
                }
            }
        }
        arg_idx += 1;
    }
    out
}

fn extract_rust_enum_attrs(
    enum_node: Node<'_>,
    src: &str,
    enum_id: &str,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let empty_env = RustLocalEnv::new();
    let mut stack = vec![enum_node];
    while let Some(n) = stack.pop() {
        if n.kind() == "attribute_item" {
            extract_rust_attr_node_refs(n, src, enum_id, None, &empty_env, refs, bridges);
            continue;
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
}

fn extract_rust_trait_methods(
    trait_node: Node<'_>,
    src: &str,
    trait_id: &str,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) -> Vec<String> {
    let mut methods = Vec::new();
    let Some(body) = trait_node.child_by_field_name("body") else {
        return methods;
    };
    let mut c = body.walk();
    for ch in body.children(&mut c) {
        match ch.kind() {
            "function_signature_item" => {
                if let Some(n) = ch.child_by_field_name("name") {
                    methods.push(node_text(n, src).to_string());
                }
            }
            "function_item" => {
                if let Some(n) = ch.child_by_field_name("name") {
                    methods.push(node_text(n, src).to_string());
                }
                if let Some(fbody) = ch.child_by_field_name("body") {
                    let mut env = RustLocalEnv::new();
                    let _ = extract_rust_params_into_env(
                        ch, src, None, trait_id, &mut env, refs, bridges,
                    );
                    walk_rust_expr(fbody, src, trait_id, None, &mut env, refs, bridges);
                }
            }
            _ => {}
        }
    }
    methods
}

fn rust_visibility(node: Node<'_>, src: &str) -> Visibility {
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        if ch.kind() == "visibility_modifier" {
            let txt = node_text(ch, src);
            if txt == "pub" || txt.starts_with("pub(") {
                return Visibility::Public;
            }
        }
    }
    Visibility::Private
}

fn extract_rust_params_into_env(
    node: Node<'_>,
    src: &str,
    impl_type: Option<&str>,
    from_id: &str,
    env: &mut RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) -> Vec<String> {
    let mut params = Vec::new();
    if let Some(plist) = node.child_by_field_name("parameters") {
        let mut c = plist.walk();
        for ch in plist.children(&mut c) {
            if ch.kind() == "parameter" {
                let param_ty = ch.child_by_field_name("type").and_then(|t| {
                    collect_type_refs(t, src, from_id, impl_type, refs, bridges);
                    let norm = normalize_rust_type_text(node_text(t, src), impl_type);
                    if norm.is_empty() {
                        None
                    } else {
                        Some(vec![ReceiverStep::Type(norm)])
                    }
                });
                if let Some(pat) = ch.child_by_field_name("pattern") {
                    let raw_pat = node_text(pat, src)
                        .trim()
                        .trim_start_matches("mut ")
                        .to_string();
                    params.push(raw_pat);
                    bind_rust_pattern(pat, src, param_ty, impl_type, env);
                }
            } else if ch.kind() == "self_parameter" {
                params.push("self".to_string());
                let self_chain = impl_type.map(|t| vec![ReceiverStep::Type(t.to_string())]);
                env.bind("self".to_string(), self_chain);
            }
        }
    }
    params
}

fn pattern_field_chain(
    raw_pat_type: &str,
    field_key: &str,
    scrutinee_chain: Option<&[ReceiverStep]>,
    impl_type: Option<&str>,
) -> Option<Vec<ReceiverStep>> {
    let cleaned = strip_balanced_generics(raw_pat_type);
    let pat_type = cleaned.trim();
    if pat_type.is_empty() {
        return None;
    }
    if let Some((prefix, last_seg)) = pat_type.rsplit_once("::") {
        let prefix = prefix.trim();
        let last_seg = last_seg.trim();
        let prefix_last = prefix.rsplit("::").next().unwrap_or(prefix).trim();
        let is_enum_variant = prefix_last == "Self"
            || prefix_last
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase());
        if is_enum_variant {
            let variant_field = format!("{}::{}", last_seg, field_key);
            if let Some(base) = scrutinee_chain {
                let mut ch = base.to_vec();
                ch.push(ReceiverStep::Field(variant_field));
                return Some(ch);
            }
            let norm = normalize_rust_type_text(prefix, impl_type);
            let resolved = norm.trim_start_matches("[]").to_string();
            if !resolved.is_empty() {
                return Some(vec![
                    ReceiverStep::Type(resolved),
                    ReceiverStep::Field(variant_field),
                ]);
            }
            return None;
        }
        // Module-qualified struct pattern (e.g. crate::foo::MyStruct { field }).
        if let Some(base) = scrutinee_chain {
            let mut ch = base.to_vec();
            ch.push(ReceiverStep::Field(field_key.to_string()));
            return Some(ch);
        }
        let norm = normalize_rust_type_text(pat_type, impl_type);
        let resolved = norm.trim_start_matches("[]").to_string();
        if !resolved.is_empty() {
            return Some(vec![
                ReceiverStep::Type(resolved),
                ReceiverStep::Field(field_key.to_string()),
            ]);
        }
        return None;
    }
    // Bare pattern name (could be a struct or an imported enum variant).
    if let Some(base) = scrutinee_chain {
        let mut ch = base.to_vec();
        ch.push(ReceiverStep::Field(format!("{}::{}", pat_type, field_key)));
        return Some(ch);
    }
    if pat_type == "Self"
        || pat_type
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
    {
        let norm = normalize_rust_type_text(pat_type, impl_type);
        let resolved = norm.trim_start_matches("[]").to_string();
        if !resolved.is_empty() {
            return Some(vec![
                ReceiverStep::Type(resolved),
                ReceiverStep::Field(field_key.to_string()),
            ]);
        }
    }
    None
}

fn chain_with_err_result(chain: Option<&[ReceiverStep]>) -> Option<Vec<ReceiverStep>> {
    let mut out = chain?.to_vec();
    for idx in (0..out.len()).rev() {
        match &mut out[idx] {
            ReceiverStep::Call { result_index, .. } => {
                *result_index = 1;
                out.truncate(idx + 1);
                return Some(out);
            }
            ReceiverStep::MethodCall {
                name, result_index, ..
            } => {
                if matches!(
                    name.as_str(),
                    "map_err" | "context" | "with_context" | "or_else" | "or"
                ) {
                    return None;
                }
                if matches!(
                    name.as_str(),
                    "as_ref"
                        | "as_mut"
                        | "as_deref"
                        | "as_deref_mut"
                        | "inspect"
                        | "inspect_err"
                        | "map"
                        | "and_then"
                ) {
                    continue;
                }
                *result_index = 1;
                out.truncate(idx + 1);
                return Some(out);
            }
            _ => return None,
        }
    }
    None
}

fn bind_rust_pattern(
    pat: Node<'_>,
    src: &str,
    inferred_chain: Option<Vec<ReceiverStep>>,
    impl_type: Option<&str>,
    env: &mut RustLocalEnv,
) {
    match pat.kind() {
        "match_pattern" => {
            let cond_id = pat.child_by_field_name("condition").map(|n| n.id());
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_named() && Some(ch.id()) != cond_id {
                    bind_rust_pattern(ch, src, inferred_chain.clone(), impl_type, env);
                }
            }
        }
        "identifier" => {
            let name = node_text(pat, src);
            if name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            {
                env.bind(name.to_string(), inferred_chain);
            }
        }
        "mut_pattern" | "ref_pattern" | "reference_pattern" => {
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_named() && ch.kind() != "mutable_specifier" {
                    bind_rust_pattern(ch, src, inferred_chain.clone(), impl_type, env);
                }
            }
        }
        "tuple_struct_pattern" => {
            let type_node = pat.child_by_field_name("type");
            let type_id = type_node.map(|n| n.id());
            let type_name = type_node.map(|n| node_text(n, src)).unwrap_or("").trim();
            let is_wrapper = matches!(type_name, "Some" | "Ok" | "Box" | "Arc" | "Rc")
                || type_name.ends_with("::Some")
                || type_name.ends_with("::Ok");
            let is_err = type_name == "Err" || type_name.ends_with("::Err");
            let mut c = pat.walk();
            let mut pos_idx: Option<usize> = Some(0);
            for ch in pat.children(&mut c) {
                if Some(ch.id()) == type_id {
                    continue;
                }
                if ch.is_extra() {
                    continue;
                }
                if ch.kind() == "remaining_field_pattern" {
                    pos_idx = None;
                    continue;
                }
                if ch.kind() == "_" {
                    if let Some(idx) = pos_idx {
                        pos_idx = Some(idx + 1);
                    }
                    continue;
                }
                if ch.is_named() {
                    let field_chain = if is_wrapper {
                        inferred_chain.clone()
                    } else if is_err {
                        chain_with_err_result(inferred_chain.as_deref())
                    } else if let Some(idx) = pos_idx {
                        pattern_field_chain(
                            type_name,
                            &idx.to_string(),
                            inferred_chain.as_deref(),
                            impl_type,
                        )
                    } else {
                        None
                    };
                    if let Some(idx) = pos_idx {
                        pos_idx = Some(idx + 1);
                    }
                    bind_rust_pattern(ch, src, field_chain, impl_type, env);
                }
            }
        }
        "tuple_pattern" => {
            let mut elems: Vec<(Option<usize>, Option<Node<'_>>)> = Vec::new();
            let mut pos_idx: Option<usize> = Some(0);
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_extra() {
                    continue;
                }
                if ch.kind() == "remaining_field_pattern" {
                    pos_idx = None;
                    continue;
                }
                if ch.kind() == "_" {
                    elems.push((pos_idx, None));
                    if let Some(idx) = pos_idx {
                        pos_idx = Some(idx + 1);
                    }
                    continue;
                }
                if ch.is_named() {
                    elems.push((pos_idx, Some(ch)));
                    if let Some(idx) = pos_idx {
                        pos_idx = Some(idx + 1);
                    }
                }
            }
            let is_enumerate_elem = elems.len() == 2
                && inferred_chain.as_ref().is_some_and(|v| {
                    v.len() >= 2
                        && matches!(v.last(), Some(ReceiverStep::Elem))
                        && matches!(
                            &v[v.len() - 2],
                            ReceiverStep::MethodCall { name, .. } if name == "enumerate"
                        )
                });
            if is_enumerate_elem {
                if let Some(first_pat) = elems[0].1 {
                    bind_rust_pattern(first_pat, src, None, impl_type, env);
                }
                if let Some(second_pat) = elems[1].1 {
                    bind_rust_pattern(second_pat, src, inferred_chain, impl_type, env);
                }
            } else {
                for (slot_idx, node_opt) in elems {
                    let Some(ch) = node_opt else {
                        continue;
                    };
                    let elem_chain = match (slot_idx, &inferred_chain) {
                        (Some(idx), Some(base)) => {
                            let mut next = base.clone();
                            next.push(ReceiverStep::TupleElem(idx));
                            Some(next)
                        }
                        _ => None,
                    };
                    bind_rust_pattern(ch, src, elem_chain, impl_type, env);
                }
            }
        }
        "struct_pattern" => {
            let type_name = pat
                .child_by_field_name("type")
                .map(|n| node_text(n, src))
                .unwrap_or("")
                .trim();
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.kind() == "field_pattern" {
                    let fname_opt = ch.child_by_field_name("name").map(|n| node_text(n, src));
                    let field_chain = fname_opt.and_then(|fname| {
                        pattern_field_chain(type_name, fname, inferred_chain.as_deref(), impl_type)
                    });
                    if let Some(inner_pat) = ch.child_by_field_name("pattern") {
                        bind_rust_pattern(inner_pat, src, field_chain, impl_type, env);
                    } else if let Some(name) = fname_opt {
                        env.bind(name.to_string(), field_chain);
                    }
                }
            }
        }
        "or_pattern" => {
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_named() {
                    bind_rust_pattern(ch, src, inferred_chain.clone(), impl_type, env);
                }
            }
        }
        "captured_pattern" => {
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_named() {
                    bind_rust_pattern(ch, src, inferred_chain.clone(), impl_type, env);
                }
            }
        }
        "slice_pattern" => {
            let elem_chain = inferred_chain.map(|mut ch| {
                ch.push(ReceiverStep::Elem);
                ch
            });
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_named() {
                    bind_rust_pattern(ch, src, elem_chain.clone(), impl_type, env);
                }
            }
        }
        _ => {}
    }
}

fn bind_rust_tuple_pattern_elements(
    pat: Node<'_>,
    src: &str,
    elem_chains: &[Option<Vec<ReceiverStep>>],
    impl_type: Option<&str>,
    env: &mut RustLocalEnv,
) -> bool {
    match pat.kind() {
        "match_pattern" => {
            let cond_id = pat.child_by_field_name("condition").map(|n| n.id());
            let mut handled = false;
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_named() && Some(ch.id()) != cond_id {
                    handled |=
                        bind_rust_tuple_pattern_elements(ch, src, elem_chains, impl_type, env);
                }
            }
            handled
        }
        "or_pattern" => {
            let mut handled = false;
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_named() {
                    handled |=
                        bind_rust_tuple_pattern_elements(ch, src, elem_chains, impl_type, env);
                }
            }
            handled
        }
        "tuple_pattern" => {
            let mut pos_idx: Option<usize> = Some(0);
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_extra() {
                    continue;
                }
                if ch.kind() == "remaining_field_pattern" {
                    pos_idx = None;
                    continue;
                }
                if ch.kind() == "_" {
                    if let Some(idx) = pos_idx {
                        pos_idx = Some(idx + 1);
                    }
                    continue;
                }
                if ch.is_named() {
                    let chain = pos_idx.and_then(|idx| elem_chains.get(idx).cloned().flatten());
                    if let Some(idx) = pos_idx {
                        pos_idx = Some(idx + 1);
                    }
                    bind_rust_pattern(ch, src, chain, impl_type, env);
                }
            }
            true
        }
        _ => false,
    }
}

fn extract_rust_tuple_expr_chains(
    expr: Node<'_>,
    src: &str,
    env: &RustLocalEnv,
    impl_type: Option<&str>,
) -> Option<Vec<Option<Vec<ReceiverStep>>>> {
    let mut inner = expr;
    while inner.kind() == "parenthesized_expression" {
        if let Some(ch) = inner.named_child(0) {
            inner = ch;
        } else {
            break;
        }
    }
    if inner.kind() != "tuple_expression" {
        return None;
    }
    let mut out = Vec::new();
    let mut c = inner.walk();
    for ch in inner.children(&mut c) {
        if ch.is_named() && !ch.is_extra() {
            out.push(extract_rust_receiver_chain(ch, src, env, impl_type));
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn is_rust_builtin_type(t: &str) -> bool {
    matches!(
        t,
        "String"
            | "Vec"
            | "VecDeque"
            | "Option"
            | "Result"
            | "Box"
            | "Arc"
            | "Rc"
            | "Pin"
            | "Mutex"
            | "RwLock"
            | "RefCell"
            | "Cell"
            | "OnceLock"
            | "OnceCell"
            | "LazyLock"
            | "HashMap"
            | "HashSet"
            | "BTreeMap"
            | "BTreeSet"
            | "IndexMap"
            | "IndexSet"
            | "Cow"
            | "Self"
            | "usize"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "str"
            | "Path"
            | "PathBuf"
            | "Duration"
            | "Instant"
            | "Bytes"
            | "PhantomData"
    )
}

fn is_rust_keyword_or_literal(ident: &str) -> bool {
    matches!(
        ident,
        "self"
            | "Self"
            | "super"
            | "crate"
            | "_"
            | "true"
            | "false"
            | "None"
            | "Some"
            | "Ok"
            | "Err"
            | "if"
            | "else"
            | "match"
            | "for"
            | "while"
            | "loop"
            | "in"
            | "let"
            | "mut"
            | "ref"
            | "const"
            | "static"
            | "fn"
            | "pub"
            | "async"
            | "await"
            | "move"
            | "return"
            | "break"
            | "continue"
            | "as"
            | "where"
            | "impl"
            | "trait"
            | "struct"
            | "enum"
            | "type"
            | "mod"
            | "use"
            | "extern"
            | "unsafe"
            | "dyn"
            | "box"
    )
}

fn has_matching_outer_parens(s: &str) -> bool {
    if !s.starts_with('(') || !s.ends_with(')') || s.len() < 2 {
        return false;
    }
    let mut depth = 0usize;
    let last_idx = s.len() - 1;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 && i != last_idx {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

fn strip_rust_type_prefixes(raw: &str) -> &str {
    let mut s = raw.trim();
    while has_matching_outer_parens(s) {
        let inner = s[1..s.len() - 1].trim();
        if split_top_level_rust_type_list(inner).len() > 1 || inner.contains(',') {
            // Preserve tuple types like `(A, B)` and 1-tuples like `(A,)`.
            let parts = split_top_level_rust_type_list(inner);
            if parts.len() > 1 || inner.ends_with(',') {
                break;
            }
        }
        s = inner;
    }
    loop {
        if let Some(rest) = s.strip_prefix('&') {
            s = rest.trim_start();
            if s.starts_with('\'') {
                if let Some(sp) = s.find(char::is_whitespace) {
                    s = s[sp..].trim_start();
                } else {
                    return "";
                }
            }
            if let Some(rest) = s.strip_prefix("mut ") {
                s = rest.trim_start();
            }
            continue;
        }
        if let Some(rest) = s.strip_prefix("dyn ") {
            s = rest.trim_start();
            continue;
        }
        if let Some(rest) = s.strip_prefix("impl ") {
            s = rest.trim_start();
            continue;
        }
        break;
    }
    s
}

fn extract_rust_base_type_name(raw: &str) -> String {
    let s = strip_rust_type_prefixes(raw);
    let before_gen = s.split('<').next().unwrap_or(s).trim();
    before_gen
        .rsplit("::")
        .next()
        .unwrap_or(before_gen)
        .trim()
        .to_string()
}

fn split_top_level_generic_args(raw: &str) -> Vec<&str> {
    let Some(start) = raw.find('<') else {
        return Vec::new();
    };
    let Some(end) = raw.rfind('>') else {
        return Vec::new();
    };
    if end <= start + 1 {
        return Vec::new();
    }
    let inner = &raw[start + 1..end];
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut seg_start = 0usize;
    for (i, ch) in inner.char_indices() {
        match ch {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                args.push(inner[seg_start..i].trim());
                seg_start = i + 1;
            }
            _ => {}
        }
    }
    let tail = inner[seg_start..].trim();
    if !tail.is_empty() {
        args.push(tail);
    }
    args
}

fn extract_generic_type_arg_names(raw: &str, impl_type: Option<&str>) -> Vec<String> {
    split_top_level_generic_args(raw)
        .into_iter()
        .filter(|a| !a.starts_with('\''))
        .filter_map(|a| {
            let norm = normalize_rust_type_text(a, impl_type);
            let clean = norm.trim_start_matches("[]").to_string();
            if clean.is_empty() { None } else { Some(clean) }
        })
        .collect()
}

fn normalize_rust_type_text(raw: &str, impl_type: Option<&str>) -> String {
    let s = strip_rust_type_prefixes(raw);

    if has_matching_outer_parens(s) {
        let inner = &s[1..s.len() - 1];
        let parts = split_top_level_rust_type_list(inner);
        if parts.len() >= 2 {
            let norms: Vec<String> = parts
                .into_iter()
                .map(|p| normalize_rust_type_text(p, impl_type))
                .collect();
            if norms.iter().any(|p| !p.is_empty()) {
                return format!("({})", norms.join(","));
            }
            return String::new();
        }
    }

    if s.starts_with('[') && s.ends_with(']') && s.len() >= 2 {
        let inner = &s[1..s.len() - 1];
        let elem = inner.split(';').next().unwrap_or(inner).trim();
        let norm = normalize_rust_type_text(elem, impl_type);
        return if norm.is_empty() {
            String::new()
        } else {
            format!("[]{}", norm)
        };
    }

    let base = s.split('<').next().unwrap_or(s).trim();
    let short_base = base.rsplit("::").next().unwrap_or(base).trim();

    if short_base == "Self" {
        return impl_type
            .filter(|t| !is_rust_builtin_type(t))
            .unwrap_or("")
            .to_string();
    }

    let type_args: Vec<&str> = split_top_level_generic_args(s)
        .into_iter()
        .filter(|a| !a.starts_with('\''))
        .collect();

    if !type_args.is_empty()
        && (matches!(
            short_base,
            "Arc"
                | "Rc"
                | "Weak"
                | "Box"
                | "Pin"
                | "Mutex"
                | "RwLock"
                | "RefCell"
                | "Cell"
                | "OnceLock"
                | "OnceCell"
                | "LazyLock"
                | "Option"
                | "Result"
                | "MutexGuard"
                | "RwLockReadGuard"
                | "RwLockWriteGuard"
                | "OwnedMutexGuard"
                | "MappedMutexGuard"
                | "Ref"
                | "RefMut"
                | "ArcSwap"
                | "ArcSwapOption"
                | "Cow"
                | "ManuallyDrop"
                | "MaybeUninit"
                | "NonNull"
                | "BoxFuture"
                | "LocalBoxFuture"
                | "Receiver"
                | "UnboundedReceiver"
        ) || short_base.ends_with("Result")
            || short_base.ends_with("Mutex")
            || short_base.ends_with("RwLock"))
    {
        if let Some(first) = type_args.first() {
            return normalize_rust_type_text(first, impl_type);
        }
        return String::new();
    }

    if matches!(
        short_base,
        "Vec"
            | "VecDeque"
            | "HashSet"
            | "BTreeSet"
            | "IndexSet"
            | "BinaryHeap"
            | "LinkedList"
            | "SmallVec"
    ) {
        if let Some(first) = type_args.first() {
            let norm = normalize_rust_type_text(first, impl_type);
            return if norm.is_empty() {
                String::new()
            } else if norm.starts_with("[]") {
                norm
            } else {
                format!("[]{}", norm)
            };
        }
        return String::new();
    }

    if matches!(short_base, "HashMap" | "BTreeMap" | "IndexMap" | "DashMap") {
        if let Some(val_ty) = type_args.get(1) {
            let norm = normalize_rust_type_text(val_ty, impl_type);
            return if norm.is_empty() {
                String::new()
            } else {
                format!("[]{}", norm)
            };
        }
        return String::new();
    }

    if matches!(short_base, "Future" | "Stream") {
        for arg in type_args {
            if let Some((_, rhs)) = arg.split_once('=') {
                return normalize_rust_type_text(rhs, impl_type);
            }
        }
        return String::new();
    }

    if short_base.is_empty()
        || is_rust_builtin_type(short_base)
        || !short_base
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
    {
        return String::new();
    }
    // Preserve module qualifier (e.g. `remote::Worker`) while dropping `crate`/`super`/`self`.
    let segs: Vec<&str> = base
        .split("::")
        .map(str::trim)
        .filter(|p| !p.is_empty() && !matches!(*p, "crate" | "super" | "self"))
        .collect();
    if segs.len() >= 2 {
        segs.join("::")
    } else {
        short_base.to_string()
    }
}

fn extract_rust_result_err_type(raw: &str, impl_type: Option<&str>) -> String {
    let s = strip_rust_type_prefixes(raw);
    let base = s.split('<').next().unwrap_or(s).trim();
    let short_base = base.rsplit("::").next().unwrap_or(base).trim();
    let type_args: Vec<&str> = split_top_level_generic_args(s)
        .into_iter()
        .filter(|a| !a.starts_with('\''))
        .collect();
    if (short_base == "Result" || short_base.ends_with("Result")) && type_args.len() >= 2 {
        return normalize_rust_type_text(type_args[1], impl_type);
    }
    if matches!(short_base, "BoxFuture" | "LocalBoxFuture" | "Pin" | "Box")
        && let Some(first) = type_args.first()
    {
        return extract_rust_result_err_type(first, impl_type);
    }
    if matches!(short_base, "Future" | "Stream") {
        for arg in type_args {
            if let Some((_, rhs)) = arg.split_once('=') {
                return extract_rust_result_err_type(rhs, impl_type);
            }
        }
    }
    String::new()
}

fn normalize_rust_return_types(raw: &str, impl_type: Option<&str>) -> Vec<String> {
    let ok = normalize_rust_type_text(raw, impl_type);
    let err = extract_rust_result_err_type(raw, impl_type);
    if !err.is_empty() {
        vec![ok, err]
    } else if !ok.is_empty() {
        vec![ok]
    } else {
        Vec::new()
    }
}

fn is_rust_const_ident(name: &str) -> bool {
    name.len() >= 2
        && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && name.chars().any(|c| c.is_ascii_uppercase())
}

fn is_transparent_wrapper_ctor(ftxt: &str) -> bool {
    matches!(ftxt, "Some" | "Ok")
        || ftxt.ends_with("Arc::new")
        || ftxt.ends_with("Arc::clone")
        || ftxt.ends_with("Arc::downgrade")
        || ftxt.ends_with("Rc::new")
        || ftxt.ends_with("Rc::clone")
        || ftxt.ends_with("Rc::downgrade")
        || ftxt.ends_with("Box::new")
        || ftxt.ends_with("Box::pin")
        || ftxt.ends_with("Pin::new")
        || ftxt.ends_with("Mutex::new")
        || ftxt.ends_with("RwLock::new")
        || ftxt.ends_with("RefCell::new")
        || ftxt.ends_with("Cell::new")
        || ftxt.ends_with("ArcSwap::new")
        || ftxt.ends_with("mem::take")
        || ftxt.ends_with("mem::replace")
}

fn extract_rust_receiver_chain(
    expr: Node<'_>,
    src: &str,
    env: &RustLocalEnv,
    impl_type: Option<&str>,
) -> Option<Vec<ReceiverStep>> {
    match expr.kind() {
        "parenthesized_expression" | "await_expression" | "try_expression" => expr
            .named_child(0)
            .and_then(|inner| extract_rust_receiver_chain(inner, src, env, impl_type)),
        "reference_expression" => expr
            .child_by_field_name("value")
            .or_else(|| expr.named_child(0))
            .and_then(|inner| extract_rust_receiver_chain(inner, src, env, impl_type)),
        "unary_expression" => {
            if node_text(expr, src).trim_start().starts_with('*') {
                expr.child_by_field_name("value")
                    .or_else(|| expr.named_child(0))
                    .and_then(|inner| extract_rust_receiver_chain(inner, src, env, impl_type))
            } else {
                None
            }
        }
        "block" => {
            let mut block_env = env.clone();
            block_env.push_scope();
            let mut last_expr = None;
            let mut c = expr.walk();
            for ch in expr.children(&mut c) {
                if !ch.is_named() || matches!(ch.kind(), "line_comment" | "block_comment") {
                    continue;
                }
                if ch.kind() == "let_declaration" {
                    let tuple_chains = if ch.child_by_field_name("type").is_none() {
                        ch.child_by_field_name("value").and_then(|v| {
                            extract_rust_tuple_expr_chains(v, src, &block_env, impl_type)
                        })
                    } else {
                        None
                    };
                    let inferred = ch
                        .child_by_field_name("type")
                        .and_then(|t| {
                            let norm = normalize_rust_type_text(node_text(t, src), impl_type);
                            if norm.is_empty() {
                                None
                            } else {
                                Some(vec![ReceiverStep::Type(norm)])
                            }
                        })
                        .or_else(|| {
                            ch.child_by_field_name("value").and_then(|v| {
                                extract_rust_receiver_chain(v, src, &block_env, impl_type)
                            })
                        });
                    if let Some(pat) = ch.child_by_field_name("pattern") {
                        let handled = tuple_chains.as_deref().is_some_and(|elems| {
                            bind_rust_tuple_pattern_elements(
                                pat,
                                src,
                                elems,
                                impl_type,
                                &mut block_env,
                            )
                        });
                        if !handled {
                            bind_rust_pattern(pat, src, inferred, impl_type, &mut block_env);
                        }
                    }
                    last_expr = None;
                } else {
                    last_expr = Some(ch);
                }
            }
            let last = last_expr?;
            if matches!(last.kind(), "expression_statement" | "let_declaration") {
                None
            } else {
                extract_rust_receiver_chain(last, src, &block_env, impl_type)
            }
        }
        "if_expression" => {
            let mut cond_env = env.clone();
            cond_env.push_scope();
            if let Some(cond) = expr.child_by_field_name("condition") {
                bind_rust_condition_patterns(cond, src, impl_type, &mut cond_env);
            }
            expr.child_by_field_name("consequence")
                .and_then(|c| extract_rust_receiver_chain(c, src, &cond_env, impl_type))
                .or_else(|| {
                    let alt = expr.child_by_field_name("alternative")?;
                    let inner = alt.named_child(0).unwrap_or(alt);
                    extract_rust_receiver_chain(inner, src, env, impl_type)
                })
        }
        "match_expression" => {
            let (scrutinee, tuple_scrutinee) = if let Some(v) = expr.child_by_field_name("value") {
                (
                    extract_rust_receiver_chain(v, src, env, impl_type),
                    extract_rust_tuple_expr_chains(v, src, env, impl_type),
                )
            } else {
                (None, None)
            };
            let body = expr.child_by_field_name("body")?;
            let mut c = body.walk();
            for ch in body.children(&mut c) {
                if ch.kind() == "match_arm"
                    && let Some(val) = ch.child_by_field_name("value")
                {
                    let mut arm_env = env.clone();
                    arm_env.push_scope();
                    if let Some(pat) = ch.child_by_field_name("pattern") {
                        let handled = tuple_scrutinee.as_deref().is_some_and(|elems| {
                            bind_rust_tuple_pattern_elements(
                                pat,
                                src,
                                elems,
                                impl_type,
                                &mut arm_env,
                            )
                        });
                        if !handled {
                            bind_rust_pattern(pat, src, scrutinee.clone(), impl_type, &mut arm_env);
                        }
                    }
                    if let Some(chain) = extract_rust_receiver_chain(val, src, &arm_env, impl_type)
                    {
                        return Some(chain);
                    }
                }
            }
            None
        }
        "self" => impl_type.map(|t| vec![ReceiverStep::Type(t.to_string())]),
        "identifier" => {
            let name = node_text(expr, src);
            if name == "self" {
                impl_type.map(|t| vec![ReceiverStep::Type(t.to_string())])
            } else if let Some(ch) = env.lookup_chain(name) {
                Some(ch)
            } else if !env.is_shadowed(name) && is_rust_const_ident(name) {
                Some(vec![ReceiverStep::Call {
                    qualifier: None,
                    name: name.to_string(),
                    result_index: 0,
                }])
            } else {
                None
            }
        }
        "scoped_identifier" => {
            let txt = strip_balanced_generics(node_text(expr, src));
            let parts: Vec<&str> = txt.split("::").filter(|p| !p.is_empty()).collect();
            if parts.len() >= 2 {
                let last = *parts.last()?;
                let prev = parts[parts.len() - 2];
                if is_rust_const_ident(last) {
                    let (qual, name) = split_rust_scoped_callee(&txt, impl_type)?;
                    return Some(vec![ReceiverStep::Call {
                        qualifier: qual,
                        name,
                        result_index: 0,
                    }]);
                }
                if prev == "Self" || prev.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    let enum_path = parts[..parts.len() - 1].join("::");
                    let norm = normalize_rust_type_text(&enum_path, impl_type);
                    let resolved = norm.trim_start_matches("[]").to_string();
                    if !resolved.is_empty() {
                        return Some(vec![ReceiverStep::Type(resolved)]);
                    }
                }
            }
            None
        }
        "struct_expression" => {
            let name_node = expr.child_by_field_name("name")?;
            let norm = normalize_rust_type_text(node_text(name_node, src), impl_type);
            let resolved = norm.trim_start_matches("[]").to_string();
            if resolved.is_empty() {
                None
            } else {
                Some(vec![ReceiverStep::Type(resolved)])
            }
        }
        "field_expression" => {
            let value = expr.child_by_field_name("value")?;
            let field = expr.child_by_field_name("field")?;
            let mut chain = extract_rust_receiver_chain(value, src, env, impl_type)?;
            chain.push(ReceiverStep::Field(node_text(field, src).to_string()));
            Some(chain)
        }
        "index_expression" => {
            let obj = expr.named_child(0)?;
            let mut chain = extract_rust_receiver_chain(obj, src, env, impl_type)?;
            chain.push(ReceiverStep::Elem);
            Some(chain)
        }
        "call_expression" => {
            let raw_func = expr.child_by_field_name("function")?;
            let func = if raw_func.kind() == "generic_function" {
                raw_func.child_by_field_name("function").unwrap_or(raw_func)
            } else {
                raw_func
            };
            let ftxt = node_text(func, src);
            if is_transparent_wrapper_ctor(ftxt) {
                let args = expr.child_by_field_name("arguments")?;
                let first_arg = args.named_child(0)?;
                return extract_rust_receiver_chain(first_arg, src, env, impl_type);
            }
            let short_callee = match func.kind() {
                "field_expression" => func
                    .child_by_field_name("field")
                    .map(|f| node_text(f, src))
                    .unwrap_or(""),
                "scoped_identifier" => ftxt.rsplit("::").next().unwrap_or(ftxt).trim(),
                "identifier" => ftxt.trim(),
                _ => "",
            };
            // Generic TypeMap/downcast/parse calls: `ext.get::<MyType>()`, `any.downcast_mut::<T>()`.
            if raw_func.kind() == "generic_function"
                && matches!(
                    short_callee,
                    "get"
                        | "get_mut"
                        | "remove"
                        | "take"
                        | "downcast_ref"
                        | "downcast_mut"
                        | "downcast"
                        | "parse"
                        | "get_or_init"
                        | "get_or_insert"
                        | "get_or_insert_with"
                )
                && let Some(targs) = raw_func.child_by_field_name("type_arguments")
            {
                for arg in split_top_level_generic_args(node_text(targs, src)) {
                    if arg.starts_with('\'') {
                        continue;
                    }
                    let norm = normalize_rust_type_text(arg, impl_type);
                    if !norm.is_empty() {
                        return Some(vec![ReceiverStep::Type(norm)]);
                    }
                }
            }
            // Transparent async combinators wrapping a future argument.
            if matches!(short_callee, "run_until_cancelled" | "timeout")
                && let Some(args) = expr.child_by_field_name("arguments")
            {
                let arg_idx = if short_callee == "timeout" { 1 } else { 0 };
                if let Some(fut_arg) = args.named_child(arg_idx)
                    && let Some(ch) = extract_rust_receiver_chain(fut_arg, src, env, impl_type)
                {
                    return Some(ch);
                }
            }
            match func.kind() {
                "field_expression" => {
                    let recv = func.child_by_field_name("value")?;
                    let mname = node_text(func.child_by_field_name("field")?, src);
                    if let Some(mut chain) = extract_rust_receiver_chain(recv, src, env, impl_type)
                    {
                        chain.push(ReceiverStep::MethodCall {
                            name: mname.to_string(),
                            result_index: 0,
                        });
                        return Some(chain);
                    }
                    // Fallback for external container `get_or_init`/`get_or_insert`/`get_or_insert_with`
                    // when receiver type is external (e.g. TypeMap / Extensions).
                    if matches!(
                        mname,
                        "get_or_init" | "get_or_insert" | "get_or_insert_with"
                    ) && let Some(args) = expr.child_by_field_name("arguments")
                        && let Some(arg0) = args.named_child(0)
                    {
                        if arg0.kind() == "closure_expression"
                            && let Some(body) = arg0.child_by_field_name("body")
                        {
                            return extract_rust_receiver_chain(body, src, env, impl_type);
                        }
                        if arg0.kind() == "scoped_identifier" {
                            let clean = strip_balanced_generics(node_text(arg0, src));
                            if let Some((prefix, _)) = clean.rsplit_once("::") {
                                let norm = normalize_rust_type_text(prefix, impl_type);
                                if !norm.is_empty() {
                                    return Some(vec![ReceiverStep::Type(norm)]);
                                }
                            }
                        }
                        return extract_rust_receiver_chain(arg0, src, env, impl_type);
                    }
                    None
                }
                "scoped_identifier" => {
                    let (qual, name) = split_rust_scoped_callee(ftxt, impl_type)?;
                    Some(vec![ReceiverStep::Call {
                        qualifier: qual,
                        name,
                        result_index: 0,
                    }])
                }
                "identifier" => Some(vec![ReceiverStep::Call {
                    qualifier: None,
                    name: ftxt.to_string(),
                    result_index: 0,
                }]),
                _ => None,
            }
        }
        _ => None,
    }
}

fn collect_type_refs(
    node: Node<'_>,
    src: &str,
    from_id: &str,
    impl_type: Option<&str>,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "scoped_type_identifier" => {
                emit_scoped_type_ref(
                    node_text(n, src),
                    from_id,
                    impl_type,
                    n.start_position().row + 1,
                    refs,
                );
                continue;
            }
            "type_identifier" => {
                let t = node_text(n, src);
                if !is_rust_builtin_type(t) {
                    refs.push(RawRef {
                        from_symbol_id: from_id.to_string(),
                        target_name: t.to_string(),
                        qualifier: None,
                        kind: EdgeKind::TypeRef,
                        line: n.start_position().row + 1,
                        receiver_chain: None,
                    });
                }
            }
            "array_type" => {
                if let Some(elem) = n.child_by_field_name("element") {
                    stack.push(elem);
                }
                if let Some(len_expr) = n.child_by_field_name("length") {
                    let mut env = RustLocalEnv::new();
                    walk_rust_expr(len_expr, src, from_id, impl_type, &mut env, refs, bridges);
                }
                continue;
            }
            "enum_variant" => {
                if let Some(body) = n.child_by_field_name("body") {
                    stack.push(body);
                }
                if let Some(val_expr) = n.child_by_field_name("value") {
                    let mut env = RustLocalEnv::new();
                    walk_rust_expr(val_expr, src, from_id, impl_type, &mut env, refs, bridges);
                }
                continue;
            }
            "const_block" => {
                let mut env = RustLocalEnv::new();
                walk_rust_expr(n, src, from_id, impl_type, &mut env, refs, bridges);
                continue;
            }
            _ => {}
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
}

fn walk_rust_expr(
    node: Node<'_>,
    src: &str,
    from_id: &str,
    impl_type: Option<&str>,
    env: &mut RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let line = node.start_position().row + 1;
    match node.kind() {
        "block" => {
            env.push_scope();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                walk_rust_expr(ch, src, from_id, impl_type, env, refs, bridges);
            }
            env.pop_scope();
        }
        "let_declaration" => {
            if let Some(ty) = node.child_by_field_name("type") {
                collect_type_refs(ty, src, from_id, impl_type, refs, bridges);
            }
            if let Some(val) = node.child_by_field_name("value") {
                walk_rust_expr(val, src, from_id, impl_type, env, refs, bridges);
            }
            if let Some(alt) = node.child_by_field_name("alternative") {
                walk_rust_expr(alt, src, from_id, impl_type, env, refs, bridges);
            }
            let tuple_chains = if node.child_by_field_name("type").is_none() {
                node.child_by_field_name("value")
                    .and_then(|v| extract_rust_tuple_expr_chains(v, src, env, impl_type))
            } else {
                None
            };
            let inferred = node
                .child_by_field_name("type")
                .and_then(|t| {
                    let norm = normalize_rust_type_text(node_text(t, src), impl_type);
                    if norm.is_empty() {
                        None
                    } else {
                        Some(vec![ReceiverStep::Type(norm)])
                    }
                })
                .or_else(|| {
                    node.child_by_field_name("value")
                        .and_then(|v| extract_rust_receiver_chain(v, src, env, impl_type))
                });
            if let Some(pat) = node.child_by_field_name("pattern") {
                collect_pattern_type_refs(pat, src, from_id, refs);
                let handled = tuple_chains.as_deref().is_some_and(|elems| {
                    bind_rust_tuple_pattern_elements(pat, src, elems, impl_type, env)
                });
                if !handled {
                    bind_rust_pattern(pat, src, inferred, impl_type, env);
                }
            }
        }
        "if_expression" => {
            env.push_scope();
            if let Some(cond) = node.child_by_field_name("condition") {
                walk_rust_condition(cond, src, from_id, impl_type, env, refs, bridges);
            }
            if let Some(cons) = node.child_by_field_name("consequence") {
                walk_rust_expr(cons, src, from_id, impl_type, env, refs, bridges);
            }
            env.pop_scope();
            if let Some(alt) = node.child_by_field_name("alternative") {
                walk_rust_expr(alt, src, from_id, impl_type, env, refs, bridges);
            }
        }
        "while_expression" => {
            env.push_scope();
            if let Some(cond) = node.child_by_field_name("condition") {
                walk_rust_condition(cond, src, from_id, impl_type, env, refs, bridges);
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_rust_expr(body, src, from_id, impl_type, env, refs, bridges);
            }
            env.pop_scope();
        }
        "for_expression" => {
            let elem_chain = node.child_by_field_name("value").and_then(|v| {
                walk_rust_expr(v, src, from_id, impl_type, env, refs, bridges);
                let mut ch = extract_rust_receiver_chain(v, src, env, impl_type)?;
                ch.push(ReceiverStep::Elem);
                Some(ch)
            });
            env.push_scope();
            if let Some(pat) = node.child_by_field_name("pattern") {
                bind_rust_pattern(pat, src, elem_chain, impl_type, env);
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_rust_expr(body, src, from_id, impl_type, env, refs, bridges);
            }
            env.pop_scope();
        }
        "closure_expression" => {
            walk_rust_closure(
                node,
                src,
                from_id,
                impl_type,
                env,
                refs,
                bridges,
                &ClosureParamHints::None,
            );
        }
        "match_expression" => {
            let (scrutinee, tuple_scrutinee) = if let Some(v) = node.child_by_field_name("value") {
                walk_rust_expr(v, src, from_id, impl_type, env, refs, bridges);
                (
                    extract_rust_receiver_chain(v, src, env, impl_type),
                    extract_rust_tuple_expr_chains(v, src, env, impl_type),
                )
            } else {
                (None, None)
            };
            if let Some(body) = node.child_by_field_name("body") {
                let mut c = body.walk();
                for ch in body.children(&mut c) {
                    if ch.kind() == "match_arm" {
                        walk_rust_match_arm(
                            ch,
                            src,
                            from_id,
                            impl_type,
                            scrutinee.clone(),
                            tuple_scrutinee.as_deref(),
                            env,
                            refs,
                            bridges,
                        );
                    } else {
                        walk_rust_expr(ch, src, from_id, impl_type, env, refs, bridges);
                    }
                }
            }
        }
        "match_arm" => {
            walk_rust_match_arm(
                node, src, from_id, impl_type, None, None, env, refs, bridges,
            );
        }
        "call_expression" => {
            let mut closure_param_hint: Option<(usize, Vec<ReceiverStep>)> = None;
            let mut callee_closure_target: Option<ClosureCalleeTarget> = None;
            if let Some(raw_func) = node.child_by_field_name("function") {
                let func = if raw_func.kind() == "generic_function" {
                    if let Some(targs) = raw_func.child_by_field_name("type_arguments") {
                        collect_type_refs(targs, src, from_id, impl_type, refs, bridges);
                    }
                    raw_func.child_by_field_name("function").unwrap_or(raw_func)
                } else {
                    raw_func
                };
                let ftxt = node_text(func, src);
                if (ftxt.ends_with("env::var") || ftxt.ends_with("env::var_os"))
                    && let Some(arg) = first_rust_str_arg(node, src)
                {
                    bridges.push(BridgeHint {
                        from_symbol_id: from_id.to_string(),
                        bridge_kind: BridgeKind::EnvKey,
                        key: arg,
                        role: BridgeRole::Consumer,
                        line,
                        confidence: Confidence::Resolved,
                    });
                } else if ftxt.ends_with("Command::new")
                    && let Some(arg) = first_rust_str_arg(node, src)
                {
                    bridges.push(BridgeHint {
                        from_symbol_id: from_id.to_string(),
                        bridge_kind: BridgeKind::ExecTarget,
                        key: arg,
                        role: BridgeRole::Consumer,
                        line,
                        confidence: Confidence::Resolved,
                    });
                }
                match func.kind() {
                    "identifier" => {
                        if !env.is_shadowed(ftxt) && !is_rust_keyword_or_literal(ftxt) {
                            callee_closure_target = Some(ClosureCalleeTarget::Func {
                                qualifier: None,
                                name: ftxt.to_string(),
                            });
                            refs.push(RawRef {
                                from_symbol_id: from_id.to_string(),
                                target_name: ftxt.to_string(),
                                qualifier: None,
                                kind: EdgeKind::Calls,
                                line,
                                receiver_chain: None,
                            });
                        }
                    }
                    "field_expression" => {
                        if let Some(val) = func.child_by_field_name("value") {
                            walk_rust_expr(val, src, from_id, impl_type, env, refs, bridges);
                        }
                        if let Some(field) = func.child_by_field_name("field") {
                            let val_node = func.child_by_field_name("value");
                            let qual = val_node.map(|v| node_text(v, src).to_string());
                            let chain = val_node
                                .and_then(|v| extract_rust_receiver_chain(v, src, env, impl_type));
                            let mname = node_text(field, src);
                            if let Some(ref recv_chain) = chain {
                                closure_param_hint = infer_closure_param_hint(recv_chain, mname);
                                callee_closure_target = Some(ClosureCalleeTarget::Method {
                                    recv_chain: recv_chain.clone(),
                                    name: mname.to_string(),
                                });
                            }
                            refs.push(RawRef {
                                from_symbol_id: from_id.to_string(),
                                target_name: mname.to_string(),
                                qualifier: qual,
                                kind: EdgeKind::Calls,
                                line,
                                receiver_chain: chain,
                            });
                        }
                    }
                    "scoped_identifier" => {
                        emit_scoped_call_ref(from_id, ftxt, impl_type, line, refs);
                        if let Some((qual, name)) = split_rust_scoped_callee(ftxt, impl_type) {
                            callee_closure_target = Some(ClosureCalleeTarget::Func {
                                qualifier: qual,
                                name,
                            });
                        }
                    }
                    _ => {
                        walk_rust_expr(func, src, from_id, impl_type, env, refs, bridges);
                    }
                }
            }
            if let Some(args) = node.child_by_field_name("arguments") {
                if closure_param_hint.is_some() || callee_closure_target.is_some() {
                    let mut ac = args.walk();
                    let mut named_idx = 0usize;
                    for arg in args.children(&mut ac) {
                        if !arg.is_named() || arg.is_extra() {
                            continue;
                        }
                        let idx = named_idx;
                        named_idx += 1;
                        if arg.kind() == "closure_expression" {
                            let hints = if let Some((target_arg_idx, ref hint_chain)) =
                                closure_param_hint
                                && idx == target_arg_idx
                            {
                                ClosureParamHints::First(hint_chain.clone())
                            } else if let Some(ref target) = callee_closure_target {
                                ClosureParamHints::Callee {
                                    target: target.clone(),
                                    arg_index: idx,
                                }
                            } else {
                                ClosureParamHints::None
                            };
                            walk_rust_closure(
                                arg, src, from_id, impl_type, env, refs, bridges, &hints,
                            );
                        } else {
                            walk_rust_expr(arg, src, from_id, impl_type, env, refs, bridges);
                        }
                    }
                } else {
                    walk_rust_expr(args, src, from_id, impl_type, env, refs, bridges);
                }
            }
        }
        "macro_invocation" => {
            let mut is_select_macro = false;
            if let Some(mnode) = node.child_by_field_name("macro") {
                let mtxt = node_text(mnode, src);
                let mname = mtxt.rsplit("::").next().unwrap_or(mtxt).trim();
                is_select_macro = mname == "select";
                if !mname.is_empty() {
                    refs.push(RawRef {
                        from_symbol_id: from_id.to_string(),
                        target_name: format!("{}!", mname),
                        qualifier: None,
                        kind: EdgeKind::Calls,
                        line,
                        receiver_chain: None,
                    });
                }
            }
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.kind() == "token_tree" {
                    if is_select_macro {
                        walk_rust_select_macro(ch, src, from_id, impl_type, env, refs, bridges);
                    } else {
                        scan_rust_token_tree(ch, src, from_id, impl_type, env, refs, bridges);
                    }
                }
            }
        }
        "field_expression" => {
            if let Some(val) = node.child_by_field_name("value") {
                walk_rust_expr(val, src, from_id, impl_type, env, refs, bridges);
            }
        }
        "field_initializer" => {
            if let Some(val) = node.child_by_field_name("value") {
                walk_rust_expr(val, src, from_id, impl_type, env, refs, bridges);
            }
        }
        "shorthand_field_initializer" => {
            let ident = node_text(node, src).trim();
            if !env.is_shadowed(ident)
                && !is_rust_keyword_or_literal(ident)
                && ident.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            {
                refs.push(RawRef {
                    from_symbol_id: from_id.to_string(),
                    target_name: ident.to_string(),
                    qualifier: None,
                    kind: EdgeKind::Calls,
                    line,
                    receiver_chain: None,
                });
            }
        }
        "generic_function" => {
            if let Some(targs) = node.child_by_field_name("type_arguments") {
                collect_type_refs(targs, src, from_id, impl_type, refs, bridges);
            }
            if let Some(inner) = node.child_by_field_name("function") {
                walk_rust_expr(inner, src, from_id, impl_type, env, refs, bridges);
            }
        }
        "scoped_identifier" => {
            emit_scoped_call_ref(from_id, node_text(node, src), impl_type, line, refs);
        }
        "scoped_type_identifier" => {
            emit_scoped_type_ref(node_text(node, src), from_id, impl_type, line, refs);
        }
        "identifier" => {
            let name = node_text(node, src);
            if !env.is_shadowed(name) && !is_rust_keyword_or_literal(name) {
                if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    if !is_rust_builtin_type(name) {
                        refs.push(RawRef {
                            from_symbol_id: from_id.to_string(),
                            target_name: name.to_string(),
                            qualifier: None,
                            kind: EdgeKind::TypeRef,
                            line,
                            receiver_chain: None,
                        });
                    }
                } else {
                    refs.push(RawRef {
                        from_symbol_id: from_id.to_string(),
                        target_name: name.to_string(),
                        qualifier: None,
                        kind: EdgeKind::Calls,
                        line,
                        receiver_chain: None,
                    });
                }
            }
        }
        "type_identifier" => {
            let t = node_text(node, src);
            if !is_rust_builtin_type(t) {
                refs.push(RawRef {
                    from_symbol_id: from_id.to_string(),
                    target_name: t.to_string(),
                    qualifier: None,
                    kind: EdgeKind::TypeRef,
                    line,
                    receiver_chain: None,
                });
            }
        }
        "string_literal" | "raw_string_literal" => {
            let raw = node_text(node, src);
            if looks_like_wire_method(raw) {
                bridges.push(BridgeHint {
                    from_symbol_id: from_id.to_string(),
                    bridge_kind: BridgeKind::WireContract,
                    key: format!("rpc:{}", strip_quotes(raw)),
                    role: BridgeRole::Both,
                    line,
                    confidence: Confidence::Heuristic,
                });
            }
        }
        "attribute_item" => {
            extract_rust_attr_node_refs(node, src, from_id, impl_type, env, refs, bridges);
        }
        "use_declaration" | "line_comment" | "block_comment" | "lifetime" | "label" => {}
        _ => {
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                walk_rust_expr(ch, src, from_id, impl_type, env, refs, bridges);
            }
        }
    }
}

fn bind_rust_condition_patterns(
    cond: Node<'_>,
    src: &str,
    impl_type: Option<&str>,
    env: &mut RustLocalEnv,
) {
    match cond.kind() {
        "let_condition" => {
            let (inferred, tuple_chains) = if let Some(v) = cond.child_by_field_name("value") {
                (
                    extract_rust_receiver_chain(v, src, env, impl_type),
                    extract_rust_tuple_expr_chains(v, src, env, impl_type),
                )
            } else {
                (None, None)
            };
            if let Some(pat) = cond.child_by_field_name("pattern") {
                let handled = tuple_chains.as_deref().is_some_and(|elems| {
                    bind_rust_tuple_pattern_elements(pat, src, elems, impl_type, env)
                });
                if !handled {
                    bind_rust_pattern(pat, src, inferred, impl_type, env);
                }
            }
        }
        "let_chain" => {
            let mut c = cond.walk();
            for ch in cond.children(&mut c) {
                if ch.is_named() {
                    bind_rust_condition_patterns(ch, src, impl_type, env);
                }
            }
        }
        _ => {}
    }
}

fn walk_rust_condition(
    cond: Node<'_>,
    src: &str,
    from_id: &str,
    impl_type: Option<&str>,
    env: &mut RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    match cond.kind() {
        "let_condition" => {
            let (inferred, tuple_chains) = if let Some(v) = cond.child_by_field_name("value") {
                walk_rust_expr(v, src, from_id, impl_type, env, refs, bridges);
                (
                    extract_rust_receiver_chain(v, src, env, impl_type),
                    extract_rust_tuple_expr_chains(v, src, env, impl_type),
                )
            } else {
                (None, None)
            };
            if let Some(pat) = cond.child_by_field_name("pattern") {
                collect_pattern_type_refs(pat, src, from_id, refs);
                let handled = tuple_chains.as_deref().is_some_and(|elems| {
                    bind_rust_tuple_pattern_elements(pat, src, elems, impl_type, env)
                });
                if !handled {
                    bind_rust_pattern(pat, src, inferred, impl_type, env);
                }
            }
        }
        "let_chain" => {
            let mut c = cond.walk();
            for ch in cond.children(&mut c) {
                if ch.is_named() {
                    walk_rust_condition(ch, src, from_id, impl_type, env, refs, bridges);
                }
            }
        }
        _ => {
            walk_rust_expr(cond, src, from_id, impl_type, env, refs, bridges);
        }
    }
}

fn clean_rust_use_path(raw: &str) -> String {
    raw.split("::")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("::")
}

fn join_rust_use_path(prefix: &str, tail: &str) -> String {
    match (prefix.is_empty(), tail.is_empty()) {
        (true, _) => tail.to_string(),
        (_, true) => prefix.to_string(),
        (false, false) => format!("{}::{}", prefix, tail),
    }
}

fn rust_type_is_wrapper(raw: &str) -> bool {
    let s = strip_rust_type_prefixes(raw);
    if s.starts_with('[') && s.ends_with(']') {
        return true;
    }
    let base = s.split('<').next().unwrap_or(s).trim();
    let short_base = base.rsplit("::").next().unwrap_or(base).trim();
    let has_generic_args = s.contains('<');
    has_generic_args
        && (matches!(
            short_base,
            "Arc"
                | "Rc"
                | "Weak"
                | "Box"
                | "Pin"
                | "Mutex"
                | "RwLock"
                | "RefCell"
                | "Cell"
                | "OnceLock"
                | "OnceCell"
                | "LazyLock"
                | "Option"
                | "Result"
                | "MutexGuard"
                | "RwLockReadGuard"
                | "RwLockWriteGuard"
                | "OwnedMutexGuard"
                | "MappedMutexGuard"
                | "Ref"
                | "RefMut"
                | "ArcSwap"
                | "ArcSwapOption"
                | "Cow"
                | "ManuallyDrop"
                | "MaybeUninit"
                | "NonNull"
                | "BoxFuture"
                | "LocalBoxFuture"
                | "Receiver"
                | "UnboundedReceiver"
                | "Vec"
                | "VecDeque"
                | "HashSet"
                | "BTreeSet"
                | "IndexSet"
                | "BinaryHeap"
                | "LinkedList"
                | "SmallVec"
                | "HashMap"
                | "BTreeMap"
                | "IndexMap"
                | "DashMap"
        ) || short_base.ends_with("Result")
            || short_base.ends_with("Mutex")
            || short_base.ends_with("RwLock"))
}

fn extract_rust_use_items(
    node: Node<'_>,
    src: &str,
    prefix: &str,
    mod_scope: &str,
    is_pub: bool,
    is_test: bool,
    out: &mut Vec<RustUseItem>,
) {
    match node.kind() {
        "identifier" | "crate" | "super" | "self" => {
            let name = node_text(node, src).trim();
            if name == "self" {
                if !prefix.is_empty()
                    && let Some(local) = prefix.rsplit("::").next()
                {
                    out.push(RustUseItem {
                        local_name: local.to_string(),
                        target_path: prefix.to_string(),
                        mod_scope: mod_scope.to_string(),
                        is_pub,
                        is_test,
                    });
                }
            } else if !name.is_empty() {
                let full = join_rust_use_path(prefix, name);
                out.push(RustUseItem {
                    local_name: name.to_string(),
                    target_path: full,
                    mod_scope: mod_scope.to_string(),
                    is_pub,
                    is_test,
                });
            }
        }
        "scoped_identifier" => {
            let cleaned = clean_rust_use_path(node_text(node, src));
            if !cleaned.is_empty() {
                let full = join_rust_use_path(prefix, &cleaned);
                if let Some(local) = full.rsplit("::").next() {
                    out.push(RustUseItem {
                        local_name: local.to_string(),
                        target_path: full,
                        mod_scope: mod_scope.to_string(),
                        is_pub,
                        is_test,
                    });
                }
            }
        }
        "use_as_clause" => {
            let Some(path_node) = node.child_by_field_name("path") else {
                return;
            };
            let Some(alias_node) = node.child_by_field_name("alias") else {
                return;
            };
            let alias = node_text(alias_node, src).trim();
            if alias.is_empty() || alias == "_" {
                return;
            }
            let ptxt = clean_rust_use_path(node_text(path_node, src));
            let full = if ptxt == "self" {
                prefix.to_string()
            } else {
                join_rust_use_path(prefix, &ptxt)
            };
            if !full.is_empty() {
                out.push(RustUseItem {
                    local_name: alias.to_string(),
                    target_path: full,
                    mod_scope: mod_scope.to_string(),
                    is_pub,
                    is_test,
                });
            }
        }
        "use_wildcard" => {
            let mut c = node.walk();
            let mut sub = String::new();
            for ch in node.children(&mut c) {
                if ch.is_named() {
                    sub = clean_rust_use_path(node_text(ch, src));
                    break;
                }
            }
            let full = join_rust_use_path(prefix, &sub);
            if !full.is_empty() {
                out.push(RustUseItem {
                    local_name: "*".to_string(),
                    target_path: full,
                    mod_scope: mod_scope.to_string(),
                    is_pub,
                    is_test,
                });
            }
        }
        "scoped_use_list" => {
            let sub = node
                .child_by_field_name("path")
                .map(|p| clean_rust_use_path(node_text(p, src)))
                .unwrap_or_default();
            let next_prefix = join_rust_use_path(prefix, &sub);
            if let Some(list) = node.child_by_field_name("list") {
                extract_rust_use_items(list, src, &next_prefix, mod_scope, is_pub, is_test, out);
            }
        }
        "use_list" => {
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.is_named() {
                    extract_rust_use_items(ch, src, prefix, mod_scope, is_pub, is_test, out);
                }
            }
        }
        _ => {}
    }
}

fn infer_closure_param_hint(
    recv_chain: &[ReceiverStep],
    method: &str,
) -> Option<(usize, Vec<ReceiverStep>)> {
    if matches!(
        method,
        "any"
            | "all"
            | "find"
            | "find_map"
            | "for_each"
            | "filter_map"
            | "flat_map"
            | "position"
            | "rposition"
            | "retain"
            | "try_for_each"
            | "take_while"
            | "skip_while"
            | "partition"
            | "sort_by_key"
            | "max_by_key"
            | "min_by_key"
    ) {
        let mut ch = recv_chain.to_vec();
        ch.push(ReceiverStep::Elem);
        return Some((0, ch));
    }
    if matches!(method, "map" | "inspect" | "filter") {
        let looks_like_iter = recv_chain.last().is_some_and(|step| {
            matches!(
                step,
                ReceiverStep::MethodCall { name, .. }
                    if matches!(
                        name.as_str(),
                        "iter"
                            | "iter_mut"
                            | "into_iter"
                            | "values"
                            | "values_mut"
                            | "into_values"
                            | "drain"
                            | "rev"
                            | "take"
                            | "skip"
                            | "filter"
                            | "cloned"
                            | "copied"
                    )
            )
        }) || (recv_chain.len() == 1
            && matches!(&recv_chain[0], ReceiverStep::Type(t) if t.starts_with("[]")));
        let mut ch = recv_chain.to_vec();
        if looks_like_iter {
            ch.push(ReceiverStep::Elem);
        }
        return Some((0, ch));
    }
    if matches!(method, "and_then" | "is_some_and" | "is_ok_and") {
        return Some((0, recv_chain.to_vec()));
    }
    if matches!(
        method,
        "unwrap_or_else" | "map_err" | "inspect_err" | "or_else"
    ) {
        return chain_with_err_result(Some(recv_chain)).map(|ch| (0, ch));
    }
    if matches!(method, "map_or" | "map_or_else") {
        // Arg 0 is the default value / error callback (E); arg 1 is the Ok/Some mapper (T).
        return Some((1, recv_chain.to_vec()));
    }
    None
}

#[derive(Clone)]
enum ClosureCalleeTarget {
    Method {
        recv_chain: Vec<ReceiverStep>,
        name: String,
    },
    Func {
        qualifier: Option<String>,
        name: String,
    },
}

enum ClosureParamHints {
    None,
    First(Vec<ReceiverStep>),
    Callee {
        target: ClosureCalleeTarget,
        arg_index: usize,
    },
}

impl ClosureParamHints {
    fn hint_for(&self, param_index: usize) -> Option<Vec<ReceiverStep>> {
        match self {
            ClosureParamHints::None => None,
            ClosureParamHints::First(chain) => {
                if param_index == 0 {
                    Some(chain.clone())
                } else {
                    None
                }
            }
            ClosureParamHints::Callee { target, arg_index } => match target {
                ClosureCalleeTarget::Method { recv_chain, name } => {
                    let mut ch = recv_chain.clone();
                    ch.push(ReceiverStep::ClosureParam {
                        qualifier: None,
                        name: name.clone(),
                        arg_index: *arg_index,
                        param_index,
                    });
                    Some(ch)
                }
                ClosureCalleeTarget::Func { qualifier, name } => {
                    Some(vec![ReceiverStep::ClosureParam {
                        qualifier: qualifier.clone(),
                        name: name.clone(),
                        arg_index: *arg_index,
                        param_index,
                    }])
                }
            },
        }
    }
}

fn split_rust_scoped_callee(
    ftxt: &str,
    impl_type: Option<&str>,
) -> Option<(Option<String>, String)> {
    let clean = strip_balanced_generics(ftxt);
    let parts: Vec<&str> = clean.split("::").filter(|p| !p.is_empty()).collect();
    let last = *parts.last()?;
    let qual = match &parts[..parts.len() - 1] {
        [] => None,
        ["Self"] => impl_type.map(|t| t.to_string()),
        ["crate"] | ["super"] | ["self"] => None,
        prefix => Some(prefix.join("::")),
    };
    Some((qual, last.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn walk_rust_match_arm(
    node: Node<'_>,
    src: &str,
    from_id: &str,
    impl_type: Option<&str>,
    scrutinee: Option<Vec<ReceiverStep>>,
    tuple_scrutinee: Option<&[Option<Vec<ReceiverStep>>]>,
    env: &mut RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    env.push_scope();
    if let Some(pat) = node.child_by_field_name("pattern") {
        if pat.kind() == "match_pattern" {
            let cond = pat.child_by_field_name("condition");
            let cond_id = cond.map(|n| n.id());
            let mut pc = pat.walk();
            for pch in pat.children(&mut pc) {
                if pch.is_named() && Some(pch.id()) != cond_id {
                    collect_pattern_type_refs(pch, src, from_id, refs);
                    let handled = tuple_scrutinee.is_some_and(|elems| {
                        bind_rust_tuple_pattern_elements(pch, src, elems, impl_type, env)
                    });
                    if !handled {
                        bind_rust_pattern(pch, src, scrutinee.clone(), impl_type, env);
                    }
                }
            }
            if let Some(guard) = cond {
                walk_rust_expr(guard, src, from_id, impl_type, env, refs, bridges);
            }
        } else {
            collect_pattern_type_refs(pat, src, from_id, refs);
            let handled = tuple_scrutinee.is_some_and(|elems| {
                bind_rust_tuple_pattern_elements(pat, src, elems, impl_type, env)
            });
            if !handled {
                bind_rust_pattern(pat, src, scrutinee, impl_type, env);
            }
        }
    }
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        if Some(ch.id()) == node.child_by_field_name("pattern").map(|n| n.id()) {
            continue;
        }
        walk_rust_expr(ch, src, from_id, impl_type, env, refs, bridges);
    }
    env.pop_scope();
}

#[allow(clippy::too_many_arguments)]
fn walk_rust_closure(
    node: Node<'_>,
    src: &str,
    from_id: &str,
    impl_type: Option<&str>,
    env: &mut RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
    param_hints: &ClosureParamHints,
) {
    env.push_scope();
    if let Some(plist) = node.child_by_field_name("parameters") {
        let mut param_idx = 0usize;
        let mut c = plist.walk();
        for ch in plist.children(&mut c) {
            if ch.is_extra() {
                continue;
            }
            if ch.kind() == "_" {
                param_idx += 1;
                continue;
            }
            if ch.kind() == "parameter" {
                let pty = ch
                    .child_by_field_name("type")
                    .and_then(|t| {
                        collect_type_refs(t, src, from_id, impl_type, refs, bridges);
                        let norm = normalize_rust_type_text(node_text(t, src), impl_type);
                        if norm.is_empty() {
                            None
                        } else {
                            Some(vec![ReceiverStep::Type(norm)])
                        }
                    })
                    .or_else(|| param_hints.hint_for(param_idx));
                if let Some(pat) = ch.child_by_field_name("pattern") {
                    bind_rust_pattern(pat, src, pty, impl_type, env);
                }
                param_idx += 1;
            } else if ch.is_named() {
                let pty = param_hints.hint_for(param_idx);
                bind_rust_pattern(ch, src, pty, impl_type, env);
                param_idx += 1;
            }
        }
    }
    if let Some(body) = node.child_by_field_name("body") {
        walk_rust_expr(body, src, from_id, impl_type, env, refs, bridges);
    }
    env.pop_scope();
}

fn collect_pattern_type_refs(pat: Node<'_>, src: &str, from_id: &str, refs: &mut Vec<RawRef>) {
    let mut stack = vec![pat];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "type_identifier" => {
                let t = node_text(n, src);
                if !is_rust_builtin_type(t) {
                    refs.push(RawRef {
                        from_symbol_id: from_id.to_string(),
                        target_name: t.to_string(),
                        qualifier: None,
                        kind: EdgeKind::TypeRef,
                        line: n.start_position().row + 1,
                        receiver_chain: None,
                    });
                }
            }
            "scoped_identifier" => {
                let txt = node_text(n, src);
                if let Some(first) = txt.split("::").next()
                    && first.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                    && !is_rust_builtin_type(first)
                {
                    refs.push(RawRef {
                        from_symbol_id: from_id.to_string(),
                        target_name: first.to_string(),
                        qualifier: None,
                        kind: EdgeKind::TypeRef,
                        line: n.start_position().row + 1,
                        receiver_chain: None,
                    });
                }
            }
            _ => {
                let mut c = n.walk();
                for ch in n.children(&mut c) {
                    stack.push(ch);
                }
            }
        }
    }
}

fn extract_rust_macro_defined_items(tree: Node<'_>, src: &str, out: &mut Vec<String>) {
    let mut children = Vec::new();
    let mut c = tree.walk();
    for ch in tree.children(&mut c) {
        if matches!(ch.kind(), "line_comment" | "block_comment") {
            continue;
        }
        children.push(ch);
    }
    let len = children.len();
    let mut i = 0;
    while i < len {
        let ch = children[i];
        if ch.kind() == "token_repetition" {
            extract_rust_macro_defined_items(ch, src, out);
            i += 1;
            continue;
        }
        match node_text(ch, src) {
            "fn" => {
                let prev_ok = i == 0 || !matches!(children[i - 1].kind(), "." | "::" | "$");
                if prev_ok
                    && i + 2 < len
                    && children[i + 1].kind() == "identifier"
                    && (children[i + 2].kind() == "<"
                        || (children[i + 2].kind() == "token_tree"
                            && node_text(children[i + 2], src).starts_with('(')))
                {
                    let name = node_text(children[i + 1], src).trim();
                    if !name.is_empty() && name != "_" && !out.iter().any(|s| s == name) {
                        out.push(name.to_string());
                    }
                    i += 2;
                    continue;
                }
            }
            "const" | "static" => {
                let prev_ok = i == 0 || !matches!(children[i - 1].kind(), "." | "::" | "$");
                if prev_ok {
                    let mut name_idx = i + 1;
                    if node_text(ch, src) == "static"
                        && name_idx < len
                        && matches!(node_text(children[name_idx], src), "mut" | "ref")
                    {
                        name_idx += 1;
                    }
                    if name_idx + 1 < len
                        && children[name_idx].kind() == "identifier"
                        && children[name_idx + 1].kind() == ":"
                    {
                        let name = node_text(children[name_idx], src).trim();
                        if !name.is_empty() && name != "_" && !out.iter().any(|s| s == name) {
                            out.push(name.to_string());
                        }
                        i = name_idx + 1;
                        continue;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
}

fn walk_rust_reparsed_snippet(
    snippet: &str,
    base_line: usize,
    from_id: &str,
    impl_type: Option<&str>,
    env: &RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) -> bool {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let Some(tree) = parser.parse(snippet, None) else {
        return false;
    };
    let root = tree.root_node();
    if root.has_error() {
        return false;
    }
    let Some(fn_node) = root.named_child(0) else {
        return false;
    };
    let Some(body) = fn_node.child_by_field_name("body") else {
        return false;
    };
    let refs_before = refs.len();
    let bridges_before = bridges.len();
    let mut local_env = env.clone();
    walk_rust_expr(
        body,
        snippet,
        from_id,
        impl_type,
        &mut local_env,
        refs,
        bridges,
    );
    let line_offset = base_line.saturating_sub(1);
    for r in &mut refs[refs_before..] {
        r.line = r.line.saturating_add(line_offset);
    }
    for b in &mut bridges[bridges_before..] {
        b.line = b.line.saturating_add(line_offset);
    }
    true
}

fn walk_rust_select_macro(
    tree: Node<'_>,
    src: &str,
    from_id: &str,
    impl_type: Option<&str>,
    env: &RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let mut raw: Vec<Node<'_>> = Vec::new();
    let mut c = tree.walk();
    for ch in tree.children(&mut c) {
        if matches!(ch.kind(), "line_comment" | "block_comment") {
            continue;
        }
        raw.push(ch);
    }
    let tokens = if raw.len() >= 2
        && matches!(
            (raw.first().map(|n| n.kind()), raw.last().map(|n| n.kind())),
            (Some("{"), Some("}")) | (Some("("), Some(")")) | (Some("["), Some("]"))
        ) {
        &raw[1..raw.len() - 1]
    } else {
        &raw[..]
    };

    let len = tokens.len();
    let mut arm_start = 0usize;
    let mut i = 0usize;
    while i < len {
        if tokens[i].kind() != "=>" {
            i += 1;
            continue;
        }
        let header = &tokens[arm_start..i];
        let body_start = i + 1;
        if body_start >= len {
            scan_rust_token_slice(header, src, from_id, impl_type, env, refs, bridges);
            arm_start = len;
            break;
        }
        let body_end = if tokens[body_start].kind() == "token_tree"
            && node_text(tokens[body_start], src).starts_with('{')
        {
            body_start
        } else {
            let mut end = body_start;
            while end + 1 < len && tokens[end + 1].kind() != "," {
                end += 1;
            }
            end
        };

        let body_src = &src[tokens[body_start].start_byte()..tokens[body_end].end_byte()];
        let body_line = tokens[body_start].start_position().row + 1;

        // Skip leading `biased;` or `,` in header
        let mut hstart = 0usize;
        while hstart < header.len()
            && (header[hstart].kind() == ","
                || (hstart + 1 < header.len()
                    && node_text(header[hstart], src) == "biased"
                    && header[hstart + 1].kind() == ";"))
        {
            if node_text(header[hstart], src) == "biased" {
                hstart += 2;
            } else {
                hstart += 1;
            }
        }
        let hdr = &header[hstart..];
        let eq_pos = hdr.iter().position(|n| n.kind() == "=");
        let mut parsed_full = false;
        if let Some(eq_idx) = eq_pos
            && eq_idx > 0
            && eq_idx + 1 < hdr.len()
        {
            let pat_src = &src[hdr[0].start_byte()..hdr[eq_idx - 1].end_byte()];
            // Stop future expression before `, if <guard>` if present.
            let mut fut_end_idx = hdr.len() - 1;
            for k in (eq_idx + 1)..hdr.len() {
                if hdr[k].kind() == "," && k + 1 < hdr.len() && node_text(hdr[k + 1], src) == "if" {
                    fut_end_idx = k - 1;
                    break;
                }
            }
            if fut_end_idx > eq_idx {
                let fut_src = &src[hdr[eq_idx + 1].start_byte()..hdr[fut_end_idx].end_byte()];
                let wrapped_body = if body_src.trim_start().starts_with('{') {
                    body_src.to_string()
                } else {
                    format!("{{ {body_src} }}")
                };
                let snippet = format!("fn _k() {{ if let {pat_src} = {fut_src} {wrapped_body} }}");
                let hdr_line = hdr[0].start_position().row + 1;
                parsed_full = walk_rust_reparsed_snippet(
                    &snippet, hdr_line, from_id, impl_type, env, refs, bridges,
                );
                if parsed_full && fut_end_idx + 2 < hdr.len() {
                    scan_rust_token_slice(
                        &hdr[fut_end_idx + 2..],
                        src,
                        from_id,
                        impl_type,
                        env,
                        refs,
                        bridges,
                    );
                }
            }
        }
        if !parsed_full {
            scan_rust_token_slice(hdr, src, from_id, impl_type, env, refs, bridges);
            let wrapped = if body_src.trim_start().starts_with('{') {
                format!("fn _k() {body_src}")
            } else {
                format!("fn _k() {{ {body_src} }}")
            };
            if !walk_rust_reparsed_snippet(
                &wrapped, body_line, from_id, impl_type, env, refs, bridges,
            ) {
                scan_rust_token_slice(
                    &tokens[body_start..=body_end],
                    src,
                    from_id,
                    impl_type,
                    env,
                    refs,
                    bridges,
                );
            }
        }

        i = body_end + 1;
        if i < len && tokens[i].kind() == "," {
            i += 1;
        }
        arm_start = i;
    }
    if arm_start < len {
        scan_rust_token_slice(
            &tokens[arm_start..],
            src,
            from_id,
            impl_type,
            env,
            refs,
            bridges,
        );
    }
}

fn scan_rust_token_tree(
    tree: Node<'_>,
    src: &str,
    from_id: &str,
    impl_type: Option<&str>,
    env: &RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let mut children = Vec::new();
    let mut c = tree.walk();
    for ch in tree.children(&mut c) {
        if ch.kind() == "line_comment" || ch.kind() == "block_comment" {
            continue;
        }
        children.push(ch);
    }
    scan_rust_token_slice(&children, src, from_id, impl_type, env, refs, bridges);
}

fn reconstruct_token_slice_chain(
    children: &[Node<'_>],
    dot_idx: usize,
    src: &str,
    impl_type: Option<&str>,
    env: &RustLocalEnv,
) -> (Option<String>, Option<Vec<ReceiverStep>>) {
    let mut rev_steps: Vec<ReceiverStep> = Vec::new();
    let mut prev_ident: Option<&str> = None;
    let mut j = dot_idx;
    let mut root_steps: Option<Vec<ReceiverStep>> = None;

    while j > 0 {
        if children[j - 1].kind() == "?" {
            j -= 1;
            continue;
        }
        if j >= 2 && children[j - 2].kind() == "." && node_text(children[j - 1], src) == "await" {
            j -= 2;
            continue;
        }
        if children[j - 1].kind() == "token_tree"
            && node_text(children[j - 1], src).starts_with('(')
        {
            if j >= 2 && children[j - 2].kind() == "identifier" {
                let callee = node_text(children[j - 2], src);
                if prev_ident.is_none() {
                    prev_ident = Some(callee);
                }
                if j >= 3 && children[j - 3].kind() == "." {
                    rev_steps.push(ReceiverStep::MethodCall {
                        name: callee.to_string(),
                        result_index: 0,
                    });
                    j -= 3;
                    continue;
                }
                if j >= 3 && children[j - 3].kind() == "::" {
                    let mut seg_start = j - 2;
                    while seg_start >= 2
                        && children[seg_start - 1].kind() == "::"
                        && matches!(
                            children[seg_start - 2].kind(),
                            "identifier" | "self" | "super" | "crate"
                        )
                    {
                        seg_start -= 2;
                    }
                    let path: Vec<&str> = (seg_start..=j - 2)
                        .step_by(2)
                        .map(|idx| node_text(children[idx], src))
                        .collect();
                    let full_path = path.join("::");
                    if let Some((qual, name)) = split_rust_scoped_callee(&full_path, impl_type) {
                        root_steps = Some(vec![ReceiverStep::Call {
                            qualifier: qual,
                            name,
                            result_index: 0,
                        }]);
                    }
                    break;
                }
                root_steps = Some(vec![ReceiverStep::Call {
                    qualifier: None,
                    name: callee.to_string(),
                    result_index: 0,
                }]);
                break;
            }
            break;
        }
        if matches!(children[j - 1].kind(), "identifier" | "self") {
            let ident = node_text(children[j - 1], src);
            if prev_ident.is_none() {
                prev_ident = Some(ident);
            }
            if j >= 2 && children[j - 2].kind() == "." {
                rev_steps.push(ReceiverStep::Field(ident.to_string()));
                j -= 2;
                continue;
            }
            root_steps = match ident {
                "self" => impl_type.map(|t| vec![ReceiverStep::Type(t.to_string())]),
                var => env.lookup_chain(var).or_else(|| {
                    if !env.is_shadowed(var) && is_rust_const_ident(var) {
                        Some(vec![ReceiverStep::Call {
                            qualifier: None,
                            name: var.to_string(),
                            result_index: 0,
                        }])
                    } else {
                        None
                    }
                }),
            };
            break;
        }
        break;
    }

    let chain = root_steps.map(|mut base| {
        rev_steps.reverse();
        base.extend(rev_steps);
        base
    });
    (prev_ident.map(|s| s.to_string()), chain)
}

fn scan_rust_token_slice(
    children: &[Node<'_>],
    src: &str,
    from_id: &str,
    impl_type: Option<&str>,
    env: &RustLocalEnv,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let len = children.len();
    let mut i = 0;
    while i < len {
        let ch = children[i];
        let kind = ch.kind();
        let line = ch.start_position().row + 1;

        if kind == "token_tree" || kind == "token_repetition" {
            scan_rust_token_tree(ch, src, from_id, impl_type, env, refs, bridges);
            i += 1;
            continue;
        }

        if kind == "string_literal" || kind == "raw_string_literal" {
            let raw = node_text(ch, src);
            if looks_like_wire_method(raw) {
                bridges.push(BridgeHint {
                    from_symbol_id: from_id.to_string(),
                    bridge_kind: BridgeKind::WireContract,
                    key: format!("rpc:{}", strip_quotes(raw)),
                    role: BridgeRole::Both,
                    line,
                    confidence: Confidence::Heuristic,
                });
            }
            i += 1;
            continue;
        }

        if kind == "." && i + 1 < len && children[i + 1].kind() == "identifier" {
            let method_name = node_text(children[i + 1], src);
            let mut next_idx = i + 2;
            if next_idx + 1 < len
                && children[next_idx].kind() == "::"
                && children[next_idx + 1].kind() == "<"
            {
                let mut depth = 0usize;
                next_idx += 1;
                while next_idx < len {
                    match children[next_idx].kind() {
                        "<" => depth += 1,
                        ">" => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                next_idx += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    next_idx += 1;
                }
            }
            let is_call = next_idx < len
                && children[next_idx].kind() == "token_tree"
                && node_text(children[next_idx], src).starts_with('(');
            if is_call {
                let (prev_ident, chain) =
                    reconstruct_token_slice_chain(children, i, src, impl_type, env);
                refs.push(RawRef {
                    from_symbol_id: from_id.to_string(),
                    target_name: method_name.to_string(),
                    qualifier: Some(prev_ident.unwrap_or_else(|| "_expr".to_string())),
                    kind: EdgeKind::Calls,
                    line,
                    receiver_chain: chain,
                });
                i += 2;
                continue;
            }
        }

        if kind == "$" {
            i += 2;
            continue;
        }

        if matches!(kind, "identifier" | "self" | "crate" | "super") {
            let first_txt = node_text(ch, src);
            let mut end_idx = i;
            let mut segments: Vec<&str> = vec![first_txt];
            while end_idx + 2 < len
                && children[end_idx + 1].kind() == "::"
                && matches!(
                    children[end_idx + 2].kind(),
                    "identifier" | "self" | "super" | "crate"
                )
            {
                segments.push(node_text(children[end_idx + 2], src));
                end_idx += 2;
            }

            let followed_by_bang = end_idx + 1 < len && children[end_idx + 1].kind() == "!";
            if followed_by_bang {
                if let Some(&last) = segments.last()
                    && !is_rust_keyword_or_literal(last)
                {
                    refs.push(RawRef {
                        from_symbol_id: from_id.to_string(),
                        target_name: format!("{}!", last),
                        qualifier: None,
                        kind: EdgeKind::Calls,
                        line,
                        receiver_chain: None,
                    });
                }
                i = end_idx + 2;
                continue;
            }

            if segments.len() >= 2 {
                let full_path = segments.join("::");
                emit_scoped_call_ref(from_id, &full_path, impl_type, line, refs);
                i = end_idx + 1;
                continue;
            }

            if !is_rust_keyword_or_literal(first_txt) {
                let next_kind = if i + 1 < len {
                    Some(children[i + 1].kind())
                } else {
                    None
                };
                let is_call = next_kind == Some("token_tree")
                    && node_text(children[i + 1], src).starts_with('(');
                let is_key_or_decl = matches!(next_kind, Some("=") | Some(":"));
                let prev_is_dot = i > 0 && children[i - 1].kind() == ".";

                if is_call && !prev_is_dot && !env.is_shadowed(first_txt) {
                    refs.push(RawRef {
                        from_symbol_id: from_id.to_string(),
                        target_name: first_txt.to_string(),
                        qualifier: None,
                        kind: EdgeKind::Calls,
                        line,
                        receiver_chain: None,
                    });
                } else if !is_key_or_decl && !prev_is_dot && !env.is_shadowed(first_txt) {
                    if first_txt
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_uppercase())
                    {
                        if !is_rust_builtin_type(first_txt) {
                            refs.push(RawRef {
                                from_symbol_id: from_id.to_string(),
                                target_name: first_txt.to_string(),
                                qualifier: None,
                                kind: EdgeKind::TypeRef,
                                line,
                                receiver_chain: None,
                            });
                        }
                    } else if next_kind != Some(".") {
                        refs.push(RawRef {
                            from_symbol_id: from_id.to_string(),
                            target_name: first_txt.to_string(),
                            qualifier: None,
                            kind: EdgeKind::Calls,
                            line,
                            receiver_chain: None,
                        });
                    }
                }
            }
        }

        i += 1;
    }
}

fn first_rust_str_arg(call: Node<'_>, src: &str) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    let mut c = args.walk();
    for ch in args.children(&mut c) {
        if ch.kind() == "string_literal" {
            return Some(strip_quotes(node_text(ch, src)));
        }
    }
    None
}

pub fn lower_rust_node(node: Node<'_>, src: &str) -> IrNode {
    match node.kind() {
        "block" => {
            let mut stmts = Vec::new();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.is_named() && ch.kind() != "line_comment" && ch.kind() != "block_comment" {
                    stmts.push(lower_rust_node(ch, src));
                }
            }
            IrNode::Seq(stmts)
        }
        "expression_statement" => {
            if let Some(ch) = node.named_child(0) {
                lower_rust_node(ch, src)
            } else {
                IrNode::Seq(vec![])
            }
        }
        "let_declaration" => {
            let dst = node
                .child_by_field_name("pattern")
                .map(|p| {
                    node_text(p, src)
                        .trim()
                        .trim_start_matches("mut ")
                        .to_string()
                })
                .unwrap_or_else(|| "_".into());
            let val = node
                .child_by_field_name("value")
                .map(|v| lower_rust_node(v, src))
                .unwrap_or(IrNode::Lit(LitClass::Null));
            IrNode::Assign {
                dst,
                val: Box::new(val),
            }
        }
        "assignment_expression" => {
            let dst = node
                .child_by_field_name("left")
                .map(|p| node_text(p, src).trim().to_string())
                .unwrap_or_else(|| "_".into());
            let val = node
                .child_by_field_name("right")
                .map(|v| lower_rust_node(v, src))
                .unwrap_or_else(|| IrNode::Opaque("rhs".into()));
            IrNode::Assign {
                dst,
                val: Box::new(val),
            }
        }
        "for_expression" => {
            let var = node
                .child_by_field_name("pattern")
                .map(|p| node_text(p, src).trim().to_string());
            let body = node
                .child_by_field_name("body")
                .map(|b| lower_rust_node(b, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            let kind = if let Some(val) = node.child_by_field_name("value") {
                if val.kind() == "range_expression" {
                    let start = val
                        .named_child(0)
                        .map(|n| lower_rust_node(n, src))
                        .unwrap_or(IrNode::Lit(LitClass::Zero));
                    let raw_end = val
                        .named_child(1)
                        .map(|n| lower_rust_node(n, src))
                        .unwrap_or_else(|| IrNode::Opaque("end".into()));
                    let end = if node_text(val, src).contains("..=") {
                        IrNode::BinOp {
                            op: OpClass::Add,
                            lhs: Box::new(raw_end),
                            rhs: Box::new(IrNode::Lit(LitClass::One)),
                        }
                    } else {
                        raw_end
                    };
                    LoopKind::IterRange {
                        start: Box::new(start),
                        end: Box::new(end),
                        step: Box::new(IrNode::Lit(LitClass::One)),
                    }
                } else {
                    LoopKind::IterSeq {
                        seq: Box::new(lower_rust_node(val, src)),
                    }
                }
            } else {
                LoopKind::Infinite
            };
            IrNode::Loop {
                kind,
                var,
                body: Box::new(body),
            }
        }
        "while_expression" => {
            let cond = node
                .child_by_field_name("condition")
                .map(|c| lower_rust_node(c, src))
                .unwrap_or(IrNode::Lit(LitClass::Bool(true)));
            let body = node
                .child_by_field_name("body")
                .map(|b| lower_rust_node(b, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            IrNode::Loop {
                kind: LoopKind::While {
                    cond: Box::new(cond),
                },
                var: None,
                body: Box::new(body),
            }
        }
        "loop_expression" => {
            let body = node
                .child_by_field_name("body")
                .map(|b| lower_rust_node(b, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            IrNode::Loop {
                kind: LoopKind::Infinite,
                var: None,
                body: Box::new(body),
            }
        }
        "if_expression" => {
            let cond = node
                .child_by_field_name("condition")
                .map(|c| lower_rust_node(c, src))
                .unwrap_or_else(|| IrNode::Opaque("cond".into()));
            let then_br = node
                .child_by_field_name("consequence")
                .map(|b| lower_rust_node(b, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            let else_br = node.child_by_field_name("alternative").map(|alt| {
                // else_clause wraps block or if_expression
                let inner = alt.named_child(0).unwrap_or(alt);
                Box::new(lower_rust_node(inner, src))
            });
            IrNode::Branch {
                cond: Box::new(cond),
                then_br: Box::new(then_br),
                else_br,
            }
        }
        "match_expression" => {
            let scrutinee = node
                .child_by_field_name("value")
                .map(|v| Box::new(lower_rust_node(v, src)));
            let mut arms = Vec::new();
            let mut default = None;
            if let Some(body) = node.child_by_field_name("body") {
                let mut c = body.walk();
                for ch in body.children(&mut c) {
                    if ch.kind() == "match_arm" {
                        let pat = ch
                            .child_by_field_name("pattern")
                            .map(|p| node_text(p, src).trim().to_string())
                            .unwrap_or_default();
                        let val = ch
                            .child_by_field_name("value")
                            .map(|v| lower_rust_node(v, src))
                            .unwrap_or_else(|| IrNode::Seq(vec![]));
                        if pat == "_" {
                            default = Some(Box::new(val));
                        } else if let Some(pnode) = ch.child_by_field_name("pattern") {
                            let inner_pat = pnode.named_child(0).unwrap_or(pnode);
                            arms.push((lower_rust_node(inner_pat, src), val));
                        }
                    }
                }
            }
            IrNode::MultiBranch {
                scrutinee,
                arms,
                default,
            }
        }
        "try_expression" => {
            let inner = node
                .named_child(0)
                .map(|n| lower_rust_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("try".into()));
            IrNode::PropagateErr(Box::new(inner))
        }
        "return_expression" => {
            let val = node
                .named_child(0)
                .map(|n| Box::new(lower_rust_node(n, src)));
            IrNode::Return(val)
        }
        "break_expression" => IrNode::Break,
        "continue_expression" => IrNode::Continue,
        "call_expression" => {
            let callee = node
                .child_by_field_name("function")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            let mut args = Vec::new();
            if let Some(al) = node.child_by_field_name("arguments") {
                let mut c = al.walk();
                for ch in al.children(&mut c) {
                    if ch.is_named() {
                        args.push(lower_rust_node(ch, src));
                    }
                }
            }
            IrNode::Call { callee, args }
        }
        "binary_expression" => {
            let op = node
                .child_by_field_name("operator")
                .map(|n| map_binop(node_text(n, src)))
                .unwrap_or(OpClass::Other);
            let lhs = node
                .child_by_field_name("left")
                .map(|n| lower_rust_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("lhs".into()));
            let rhs = node
                .child_by_field_name("right")
                .map(|n| lower_rust_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("rhs".into()));
            IrNode::BinOp {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            }
        }
        "unary_expression" => {
            let txt = node_text(node, src);
            let op = if txt.starts_with('!') {
                map_unop("!")
            } else if txt.starts_with('-') {
                map_unop("-")
            } else if txt.starts_with('*') {
                map_unop("*")
            } else {
                map_unop("&")
            };
            let expr = node
                .named_child(0)
                .map(|n| lower_rust_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("unop".into()));
            IrNode::UnOp {
                op,
                expr: Box::new(expr),
            }
        }
        "field_expression" => {
            let obj = node
                .child_by_field_name("value")
                .map(|n| lower_rust_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("obj".into()));
            let field = node
                .child_by_field_name("field")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            IrNode::Member {
                obj: Box::new(obj),
                field,
            }
        }
        "index_expression" => {
            let obj = node
                .named_child(0)
                .map(|n| lower_rust_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("arr".into()));
            let idx = node
                .named_child(1)
                .map(|n| lower_rust_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("idx".into()));
            IrNode::Index {
                obj: Box::new(obj),
                idx: Box::new(idx),
            }
        }
        "parenthesized_expression" => node
            .named_child(0)
            .map(|n| lower_rust_node(n, src))
            .unwrap_or_else(|| IrNode::Opaque("paren".into())),
        "identifier" | "self" => IrNode::Var(node_text(node, src).to_string()),
        "integer_literal" | "float_literal" | "string_literal" | "raw_string_literal"
        | "char_literal" | "boolean_literal" => literal_node(node_text(node, src)),
        _ => IrNode::Opaque(node.kind().to_string()),
    }
}
