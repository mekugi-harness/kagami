use crate::ir::{IrFunc, IrNode, LitClass, LoopKind, OpClass};
use crate::lang::{
    literal_node, looks_like_wire_method, make_symbol_id, map_binop, map_unop, node_span,
    node_text, strip_quotes,
};
use crate::model::{
    BridgeHint, BridgeKind, BridgeRole, Confidence, EdgeKind, FileExtract, Language, RawRef,
    Symbol, SymbolKind, Visibility,
};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone)]
struct ImportedBinding {
    target_name: String,
    resolved_file: Option<String>,
}

pub fn extract(
    rel_path: &str,
    src: &str,
    blake3_hex: String,
    lang: Language,
    line_offset: usize,
) -> Result<FileExtract> {
    let mut parser = Parser::new();
    match lang {
        Language::TypeScript | Language::Svelte => {
            if rel_path.ends_with(".tsx") {
                parser.set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())?;
            } else {
                parser.set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())?;
            }
        }
        _ => {
            parser.set_language(&tree_sitter_javascript::LANGUAGE.into())?;
        }
    }
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("failed to parse TS/JS file {}", rel_path))?;

    let is_test_file = rel_path.contains(".test.")
        || rel_path.contains(".spec.")
        || rel_path.contains("/__tests__/");
    let is_sveltekit_route = rel_path.contains("/routes/")
        && (rel_path.ends_with("+page.ts")
            || rel_path.ends_with("+layout.ts")
            || rel_path.ends_with("+server.ts")
            || rel_path.ends_with("+page.server.ts")
            || rel_path.ends_with("+layout.server.ts"));

    let mut imports = Vec::new();
    let mut symbols = Vec::new();
    let mut refs = Vec::new();
    let mut bridges = Vec::new();
    let mut ir_funcs = Vec::new();
    let imported_bindings = collect_ts_imported_bindings(tree.root_node(), src, rel_path);
    let no_shadowed: HashSet<String> = HashSet::new();

    // Collect module-scope (top-level) calls/expressions under a <module> entrypoint symbol
    let mod_id = make_symbol_id(rel_path, "<module>", 1 + line_offset);
    let mut has_top_level_exec = false;
    {
        let root = tree.root_node();
        let mut rc = root.walk();
        for ch in root.children(&mut rc) {
            match ch.kind() {
                "import_statement" => {
                    // Static import of a relative .svelte file executes its <script module> block
                    // (skip erased whole-declaration type-only imports: import type ...)
                    let is_type_only = {
                        let mut ic = ch.walk();
                        ch.children(&mut ic).any(|k| k.kind() == "type")
                    };
                    if !is_type_only {
                        if let Some(s) = ch.child_by_field_name("source") {
                            let path = strip_quotes(node_text(s, src));
                            if path.ends_with(".svelte") {
                                if let Some(resolved_file) = resolve_relative_path(rel_path, &path) {
                                    has_top_level_exec = true;
                                    refs.push(RawRef {
                                        from_symbol_id: mod_id.clone(),
                                        target_name: "<script:module>".to_string(),
                                        qualifier: Some(resolved_file),
                                        kind: EdgeKind::Calls,
                                        line: ch.start_position().row + 1 + line_offset,
                                        receiver_chain: None,
                                    });
                                }
                            }
                        }
                    }
                }
                "expression_statement"
                | "if_statement"
                | "for_statement"
                | "for_in_statement"
                | "while_statement"
                | "try_statement" => {
                    has_top_level_exec = true;
                    collect_ts_refs_and_bridges(
                        ch,
                        src,
                        line_offset,
                        &mod_id,
                        &imported_bindings,
                        &no_shadowed,
                        &mut refs,
                        &mut bridges,
                    );
                }
                "lexical_declaration" | "variable_declaration" => {
                    if collect_ts_non_func_var_inits(
                        ch,
                        src,
                        line_offset,
                        &mod_id,
                        &imported_bindings,
                        &mut refs,
                        &mut bridges,
                    ) {
                        has_top_level_exec = true;
                    }
                }
                "export_statement" => {
                    let mut ec = ch.walk();
                    for ech in ch.children(&mut ec) {
                        if matches!(ech.kind(), "lexical_declaration" | "variable_declaration")
                            && collect_ts_non_func_var_inits(
                                ech,
                                src,
                                line_offset,
                                &mod_id,
                                &imported_bindings,
                                &mut refs,
                                &mut bridges,
                            )
                        {
                            has_top_level_exec = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if has_top_level_exec {
        symbols.push(Symbol {
            id: mod_id,
            name: "<module>".to_string(),
            qualified_name: "<module>".to_string(),
            kind: SymbolKind::Script,
            lang,
            file: rel_path.to_string(),
            span: crate::model::Span {
                start_line: 1 + line_offset,
                end_line: src.lines().count().max(1) + line_offset,
            },
            visibility: Visibility::Public,
            is_test: is_test_file,
            is_entrypoint: lang != Language::Svelte,
            is_dynamic_candidate: false,
            package_or_module: rel_path.to_string(),
        });
    }

    walk_ts_nodes(
        tree.root_node(),
        src,
        rel_path,
        lang,
        line_offset,
        false,
        is_test_file,
        is_sveltekit_route,
        &imported_bindings,
        &mut imports,
        &mut symbols,
        &mut refs,
        &mut bridges,
        &mut ir_funcs,
    );

    Ok(FileExtract {
        file: rel_path.to_string(),
        lang,
        blake3: blake3_hex,
        package_or_module: rel_path.to_string(),
        imports,
        symbols,
        refs,
        bridges,
        ir_funcs,
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
        func_callback_params: std::collections::HashMap::new(),
    })
}

pub(crate) fn resolve_relative_path(current_file: &str, import_path: &str) -> Option<String> {
    if !import_path.starts_with("./") && !import_path.starts_with("../") {
        return None;
    }
    let mut parts: Vec<&str> = match current_file.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir.split('/').collect(),
        _ => Vec::new(),
    };
    for seg in import_path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn collect_ts_imported_bindings(
    root: Node<'_>,
    src: &str,
    rel_path: &str,
) -> HashMap<String, ImportedBinding> {
    let mut bindings = HashMap::new();
    let mut rc = root.walk();
    for ch in root.children(&mut rc) {
        if ch.kind() != "import_statement" {
            continue;
        }
        let is_type_only = {
            let mut ic = ch.walk();
            ch.children(&mut ic).any(|k| k.kind() == "type")
        };
        if is_type_only {
            continue;
        }
        let Some(source_node) = ch.child_by_field_name("source") else {
            continue;
        };
        let path = strip_quotes(node_text(source_node, src));
        let resolved_file = resolve_relative_path(rel_path, &path);

        let mut ic = ch.walk();
        for clause in ch.children(&mut ic) {
            if clause.kind() != "import_clause" {
                continue;
            }
            let mut cc = clause.walk();
            for part in clause.children(&mut cc) {
                match part.kind() {
                    "identifier" => {
                        let local = node_text(part, src).to_string();
                        if resolved_file.is_some() {
                            let target_name = if path.ends_with(".svelte") {
                                Path::new(&path)
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or(&local)
                                    .to_string()
                            } else {
                                "<default>".to_string()
                            };
                            bindings.insert(
                                local,
                                ImportedBinding {
                                    target_name,
                                    resolved_file: resolved_file.clone(),
                                },
                            );
                        }
                    }
                    "named_imports" => {
                        let mut nc = part.walk();
                        for spec in part.children(&mut nc) {
                            if spec.kind() != "import_specifier" {
                                continue;
                            }
                            let spec_is_type = {
                                let mut sc = spec.walk();
                                spec.children(&mut sc).any(|k| k.kind() == "type")
                            };
                            if spec_is_type {
                                continue;
                            }
                            if let Some(orig_node) = spec.child_by_field_name("name") {
                                let orig = node_text(orig_node, src).to_string();
                                let local = spec
                                    .child_by_field_name("alias")
                                    .map(|a| node_text(a, src).to_string())
                                    .unwrap_or_else(|| orig.clone());
                                if orig == "default" {
                                    if resolved_file.is_some() {
                                        let target_name = if path.ends_with(".svelte") {
                                            Path::new(&path)
                                                .file_stem()
                                                .and_then(|s| s.to_str())
                                                .unwrap_or(&local)
                                                .to_string()
                                        } else {
                                            "<default>".to_string()
                                        };
                                        bindings.insert(
                                            local,
                                            ImportedBinding {
                                                target_name,
                                                resolved_file: resolved_file.clone(),
                                            },
                                        );
                                    }
                                } else {
                                    bindings.insert(
                                        local,
                                        ImportedBinding {
                                            target_name: orig,
                                            resolved_file: resolved_file.clone(),
                                        },
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    bindings
}

fn unwrap_ts_function_value(mut node: Node<'_>) -> Option<Node<'_>> {
    while node.kind() == "parenthesized_expression" {
        node = node.named_child(0)?;
    }
    matches!(node.kind(), "arrow_function" | "function_expression").then_some(node)
}

fn collect_ts_non_func_var_inits(
    decl_node: Node<'_>,
    src: &str,
    line_offset: usize,
    mod_id: &str,
    imported_bindings: &HashMap<String, ImportedBinding>,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) -> bool {
    let mut found = false;
    let empty_shadowed: HashSet<String> = HashSet::new();
    let mut vc = decl_node.walk();
    for decl in decl_node.children(&mut vc) {
        if decl.kind() == "variable_declarator" {
            if let Some(val) = decl.child_by_field_name("value") {
                if unwrap_ts_function_value(val).is_none() {
                    found = true;
                    collect_ts_refs_and_bridges(
                        decl,
                        src,
                        line_offset,
                        mod_id,
                        imported_bindings,
                        &empty_shadowed,
                        refs,
                        bridges,
                    );
                }
            }
        }
    }
    found
}

fn collect_ts_local_bindings(body: Node<'_>, src: &str, params: &[String]) -> HashSet<String> {
    let mut locals: HashSet<String> = params.iter().cloned().collect();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "variable_declarator" | "function_declaration" | "class_declaration" => {
                if let Some(name_node) = n.child_by_field_name("name") {
                    if name_node.kind() == "identifier" {
                        locals.insert(node_text(name_node, src).to_string());
                    }
                }
            }
            "catch_clause" => {
                if let Some(p) = n.child_by_field_name("parameter") {
                    if p.kind() == "identifier" {
                        locals.insert(node_text(p, src).to_string());
                    }
                }
            }
            "arrow_function" | "function_expression" => {
                if let Some(p) = n.child_by_field_name("parameter") {
                    locals.insert(node_text(p, src).to_string());
                }
                for p in extract_ts_params(n, src) {
                    locals.insert(p);
                }
            }
            _ => {}
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    locals
}

#[allow(clippy::too_many_arguments)]
fn walk_ts_nodes(
    parent: Node<'_>,
    src: &str,
    rel_path: &str,
    lang: Language,
    line_offset: usize,
    exported: bool,
    is_test_file: bool,
    is_sveltekit_route: bool,
    imported_bindings: &HashMap<String, ImportedBinding>,
    imports: &mut Vec<String>,
    symbols: &mut Vec<Symbol>,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
    ir_funcs: &mut Vec<IrFunc>,
) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        match child.kind() {
            "import_statement" => {
                if let Some(s) = child.child_by_field_name("source") {
                    imports.push(strip_quotes(node_text(s, src)));
                }
            }
            "export_statement" => {
                let is_default = {
                    let mut ec = child.walk();
                    child.children(&mut ec).any(|k| k.kind() == "default")
                };
                let sym_len_before = symbols.len();
                walk_ts_nodes(
                    child,
                    src,
                    rel_path,
                    lang,
                    line_offset,
                    true,
                    is_test_file,
                    is_sveltekit_route,
                    imported_bindings,
                    imports,
                    symbols,
                    refs,
                    bridges,
                    ir_funcs,
                );
                if is_default {
                    let span = node_span(child, line_offset);
                    let def_id = make_symbol_id(rel_path, "<default>", span.start_line);
                    let has_decl = symbols.len() > sym_len_before;
                    symbols.push(Symbol {
                        id: def_id.clone(),
                        name: "<default>".to_string(),
                        qualified_name: "<default>".to_string(),
                        kind: SymbolKind::Script,
                        lang,
                        file: rel_path.to_string(),
                        span: if has_decl {
                            crate::model::Span { start_line: 0, end_line: 0 }
                        } else {
                            span
                        },
                        visibility: Visibility::Public,
                        is_test: is_test_file,
                        is_entrypoint: false,
                        is_dynamic_candidate: false,
                        package_or_module: rel_path.to_string(),
                    });
                    if has_decl {
                        // Link <default> to the first declared symbol inside export default ...
                        let target_id = symbols[sym_len_before].id.clone();
                        refs.push(RawRef {
                            from_symbol_id: def_id,
                            target_name: target_id,
                            qualifier: None,
                            kind: EdgeKind::Calls,
                            line: span.start_line,
                            receiver_chain: None,
                        });
                    } else if let Some(mut val) = child.child_by_field_name("value") {
                        while val.kind() == "parenthesized_expression" {
                            if let Some(inner) = val.named_child(0) {
                                val = inner;
                            } else {
                                break;
                            }
                        }
                        let empty_shadowed = HashSet::new();
                        if val.kind() == "identifier" {
                            let ident = node_text(val, src);
                            let (target_name, qualifier) = imported_bindings
                                .get(ident)
                                .map(|b| (b.target_name.clone(), b.resolved_file.clone()))
                                .unwrap_or_else(|| (ident.to_string(), None));
                            refs.push(RawRef {
                                from_symbol_id: def_id,
                                target_name,
                                qualifier,
                                kind: EdgeKind::Calls,
                                line: span.start_line,
                                receiver_chain: None,
                            });
                        } else {
                            collect_ts_refs_and_bridges(
                                val,
                                src,
                                line_offset,
                                &def_id,
                                imported_bindings,
                                &empty_shadowed,
                                refs,
                                bridges,
                            );
                        }
                    }
                } else {
                    // Handle export { localFn as default }; and export { run as default } from './mod';
                    let reexport_source = child
                        .child_by_field_name("source")
                        .map(|s| strip_quotes(node_text(s, src)));
                    if let Some(ref spath) = reexport_source {
                        imports.push(spath.clone());
                    }
                    let mut ec = child.walk();
                    for clause in child.children(&mut ec) {
                        if clause.kind() != "export_clause" {
                            continue;
                        }
                        let mut sc = clause.walk();
                        for spec in clause.children(&mut sc) {
                            if spec.kind() != "export_specifier" {
                                continue;
                            }
                            let alias = spec.child_by_field_name("alias").map(|a| node_text(a, src));
                            if alias == Some("default") {
                                if let Some(orig) = spec.child_by_field_name("name") {
                                    let span = node_span(child, line_offset);
                                    let def_id = make_symbol_id(rel_path, "<default>", span.start_line);
                                    symbols.push(Symbol {
                                        id: def_id.clone(),
                                        name: "<default>".to_string(),
                                        qualified_name: "<default>".to_string(),
                                        kind: SymbolKind::Script,
                                        lang,
                                        file: rel_path.to_string(),
                                        span: crate::model::Span { start_line: 0, end_line: 0 },
                                        visibility: Visibility::Public,
                                        is_test: is_test_file,
                                        is_entrypoint: false,
                                        is_dynamic_candidate: false,
                                        package_or_module: rel_path.to_string(),
                                    });
                                    let ident = node_text(orig, src);
                                    if let Some(ref spath) = reexport_source {
                                        if let Some(resolved_file) = resolve_relative_path(rel_path, spath) {
                                            let target_name = if ident == "default" {
                                                if spath.ends_with(".svelte") {
                                                    Path::new(spath)
                                                        .file_stem()
                                                        .and_then(|s| s.to_str())
                                                        .unwrap_or(ident)
                                                        .to_string()
                                                } else {
                                                    "<default>".to_string()
                                                }
                                            } else {
                                                ident.to_string()
                                            };
                                            refs.push(RawRef {
                                                from_symbol_id: def_id,
                                                target_name,
                                                qualifier: Some(resolved_file),
                                                kind: EdgeKind::Calls,
                                                line: span.start_line,
                                                receiver_chain: None,
                                            });
                                        }
                                    } else {
                                        let (target_name, qualifier) = imported_bindings
                                            .get(ident)
                                            .map(|b| (b.target_name.clone(), b.resolved_file.clone()))
                                            .unwrap_or_else(|| (ident.to_string(), None));
                                        refs.push(RawRef {
                                            from_symbol_id: def_id,
                                            target_name,
                                            qualifier,
                                            kind: EdgeKind::Calls,
                                            line: span.start_line,
                                            receiver_chain: None,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "function_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, src).to_string();
                    let span = node_span(child, line_offset);
                    let id = make_symbol_id(rel_path, &name, span.start_line);
                    let is_entry = is_sveltekit_route
                        && matches!(name.as_str(), "load" | "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "actions");
                    symbols.push(Symbol {
                        id: id.clone(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind: SymbolKind::Function,
                        lang,
                        file: rel_path.to_string(),
                        span,
                        visibility: if exported { Visibility::Public } else { Visibility::Private },
                        is_test: is_test_file,
                        is_entrypoint: is_entry,
                        is_dynamic_candidate: false,
                        package_or_module: rel_path.to_string(),
                    });
                    if let Some(body) = child.child_by_field_name("body") {
                        let params = extract_ts_params(child, src);
                        let shadowed = collect_ts_local_bindings(body, src, &params);
                        if let Some(pnode) = child.child_by_field_name("parameters") {
                            collect_ts_refs_and_bridges(pnode, src, line_offset, &id, imported_bindings, &shadowed, refs, bridges);
                        }
                        collect_ts_refs_and_bridges(body, src, line_offset, &id, imported_bindings, &shadowed, refs, bridges);
                        ir_funcs.push(IrFunc {
                            symbol_id: id,
                            name,
                            file: rel_path.to_string(),
                            lang,
                            span,
                            params,
                            body: lower_ts_node(body, src),
                        });
                    }
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut vc = child.walk();
                for decl in child.children(&mut vc) {
                    if decl.kind() == "variable_declarator" {
                        if let (Some(name_node), Some(val)) =
                            (decl.child_by_field_name("name"), decl.child_by_field_name("value"))
                        {
                            if let Some(fn_val) = unwrap_ts_function_value(val) {
                                let name = node_text(name_node, src).to_string();
                                let span = node_span(decl, line_offset);
                                let id = make_symbol_id(rel_path, &name, span.start_line);
                                let is_entry = is_sveltekit_route
                                    && matches!(name.as_str(), "load" | "GET" | "POST" | "PUT" | "DELETE" | "actions");
                                symbols.push(Symbol {
                                    id: id.clone(),
                                    name: name.clone(),
                                    qualified_name: name.clone(),
                                    kind: SymbolKind::Function,
                                    lang,
                                    file: rel_path.to_string(),
                                    span,
                                    visibility: if exported { Visibility::Public } else { Visibility::Private },
                                    is_test: is_test_file,
                                    is_entrypoint: is_entry,
                                    is_dynamic_candidate: false,
                                    package_or_module: rel_path.to_string(),
                                });
                                let params = extract_ts_params(fn_val, src);
                                let shadowed = fn_val
                                    .child_by_field_name("body")
                                    .map(|b| collect_ts_local_bindings(b, src, &params))
                                    .unwrap_or_else(|| params.iter().cloned().collect());
                                if let Some(pnode) = fn_val.child_by_field_name("parameters") {
                                    collect_ts_refs_and_bridges(pnode, src, line_offset, &id, imported_bindings, &shadowed, refs, bridges);
                                }
                                if let Some(body) = fn_val.child_by_field_name("body") {
                                    collect_ts_refs_and_bridges(body, src, line_offset, &id, imported_bindings, &shadowed, refs, bridges);
                                    ir_funcs.push(IrFunc {
                                        symbol_id: id,
                                        name,
                                        file: rel_path.to_string(),
                                        lang,
                                        span,
                                        params,
                                        body: lower_ts_node(body, src),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            "class_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let cls = node_text(name_node, src).to_string();
                    let span = node_span(child, line_offset);
                    let cls_id = make_symbol_id(rel_path, &cls, span.start_line);
                    symbols.push(Symbol {
                        id: cls_id.clone(),
                        name: cls.clone(),
                        qualified_name: cls.clone(),
                        kind: SymbolKind::Type,
                        lang,
                        file: rel_path.to_string(),
                        span,
                        visibility: if exported { Visibility::Public } else { Visibility::Private },
                        is_test: is_test_file,
                        is_entrypoint: false,
                        is_dynamic_candidate: false,
                        package_or_module: rel_path.to_string(),
                    });
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut bc = body.walk();
                        for member in body.children(&mut bc) {
                            if member.kind() == "method_definition" {
                                if let Some(mname) = member.child_by_field_name("name") {
                                    let m = node_text(mname, src).to_string();
                                    let qname = format!("{}.{}", cls, m);
                                    let mspan = node_span(member, line_offset);
                                    let mid = make_symbol_id(rel_path, &qname, mspan.start_line);
                                    symbols.push(Symbol {
                                        id: mid.clone(),
                                        name: m.clone(),
                                        qualified_name: qname.clone(),
                                        kind: SymbolKind::Method,
                                        lang,
                                        file: rel_path.to_string(),
                                        span: mspan,
                                        visibility: Visibility::Public,
                                        is_test: is_test_file,
                                        is_entrypoint: m == "constructor",
                                        is_dynamic_candidate: true,
                                        package_or_module: rel_path.to_string(),
                                    });
                                    if let Some(mbody) = member.child_by_field_name("body") {
                                        let params = extract_ts_params(member, src);
                                        let shadowed = collect_ts_local_bindings(mbody, src, &params);
                                        if let Some(pnode) = member.child_by_field_name("parameters") {
                                            collect_ts_refs_and_bridges(pnode, src, line_offset, &mid, imported_bindings, &shadowed, refs, bridges);
                                        }
                                        collect_ts_refs_and_bridges(mbody, src, line_offset, &mid, imported_bindings, &shadowed, refs, bridges);
                                        ir_funcs.push(IrFunc {
                                            symbol_id: mid,
                                            name: qname,
                                            file: rel_path.to_string(),
                                            lang,
                                            span: mspan,
                                            params,
                                            body: lower_ts_node(mbody, src),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "interface_declaration" | "type_alias_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, src).to_string();
                    let span = node_span(child, line_offset);
                    let id = make_symbol_id(rel_path, &name, span.start_line);
                    symbols.push(Symbol {
                        id: id.clone(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind: if child.kind() == "interface_declaration" {
                            SymbolKind::Interface
                        } else {
                            SymbolKind::Type
                        },
                        lang,
                        file: rel_path.to_string(),
                        span,
                        visibility: if exported { Visibility::Public } else { Visibility::Private },
                        is_test: is_test_file,
                        is_entrypoint: false,
                        is_dynamic_candidate: false,
                        package_or_module: rel_path.to_string(),
                    });
                    if rel_path.contains("protocol") || rel_path.contains("schema") {
                        bridges.push(BridgeHint {
                            from_symbol_id: id.clone(),
                            bridge_kind: BridgeKind::WireContract,
                            key: format!("type:{}", name),
                            role: BridgeRole::Both,
                            line: span.start_line,
                            confidence: Confidence::Heuristic,
                        });
                    }
                    let empty_shadowed = HashSet::new();
                    collect_ts_refs_and_bridges(child, src, line_offset, &id, imported_bindings, &empty_shadowed, refs, bridges);
                }
            }
            _ => {}
        }
    }
}

fn extract_ts_params(node: Node<'_>, src: &str) -> Vec<String> {
    let mut params = Vec::new();
    if let Some(plist) = node.child_by_field_name("parameters") {
        let mut c = plist.walk();
        for ch in plist.children(&mut c) {
            if matches!(ch.kind(), "required_parameter" | "optional_parameter" | "identifier") {
                if let Some(pat) = ch.child_by_field_name("pattern") {
                    params.push(node_text(pat, src).to_string());
                } else {
                    params.push(node_text(ch, src).to_string());
                }
            }
        }
    }
    params
}

fn is_ts_value_identifier(n: Node<'_>) -> bool {
    let mut anc = n.parent();
    while let Some(a) = anc {
        if matches!(
            a.kind(),
            "type_query"
                | "type_annotation"
                | "type_alias_declaration"
                | "interface_declaration"
                | "type_arguments"
                | "type_parameters"
        ) {
            return false;
        }
        anc = a.parent();
    }
    let Some(parent) = n.parent() else {
        return true;
    };
    match parent.kind() {
        "import_clause"
        | "import_specifier"
        | "namespace_import"
        | "required_parameter"
        | "optional_parameter"
        | "rest_pattern"
        | "labeled_statement"
        | "property_signature" => false,
        "member_expression" => {
            parent.child_by_field_name("property").map(|p| p.id()) != Some(n.id())
        }
        "call_expression" => {
            parent.child_by_field_name("function").map(|f| f.id()) != Some(n.id())
        }
        "variable_declarator"
        | "function_declaration"
        | "class_declaration"
        | "method_definition" => {
            parent.child_by_field_name("name").map(|nm| nm.id()) != Some(n.id())
        }
        "pair" => parent.child_by_field_name("key").map(|k| k.id()) != Some(n.id()),
        _ => true,
    }
}

fn collect_ts_refs_and_bridges(
    node: Node<'_>,
    src: &str,
    line_offset: usize,
    from_id: &str,
    imported_bindings: &HashMap<String, ImportedBinding>,
    shadowed: &HashSet<String>,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let line = n.start_position().row + 1 + line_offset;
        match n.kind() {
            "call_expression" => {
                if let Some(func) = n.child_by_field_name("function") {
                    match func.kind() {
                        "identifier" => {
                            let ident = node_text(func, src);
                            let (target_name, qualifier) = if !shadowed.contains(ident) {
                                imported_bindings
                                    .get(ident)
                                    .map(|b| (b.target_name.clone(), b.resolved_file.clone()))
                                    .unwrap_or_else(|| (ident.to_string(), None))
                            } else {
                                (ident.to_string(), None)
                            };
                            refs.push(RawRef {
                                from_symbol_id: from_id.to_string(),
                                target_name,
                                qualifier,
                                kind: EdgeKind::Calls,
                                line,
                                receiver_chain: None,
                            });
                        }
                        "member_expression" => {
                            let obj = func.child_by_field_name("object").map(|o| node_text(o, src).to_string());
                            let prop = func.child_by_field_name("property").map(|p| node_text(p, src).to_string());
                            if let Some(target) = prop {
                                refs.push(RawRef {
                                    from_symbol_id: from_id.to_string(),
                                    target_name: target,
                                    qualifier: obj,
                                    kind: EdgeKind::Calls,
                                    line,
                                    receiver_chain: None,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "member_expression" => {
                let txt = node_text(n, src);
                if let Some(var) = txt.strip_prefix("process.env.").or_else(|| txt.strip_prefix("import.meta.env.")) {
                    if var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                        bridges.push(BridgeHint {
                            from_symbol_id: from_id.to_string(),
                            bridge_kind: BridgeKind::EnvKey,
                            key: var.to_string(),
                            role: BridgeRole::Consumer,
                            line,
                            confidence: Confidence::Resolved,
                        });
                    }
                }
            }
            "string" | "template_string" => {
                let raw = node_text(n, src);
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
            "identifier" => {
                let ident = node_text(n, src);
                if !shadowed.contains(ident) {
                    if let Some(binding) = imported_bindings.get(ident) {
                        if is_ts_value_identifier(n) {
                            refs.push(RawRef {
                                from_symbol_id: from_id.to_string(),
                                target_name: binding.target_name.clone(),
                                qualifier: binding.resolved_file.clone(),
                                kind: EdgeKind::Calls,
                                line,
                                receiver_chain: None,
                            });
                        }
                    }
                }
            }
            "shorthand_property_identifier" => {
                let ident = node_text(n, src);
                if !shadowed.contains(ident) {
                    if let Some(binding) = imported_bindings.get(ident) {
                        refs.push(RawRef {
                            from_symbol_id: from_id.to_string(),
                            target_name: binding.target_name.clone(),
                            qualifier: binding.resolved_file.clone(),
                            kind: EdgeKind::Calls,
                            line,
                            receiver_chain: None,
                        });
                    }
                }
            }
            "type_identifier" => {
                let t = node_text(n, src);
                if !matches!(t, "string" | "number" | "boolean" | "void" | "any" | "unknown" | "never" | "Promise" | "Array" | "Record" | "Partial") {
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
            _ => {}
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
}

pub fn lower_ts_node(node: Node<'_>, src: &str) -> IrNode {
    match node.kind() {
        "statement_block" => {
            let mut stmts = Vec::new();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.is_named() && ch.kind() != "comment" {
                    stmts.push(lower_ts_node(ch, src));
                }
            }
            IrNode::Seq(stmts)
        }
        "expression_statement" => node
            .named_child(0)
            .map(|n| lower_ts_node(n, src))
            .unwrap_or_else(|| IrNode::Seq(vec![])),
        "for_statement" => {
            let body = node
                .child_by_field_name("body")
                .map(|b| lower_ts_node(b, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            if let (Some(init), Some(cond), Some(inc)) = (
                node.child_by_field_name("initializer"),
                node.child_by_field_name("condition"),
                node.child_by_field_name("increment"),
            ) {
                if let Some((var, start, end, step)) = extract_ts_counting_loop(init, cond, inc, src) {
                    return IrNode::Loop {
                        kind: LoopKind::IterRange {
                            start: Box::new(start),
                            end: Box::new(end),
                            step: Box::new(step),
                        },
                        var: Some(var),
                        body: Box::new(body),
                    };
                }
            }
            let cond = node
                .child_by_field_name("condition")
                .map(|c| lower_ts_node(c, src))
                .unwrap_or(IrNode::Lit(LitClass::Bool(true)));
            IrNode::Loop {
                kind: LoopKind::While { cond: Box::new(cond) },
                var: None,
                body: Box::new(body),
            }
        }
        "for_in_statement" => {
            let var = node.child_by_field_name("left").map(|l| {
                node_text(l, src)
                    .trim()
                    .trim_start_matches("const ")
                    .trim_start_matches("let ")
                    .trim_start_matches("var ")
                    .to_string()
            });
            let seq = node
                .child_by_field_name("right")
                .map(|r| lower_ts_node(r, src))
                .unwrap_or_else(|| IrNode::Opaque("seq".into()));
            let body = node
                .child_by_field_name("body")
                .map(|b| lower_ts_node(b, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            IrNode::Loop {
                kind: LoopKind::IterSeq { seq: Box::new(seq) },
                var,
                body: Box::new(body),
            }
        }
        "while_statement" => {
            let cond = node
                .child_by_field_name("condition")
                .map(|c| lower_ts_node(c, src))
                .unwrap_or(IrNode::Lit(LitClass::Bool(true)));
            let body = node
                .child_by_field_name("body")
                .map(|b| lower_ts_node(b, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            IrNode::Loop {
                kind: LoopKind::While { cond: Box::new(cond) },
                var: None,
                body: Box::new(body),
            }
        }
        "if_statement" => {
            let cond = node
                .child_by_field_name("condition")
                .map(|c| lower_ts_node(c, src))
                .unwrap_or_else(|| IrNode::Opaque("cond".into()));
            let then_br = node
                .child_by_field_name("consequence")
                .map(|b| lower_ts_node(b, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            let else_br = node.child_by_field_name("alternative").map(|alt| {
                let inner = alt.named_child(0).unwrap_or(alt);
                Box::new(lower_ts_node(inner, src))
            });
            IrNode::Branch {
                cond: Box::new(cond),
                then_br: Box::new(then_br),
                else_br,
            }
        }
        "switch_statement" => {
            let scrutinee = node
                .child_by_field_name("value")
                .map(|v| Box::new(lower_ts_node(v, src)));
            let mut arms = Vec::new();
            let mut default = None;
            if let Some(body) = node.child_by_field_name("body") {
                let mut c = body.walk();
                for ch in body.children(&mut c) {
                    if ch.kind() == "switch_case" {
                        let pat = ch
                            .child_by_field_name("value")
                            .map(|v| lower_ts_node(v, src))
                            .unwrap_or_else(|| IrNode::Opaque("case".into()));
                        let b = lower_ts_case_body(ch, src);
                        arms.push((pat, b));
                    } else if ch.kind() == "switch_default" {
                        default = Some(Box::new(lower_ts_case_body(ch, src)));
                    }
                }
            }
            IrNode::MultiBranch {
                scrutinee,
                arms,
                default,
            }
        }
        "return_statement" => {
            let val = node.named_child(0).map(|n| Box::new(lower_ts_node(n, src)));
            IrNode::Return(val)
        }
        "throw_statement" => {
            let val = node
                .named_child(0)
                .map(|n| lower_ts_node(n, src))
                .unwrap_or(IrNode::Lit(LitClass::Null));
            IrNode::PropagateErr(Box::new(val))
        }
        "try_statement" => {
            let mut args = Vec::new();
            if let Some(body) = node.child_by_field_name("body") {
                args.push(lower_ts_node(body, src));
            }
            if let Some(handler) = node.child_by_field_name("handler") {
                let mut catch_stmts = Vec::new();
                if let Some(param) = handler.child_by_field_name("parameter") {
                    catch_stmts.push(IrNode::Assign {
                        dst: node_text(param, src).to_string(),
                        val: Box::new(IrNode::Var("<catch>".into())),
                    });
                }
                if let Some(cbody) = handler.child_by_field_name("body") {
                    catch_stmts.push(lower_ts_node(cbody, src));
                }
                args.push(IrNode::Call {
                    callee: "catch".into(),
                    args: catch_stmts,
                });
            }
            if let Some(finalizer) = node.child_by_field_name("finalizer") {
                if let Some(fbody) = finalizer.child_by_field_name("body") {
                    args.push(IrNode::Call {
                        callee: "finally".into(),
                        args: vec![lower_ts_node(fbody, src)],
                    });
                }
            }
            IrNode::Call {
                callee: "try".into(),
                args,
            }
        }
        "break_statement" => IrNode::Break,
        "continue_statement" => IrNode::Continue,
        "lexical_declaration" | "variable_declaration" => {
            let mut stmts = Vec::new();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.kind() == "variable_declarator" {
                    let dst = ch
                        .child_by_field_name("name")
                        .map(|n| node_text(n, src).to_string())
                        .unwrap_or_else(|| "_".into());
                    let val = ch
                        .child_by_field_name("value")
                        .map(|v| lower_ts_node(v, src))
                        .unwrap_or(IrNode::Lit(LitClass::Null));
                    stmts.push(IrNode::Assign {
                        dst,
                        val: Box::new(val),
                    });
                }
            }
            IrNode::Seq(stmts)
        }
        "assignment_expression" => {
            let dst = node
                .child_by_field_name("left")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_else(|| "_".into());
            let val = node
                .child_by_field_name("right")
                .map(|v| lower_ts_node(v, src))
                .unwrap_or_else(|| IrNode::Opaque("rhs".into()));
            IrNode::Assign {
                dst,
                val: Box::new(val),
            }
        }
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
                        args.push(lower_ts_node(ch, src));
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
                .map(|n| lower_ts_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("lhs".into()));
            let rhs = node
                .child_by_field_name("right")
                .map(|n| lower_ts_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("rhs".into()));
            IrNode::BinOp {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            }
        }
        "unary_expression" => {
            let op = node
                .child_by_field_name("operator")
                .map(|n| map_unop(node_text(n, src)))
                .unwrap_or(crate::ir::UnOpClass::Other);
            let expr = node
                .child_by_field_name("argument")
                .map(|n| lower_ts_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("unop".into()));
            IrNode::UnOp {
                op,
                expr: Box::new(expr),
            }
        }
        "member_expression" => {
            let obj = node
                .child_by_field_name("object")
                .map(|n| lower_ts_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("obj".into()));
            let field = node
                .child_by_field_name("property")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            IrNode::Member {
                obj: Box::new(obj),
                field,
            }
        }
        "subscript_expression" => {
            let obj = node
                .child_by_field_name("object")
                .map(|n| lower_ts_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("arr".into()));
            let idx = node
                .child_by_field_name("index")
                .map(|n| lower_ts_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("idx".into()));
            IrNode::Index {
                obj: Box::new(obj),
                idx: Box::new(idx),
            }
        }
        "await_expression" => {
            let inner = node
                .named_child(0)
                .map(|n| lower_ts_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("await".into()));
            IrNode::Call {
                callee: "await".into(),
                args: vec![inner],
            }
        }
        "ternary_expression" => {
            let cond = node
                .child_by_field_name("condition")
                .map(|c| lower_ts_node(c, src))
                .unwrap_or_else(|| IrNode::Opaque("cond".into()));
            let then_br = node
                .child_by_field_name("consequence")
                .map(|b| lower_ts_node(b, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            let else_br = node
                .child_by_field_name("alternative")
                .map(|b| Box::new(lower_ts_node(b, src)));
            IrNode::Branch {
                cond: Box::new(cond),
                then_br: Box::new(then_br),
                else_br,
            }
        }
        "arrow_function" => {
            let mut args = Vec::new();
            if let Some(p) = node.child_by_field_name("parameter") {
                args.push(IrNode::Assign {
                    dst: node_text(p, src).to_string(),
                    val: Box::new(IrNode::Lit(LitClass::Null)),
                });
            } else {
                for p in extract_ts_params(node, src) {
                    args.push(IrNode::Assign {
                        dst: p,
                        val: Box::new(IrNode::Lit(LitClass::Null)),
                    });
                }
            }
            if let Some(body) = node.child_by_field_name("body") {
                args.push(lower_ts_node(body, src));
            }
            IrNode::Call {
                callee: "<fn>".into(),
                args,
            }
        }
        "object" => {
            let mut props = Vec::new();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if !ch.is_named() || ch.kind() == "comment" {
                    continue;
                }
                match ch.kind() {
                    "pair" => {
                        let val = ch
                            .child_by_field_name("value")
                            .map(|v| lower_ts_node(v, src))
                            .unwrap_or(IrNode::Lit(LitClass::Null));
                        if let Some(key_node) = ch.child_by_field_name("key") {
                            if key_node.kind() == "computed_property_name" {
                                let mut kc = key_node.walk();
                                let k_ir = key_node
                                    .children(&mut kc)
                                    .find(|n| n.is_named() && n.kind() != "comment")
                                    .map(|e| lower_ts_node(e, src))
                                    .unwrap_or_else(|| IrNode::Opaque("computed_key".into()));
                                props.push(IrNode::Call {
                                    callee: "<computed_pair>".into(),
                                    args: vec![k_ir, val],
                                });
                            } else {
                                props.push(IrNode::Member {
                                    obj: Box::new(val),
                                    field: strip_quotes(node_text(key_node, src)),
                                });
                            }
                        } else {
                            props.push(val);
                        }
                    }
                    "shorthand_property_identifier" => {
                        let name = node_text(ch, src).to_string();
                        props.push(IrNode::Member {
                            obj: Box::new(IrNode::Var(name.clone())),
                            field: name,
                        });
                    }
                    _ => props.push(lower_ts_node(ch, src)),
                }
            }
            IrNode::Call {
                callee: "<obj>".into(),
                args: props,
            }
        }
        "array" => {
            let mut elems = Vec::new();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.is_named() && ch.kind() != "comment" {
                    elems.push(lower_ts_node(ch, src));
                }
            }
            IrNode::Call {
                callee: "<array>".into(),
                args: elems,
            }
        }
        "spread_element" => {
            let inner = node
                .named_child(0)
                .map(|n| lower_ts_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("spread".into()));
            IrNode::Call {
                callee: "<spread>".into(),
                args: vec![inner],
            }
        }
        "parenthesized_expression" => node
            .named_child(0)
            .map(|n| lower_ts_node(n, src))
            .unwrap_or_else(|| IrNode::Opaque("paren".into())),
        "identifier" | "property_identifier" => IrNode::Var(node_text(node, src).to_string()),
        "number" | "string" | "template_string" | "true" | "false" | "null" | "undefined" => {
            literal_node(node_text(node, src))
        }
        _ => IrNode::Opaque(node.kind().to_string()),
    }
}

fn lower_ts_case_body(case_node: Node<'_>, src: &str) -> IrNode {
    let mut stmts = Vec::new();
    let mut terminates = false;
    let mut c = case_node.walk();
    for ch in case_node.children(&mut c) {
        if !ch.is_named() || ch.kind() == "comment" {
            continue;
        }
        if case_node.child_by_field_name("value").map(|v| v.id()) == Some(ch.id()) {
            continue;
        }
        if ch.kind() == "break_statement" {
            terminates = true;
            continue;
        }
        if matches!(ch.kind(), "return_statement" | "throw_statement") {
            terminates = true;
        }
        stmts.push(lower_ts_node(ch, src));
    }
    if !stmts.is_empty() && !terminates {
        stmts.push(IrNode::Opaque("fallthrough".into()));
    }
    IrNode::Seq(stmts)
}

fn extract_ts_counting_loop(
    init: Node<'_>,
    cond: Node<'_>,
    inc: Node<'_>,
    src: &str,
) -> Option<(String, IrNode, IrNode, IrNode)> {
    // let i = 0
    let mut var = None;
    let mut start = None;
    let mut c = init.walk();
    for ch in init.children(&mut c) {
        if ch.kind() == "variable_declarator" {
            var = ch.child_by_field_name("name").map(|n| node_text(n, src).to_string());
            start = ch.child_by_field_name("value").map(|v| lower_ts_node(v, src));
        }
    }
    let (var, start) = (var?, start?);
    let cond_expr = if cond.kind() == "expression_statement" {
        cond.named_child(0)?
    } else {
        cond
    };
    if cond_expr.kind() != "binary_expression" {
        return None;
    }
    let op = node_text(cond_expr.child_by_field_name("operator")?, src).trim();
    if op != "<" {
        return None;
    }
    if node_text(cond_expr.child_by_field_name("left")?, src).trim() != var {
        return None;
    }
    let end = lower_ts_node(cond_expr.child_by_field_name("right")?, src);
    let inc_txt = node_text(inc, src);
    let step = if inc_txt.contains("++") {
        IrNode::Lit(LitClass::One)
    } else {
        return None;
    };
    Some((var, start, end, step))
}
