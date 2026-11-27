use crate::ir::{IrFunc, IrNode, LitClass, LoopKind, OpClass};
use crate::lang::{
    literal_node, looks_like_wire_method, make_symbol_id, map_binop, map_unop, node_span,
    node_text, strip_quotes,
};
use crate::model::{
    BridgeHint, BridgeKind, BridgeRole, Confidence, EdgeKind, FileExtract, Language, RawRef,
    ReceiverStep, RustUseItem, Symbol, SymbolKind, TraitImplInfo, Visibility,
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
    trait_impls: Vec<TraitImplInfo>,
    impl_macros: Vec<(String, String, usize)>,
    cfg_gated_symbols: HashMap<String, String>,
    type_aliases: HashMap<String, String>,
    rust_uses: Vec<RustUseItem>,
}

/// Lexical scope stack for Rust local variables and inferred receiver chains.
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
        || rel_path.ends_with("_test.rs")
        || rel_path.ends_with("_tests.rs")
        || rel_path.ends_with("tests.rs");
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
        trait_impls: Vec::new(),
        impl_macros: Vec::new(),
        cfg_gated_symbols: HashMap::new(),
        type_aliases: HashMap::new(),
        rust_uses: Vec::new(),
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
        imports: ctx.imports,
        symbols: ctx.symbols,
        refs: ctx.refs,
        bridges: ctx.bridges,
        ir_funcs: ctx.ir_funcs,
        interface_methods: ctx.interface_methods,
        embedded_types: ctx.embedded_types,
        struct_fields: ctx.struct_fields,
        func_return_types: ctx.func_return_types,
        trait_impls: ctx.trait_impls,
        impl_macros: ctx.impl_macros,
        cfg_gated_symbols: ctx.cfg_gated_symbols,
        type_aliases: ctx.type_aliases,
        rust_uses: ctx.rust_uses,
        package_or_module: pkg,
    })
}

