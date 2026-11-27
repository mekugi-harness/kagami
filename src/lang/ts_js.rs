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
use tree_sitter::{Node, Parser};

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
                                });
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
                    collect_ts_refs_and_bridges(ch, src, line_offset, &mod_id, &mut refs, &mut bridges);
                }
                "lexical_declaration" | "variable_declaration" => {
                    if collect_ts_non_func_var_inits(ch, src, line_offset, &mod_id, &mut refs, &mut bridges) {
                        has_top_level_exec = true;
                    }
                }
                "export_statement" => {
                    let mut ec = ch.walk();
                    for ech in ch.children(&mut ec) {
                        if matches!(ech.kind(), "lexical_declaration" | "variable_declaration")
                            && collect_ts_non_func_var_inits(ech, src, line_offset, &mod_id, &mut refs, &mut bridges)
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
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) -> bool {
    let mut found = false;
    let mut vc = decl_node.walk();
    for decl in decl_node.children(&mut vc) {
        if decl.kind() == "variable_declarator" {
            if let Some(val) = decl.child_by_field_name("value") {
                if unwrap_ts_function_value(val).is_none() {
                    found = true;
                    collect_ts_refs_and_bridges(decl, src, line_offset, mod_id, refs, bridges);
                }
            }
        }
    }
    found
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
                walk_ts_nodes(
                    child,
                    src,
                    rel_path,
                    lang,
                    line_offset,
                    true,
                    is_test_file,
                    is_sveltekit_route,
                    imports,
                    symbols,
                    refs,
                    bridges,
                    ir_funcs,
                );
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
                        if let Some(pnode) = child.child_by_field_name("parameters") {
                            collect_ts_refs_and_bridges(pnode, src, line_offset, &id, refs, bridges);
                        }
                        collect_ts_refs_and_bridges(body, src, line_offset, &id, refs, bridges);
                        let params = extract_ts_params(child, src);
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
                                if let Some(pnode) = fn_val.child_by_field_name("parameters") {
                                    collect_ts_refs_and_bridges(pnode, src, line_offset, &id, refs, bridges);
                                }
                                if let Some(body) = fn_val.child_by_field_name("body") {
                                    collect_ts_refs_and_bridges(body, src, line_offset, &id, refs, bridges);
                                    let params = extract_ts_params(fn_val, src);
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
                                        if let Some(pnode) = member.child_by_field_name("parameters") {
                                            collect_ts_refs_and_bridges(pnode, src, line_offset, &mid, refs, bridges);
                                        }
                                        collect_ts_refs_and_bridges(mbody, src, line_offset, &mid, refs, bridges);
                                        let params = extract_ts_params(member, src);
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
                    collect_ts_refs_and_bridges(child, src, line_offset, &id, refs, bridges);
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

fn collect_ts_refs_and_bridges(
    node: Node<'_>,
    src: &str,
    line_offset: usize,
    from_id: &str,
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
                            refs.push(RawRef {
                                from_symbol_id: from_id.to_string(),
                                target_name: node_text(func, src).to_string(),
                                qualifier: None,
                                kind: EdgeKind::Calls,
                                line,
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
            "type_identifier" => {
                let t = node_text(n, src);
                if !matches!(t, "string" | "number" | "boolean" | "void" | "any" | "unknown" | "never" | "Promise" | "Array" | "Record" | "Partial") {
                    refs.push(RawRef {
                        from_symbol_id: from_id.to_string(),
                        target_name: t.to_string(),
                        qualifier: None,
                        kind: EdgeKind::TypeRef,
                        line,
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