fn attr_is_test(attr: &str) -> bool {
    if attr.contains("#[test]")
        || attr.contains("::test]")
        || attr.contains("::test(")
        || attr.contains("#[rstest")
        || attr.contains("#[test_case")
        || attr.contains("#[traced_test")
    {
        return true;
    }
    if let Some(idx) = attr.find("cfg(") {
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
}

fn extract_serde_fn_refs(attrs: &[String], from_id: &str, line: usize, refs: &mut Vec<RawRef>) {
    const KEYS: &[&str] = &[
        "default",
        "serialize_with",
        "deserialize_with",
        "skip_serializing_if",
        "with",
    ];
    for attr in attrs {
        if !attr.contains("serde(") {
            continue;
        }
        for key in KEYS {
            let mut search = attr.as_str();
            while let Some(pos) = search.find(key) {
                let before_ok = pos == 0
                    || (!search.as_bytes()[pos - 1].is_ascii_alphanumeric()
                        && search.as_bytes()[pos - 1] != b'_');
                let after = &search[pos + key.len()..];
                let after_trim = after.trim_start();
                if before_ok && let Some(after_eq) = after_trim.strip_prefix('=') {
                    let after_eq = after_eq.trim_start();
                    if let Some(after_q) = after_eq.strip_prefix('"')
                        && let Some(end_q) = after_q.find('"')
                    {
                        let path = &after_q[..end_q];
                        if *key == "with" {
                            let mod_name = path.rsplit("::").next().unwrap_or(path).trim();
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
                        } else {
                            emit_scoped_call_ref(from_id, path, None, line, refs);
                        }
                    }
                }
                search = &search[pos + key.len()..];
            }
        }
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
    } else if prefix_parts
        .last()
        .is_some_and(|p| p.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
    {
        prefix_parts.last().map(|p| (*p).to_string())
    } else {
        Some(prefix_parts.join("::"))
    };
    if let Some(ref q) = qual
        && q.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && !is_rust_builtin_type(q)
    {
        refs.push(RawRef {
            from_symbol_id: from_id.to_string(),
            target_name: q.clone(),
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
    let mut pending_attrs: Vec<String> = Vec::new();

    for child in parent.children(&mut cursor) {
        if child.kind() == "attribute_item" {
            pending_attrs.push(node_text(child, ctx.src).to_string());
            continue;
        }
        let attrs = std::mem::take(&mut pending_attrs);
        let has_test_attr = attrs.iter().any(|a| attr_is_test(a));
        let has_cfg_attr = attrs.iter().any(|a| attr_is_non_test_cfg(a));
        let has_export_attr = attrs.iter().any(|a| attr_is_export(a));
        let has_serde_attr = attrs
            .iter()
            .any(|a| a.contains("serde") || a.contains("Serialize") || a.contains("Deserialize"));

        match child.kind() {
            "use_declaration" => {
                ctx.imports.push(node_text(child, ctx.src).to_string());
                let is_pub = rust_visibility(child, ctx.src) == Visibility::Public;
                let is_test = in_test_mod || has_test_attr;
                if let Some(arg) = child.child_by_field_name("argument") {
                    extract_rust_use_items(arg, ctx.src, "", is_pub, is_test, &mut ctx.rust_uses);
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
                    let name = node_text(name_node, ctx.src).to_string();
                    let span = node_span(child, 0);
                    let id = make_symbol_id(ctx.rel_path, &name, span.start_line);
                    let is_test = in_test_mod || has_test_attr;
                    ctx.symbols.push(Symbol {
                        id: id.clone(),
                        name: name.clone(),
                        qualified_name: name,
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
                        }
                    }
                }
            }
            "macro_invocation" => {
                if let Some(ty) = impl_type
                    && !in_test_mod
                    && !has_test_attr
                    && let Some(mnode) = child.child_by_field_name("macro")
                {
                    let mtxt = node_text(mnode, ctx.src);
                    let mname = mtxt.rsplit("::").next().unwrap_or(mtxt).trim();
                    if !mname.is_empty() {
                        ctx.impl_macros.push((
                            ty.to_string(),
                            mname.to_string(),
                            child.start_position().row + 1,
                        ));
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
                        ctx.symbols.push(Symbol {
                            id: id.clone(),
                            name,
                            qualified_name: qname,
                            kind: SymbolKind::Const,
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
                        if let Some(ty_node) = child.child_by_field_name("type") {
                            collect_type_refs(
                                ty_node,
                                ctx.src,
                                &id,
                                impl_type,
                                &mut ctx.refs,
                                &mut ctx.bridges,
                            );
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
                        let norm =
                            normalize_rust_type_text(node_text(ret_node, ctx.src), impl_type);
                        if !norm.is_empty() {
                            ctx.func_return_types.insert(id.clone(), vec![norm]);
                        }
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
                    let span = node_span(child, 0);
                    let id = make_symbol_id(ctx.rel_path, &name, span.start_line);
                    let kind = match child.kind() {
                        "struct_item" => SymbolKind::Struct,
                        "enum_item" => SymbolKind::Enum,
                        "trait_item" => SymbolKind::Trait,
                        _ => SymbolKind::Type,
                    };
                    let is_test = in_test_mod || has_test_attr;
                    ctx.symbols.push(Symbol {
                        id: id.clone(),
                        name: name.clone(),
                        qualified_name: name.clone(),
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
                    extract_serde_fn_refs(&attrs, &id, span.start_line, &mut ctx.refs);
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
                        let fields = extract_rust_struct_fields(child, ctx.src, &id, &mut ctx.refs);
                        if !fields.is_empty() {
                            ctx.struct_fields.insert(id.clone(), fields);
                        }
                    } else if kind == SymbolKind::Enum {
                        extract_rust_enum_attrs(child, ctx.src, &id, &mut ctx.refs);
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
                        && let Some(rhs) = child.child_by_field_name("type")
                    {
                        let norm = normalize_rust_type_text(node_text(rhs, ctx.src), impl_type);
                        if !norm.is_empty() && norm != name {
                            ctx.type_aliases.insert(id.clone(), norm);
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

fn extract_rust_struct_fields(
    struct_node: Node<'_>,
    src: &str,
    struct_id: &str,
    refs: &mut Vec<RawRef>,
) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let Some(body) = struct_node.child_by_field_name("body") else {
        return fields;
    };
    if body.kind() != "field_declaration_list" {
        return fields;
    }
    let mut pending_attrs: Vec<String> = Vec::new();
    let mut c = body.walk();
    for ch in body.children(&mut c) {
        if ch.kind() == "attribute_item" {
            pending_attrs.push(node_text(ch, src).to_string());
            continue;
        }
        if ch.kind() == "field_declaration" {
            let attrs = std::mem::take(&mut pending_attrs);
            extract_serde_fn_refs(&attrs, struct_id, ch.start_position().row + 1, refs);
            if let (Some(fname), Some(fty)) = (
                ch.child_by_field_name("name"),
                ch.child_by_field_name("type"),
            ) {
                let norm = normalize_rust_type_text(node_text(fty, src), None);
                if !norm.is_empty() {
                    fields.insert(node_text(fname, src).to_string(), norm);
                }
            }
        }
    }
    fields
}

fn extract_rust_enum_attrs(enum_node: Node<'_>, src: &str, enum_id: &str, refs: &mut Vec<RawRef>) {
    let mut stack = vec![enum_node];
    while let Some(n) = stack.pop() {
        if n.kind() == "attribute_item" {
            let txt = node_text(n, src).to_string();
            extract_serde_fn_refs(&[txt], enum_id, n.start_position().row + 1, refs);
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
                    bind_rust_pattern(pat, src, param_ty, env);
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

fn bind_rust_pattern(
    pat: Node<'_>,
    src: &str,
    inferred_chain: Option<Vec<ReceiverStep>>,
    env: &mut RustLocalEnv,
) {
    match pat.kind() {
        "match_pattern" => {
            let cond_id = pat.child_by_field_name("condition").map(|n| n.id());
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_named() && Some(ch.id()) != cond_id {
                    bind_rust_pattern(ch, src, inferred_chain.clone(), env);
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
                    bind_rust_pattern(ch, src, inferred_chain.clone(), env);
                }
            }
        }
        "tuple_struct_pattern" => {
            let type_name = pat
                .child_by_field_name("type")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            let is_wrapper = matches!(type_name, "Some" | "Ok" | "Box" | "Arc" | "Rc")
                || type_name.ends_with("::Some")
                || type_name.ends_with("::Ok");
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if Some(ch.id()) == pat.child_by_field_name("type").map(|n| n.id()) {
                    continue;
                }
                if ch.is_named() {
                    bind_rust_pattern(
                        ch,
                        src,
                        if is_wrapper {
                            inferred_chain.clone()
                        } else {
                            None
                        },
                        env,
                    );
                }
            }
        }
        "tuple_pattern" => {
            let mut named = Vec::new();
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_named() {
                    named.push(ch);
                }
            }
            if named.len() == 2
                && inferred_chain
                    .as_ref()
                    .and_then(|v| v.last())
                    .map(|s| matches!(s, ReceiverStep::Elem))
                    .unwrap_or(false)
            {
                bind_rust_pattern(named[0], src, None, env);
                bind_rust_pattern(named[1], src, inferred_chain, env);
            } else {
                for ch in named {
                    bind_rust_pattern(ch, src, None, env);
                }
            }
        }
        "struct_pattern" => {
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.kind() == "field_pattern" {
                    if let Some(inner_pat) = ch.child_by_field_name("pattern") {
                        bind_rust_pattern(inner_pat, src, None, env);
                    } else if let Some(fname) = ch.child_by_field_name("name") {
                        let name = node_text(fname, src);
                        env.bind(name.to_string(), None);
                    }
                }
            }
        }
        "slice_pattern" | "or_pattern" | "captured_pattern" => {
            let mut c = pat.walk();
            for ch in pat.children(&mut c) {
                if ch.is_named() {
                    bind_rust_pattern(ch, src, None, env);
                }
            }
        }
        _ => {}
    }
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
            | "Value"
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

fn strip_rust_type_prefixes(raw: &str) -> &str {
    let mut s = raw.trim();
    while s.starts_with('(') && s.ends_with(')') && s.len() >= 2 {
        s = s[1..s.len() - 1].trim();
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

    if matches!(
        short_base,
        "Arc"
            | "Rc"
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
    ) || (short_base.ends_with("Result") && !type_args.is_empty())
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

fn is_transparent_wrapper_ctor(ftxt: &str) -> bool {
    matches!(ftxt, "Some" | "Ok")
        || ftxt.ends_with("Arc::new")
        || ftxt.ends_with("Arc::clone")
        || ftxt.ends_with("Rc::new")
        || ftxt.ends_with("Rc::clone")
        || ftxt.ends_with("Box::new")
        || ftxt.ends_with("Box::pin")
        || ftxt.ends_with("Pin::new")
        || ftxt.ends_with("Mutex::new")
        || ftxt.ends_with("RwLock::new")
        || ftxt.ends_with("RefCell::new")
        || ftxt.ends_with("Cell::new")
        || ftxt.ends_with("ArcSwap::new")
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
        "self" => impl_type.map(|t| vec![ReceiverStep::Type(t.to_string())]),
        "identifier" => {
            let name = node_text(expr, src);
            if name == "self" {
                impl_type.map(|t| vec![ReceiverStep::Type(t.to_string())])
            } else {
                env.lookup_chain(name)
            }
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
            match func.kind() {
                "field_expression" => {
                    let recv = func.child_by_field_name("value")?;
                    let mname = node_text(func.child_by_field_name("field")?, src);
                    let mut chain = extract_rust_receiver_chain(recv, src, env, impl_type)?;
                    chain.push(ReceiverStep::MethodCall {
                        name: mname.to_string(),
                        result_index: 0,
                    });
                    Some(chain)
                }
                "scoped_identifier" => {
                    let clean = strip_balanced_generics(ftxt);
                    let parts: Vec<&str> = clean.split("::").filter(|p| !p.is_empty()).collect();
                    let last = *parts.last()?;
                    let raw_qual = if parts.len() >= 2 {
                        Some(parts[parts.len() - 2])
                    } else {
                        None
                    };
                    let qual = match raw_qual {
                        Some("Self") => impl_type.map(|t| t.to_string()),
                        Some("crate") | Some("super") | Some("self") | None => None,
                        Some(q) => Some(q.to_string()),
                    };
                    Some(vec![ReceiverStep::Call {
                        qualifier: qual,
                        name: last.to_string(),
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
                bind_rust_pattern(pat, src, inferred, env);
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
                bind_rust_pattern(pat, src, elem_chain, env);
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_rust_expr(body, src, from_id, impl_type, env, refs, bridges);
            }
            env.pop_scope();
        }
        "closure_expression" => {
            walk_rust_closure(node, src, from_id, impl_type, env, refs, bridges, None);
        }
        "match_arm" => {
            env.push_scope();
            if let Some(pat) = node.child_by_field_name("pattern") {
                if pat.kind() == "match_pattern" {
                    let cond = pat.child_by_field_name("condition");
                    let cond_id = cond.map(|n| n.id());
                    let mut pc = pat.walk();
                    for pch in pat.children(&mut pc) {
                        if pch.is_named() && Some(pch.id()) != cond_id {
                            collect_pattern_type_refs(pch, src, from_id, refs);
                            bind_rust_pattern(pch, src, None, env);
                        }
                    }
                    if let Some(guard) = cond {
                        walk_rust_expr(guard, src, from_id, impl_type, env, refs, bridges);
                    }
                } else {
                    collect_pattern_type_refs(pat, src, from_id, refs);
                    bind_rust_pattern(pat, src, None, env);
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
        "call_expression" => {
            let mut closure_param_hint: Option<Vec<ReceiverStep>> = None;
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
                    }
                    _ => {
                        walk_rust_expr(func, src, from_id, impl_type, env, refs, bridges);
                    }
                }
            }
            if let Some(args) = node.child_by_field_name("arguments") {
                if closure_param_hint.is_some() {
                    let mut ac = args.walk();
                    for arg in args.children(&mut ac) {
                        if arg.kind() == "closure_expression" {
                            walk_rust_closure(
                                arg,
                                src,
                                from_id,
                                impl_type,
                                env,
                                refs,
                                bridges,
                                closure_param_hint.clone(),
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
            if let Some(mnode) = node.child_by_field_name("macro") {
                let mtxt = node_text(mnode, src);
                let mname = mtxt.rsplit("::").next().unwrap_or(mtxt).trim();
                if !mname.is_empty() {
                    refs.push(RawRef {
                        from_symbol_id: from_id.to_string(),
                        target_name: mname.to_string(),
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
                    scan_rust_token_tree(ch, src, from_id, impl_type, env, refs, bridges);
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
        "attribute_item" | "use_declaration" | "line_comment" | "block_comment" | "lifetime"
        | "label" => {}
        _ => {
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                walk_rust_expr(ch, src, from_id, impl_type, env, refs, bridges);
            }
        }
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
            let inferred = cond.child_by_field_name("value").and_then(|v| {
                walk_rust_expr(v, src, from_id, impl_type, env, refs, bridges);
                extract_rust_receiver_chain(v, src, env, impl_type)
            });
            if let Some(pat) = cond.child_by_field_name("pattern") {
                collect_pattern_type_refs(pat, src, from_id, refs);
                bind_rust_pattern(pat, src, inferred, env);
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

fn extract_rust_use_items(
    node: Node<'_>,
    src: &str,
    prefix: &str,
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
                        is_pub,
                        is_test,
                    });
                }
            } else if !name.is_empty() {
                let full = join_rust_use_path(prefix, name);
                out.push(RustUseItem {
                    local_name: name.to_string(),
                    target_path: full,
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
                extract_rust_use_items(list, src, &next_prefix, is_pub, is_test, out);
            }
        }
        "use_list" => {
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.is_named() {
                    extract_rust_use_items(ch, src, prefix, is_pub, is_test, out);
                }
            }
        }
        _ => {}
    }
}

fn infer_closure_param_hint(
    recv_chain: &[ReceiverStep],
    method: &str,
) -> Option<Vec<ReceiverStep>> {
    if matches!(
        method,
        "any"
            | "all"
            | "find"
            | "find_map"
            | "for_each"
            | "filter"
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
        return Some(ch);
    }
    if matches!(method, "map" | "inspect") {
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
        return Some(ch);
    }
    if matches!(
        method,
        "and_then" | "is_some_and" | "is_ok_and" | "map_or" | "map_or_else"
    ) {
        return Some(recv_chain.to_vec());
    }
    None
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
    first_param_hint: Option<Vec<ReceiverStep>>,
) {
    env.push_scope();
    if let Some(plist) = node.child_by_field_name("parameters") {
        let mut hint = first_param_hint;
        let mut c = plist.walk();
        for ch in plist.children(&mut c) {
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
                    .or_else(|| hint.take());
                if let Some(pat) = ch.child_by_field_name("pattern") {
                    bind_rust_pattern(pat, src, pty, env);
                }
            } else if ch.is_named() {
                let pty = hint.take();
                bind_rust_pattern(ch, src, pty, env);
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
                // Reconstruct base.field1.field2 chain backwards from '.'
                let mut rev_idents: Vec<&str> = Vec::new();
                let mut j = i;
                while j > 0 && matches!(children[j - 1].kind(), "identifier" | "self") {
                    rev_idents.push(node_text(children[j - 1], src));
                    j -= 1;
                    if j > 0 && children[j - 1].kind() == "." {
                        j -= 1;
                    } else {
                        break;
                    }
                }
                rev_idents.reverse();
                let prev_ident = rev_idents.last().copied();
                let chain = rev_idents.first().and_then(|&root| {
                    let mut base = match root {
                        "self" => impl_type.map(|t| vec![ReceiverStep::Type(t.to_string())])?,
                        var => env.lookup_chain(var)?,
                    };
                    for field in &rev_idents[1..] {
                        base.push(ReceiverStep::Field((*field).to_string()));
                    }
                    Some(base)
                });
                refs.push(RawRef {
                    from_symbol_id: from_id.to_string(),
                    target_name: method_name.to_string(),
                    qualifier: prev_ident.map(|s| s.to_string()),
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

        if kind == "identifier" || kind == "self" {
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
                        target_name: last.to_string(),
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
