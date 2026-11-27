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

pub fn extract(rel_path: &str, src: &str, blake3_hex: String) -> Result<FileExtract> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_go::LANGUAGE.into())?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("failed to parse Go file {}", rel_path))?;

    let is_test_file = rel_path.ends_with("_test.go");
    let mut pkg = String::new();
    let mut imports = Vec::new();
    let mut symbols = Vec::new();
    let mut refs = Vec::new();
    let mut bridges = Vec::new();
    let mut ir_funcs = Vec::new();

    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "package_clause" => {
                if let Some(id) = find_child_by_kind(child, "package_identifier") {
                    pkg = node_text(id, src).to_string();
                }
            }
            "import_declaration" => {
                collect_imports(child, src, &mut imports);
            }
            "function_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, src).to_string();
                    let span = node_span(child, 0);
                    let id = make_symbol_id(rel_path, &name, span.start_line);
                    let vis = go_visibility(&name);
                    let is_test = is_test_file
                        || name.starts_with("Test")
                        || name.starts_with("Benchmark")
                        || name.starts_with("Fuzz")
                        || name.starts_with("Example");
                    let is_entrypoint = name == "main" || name == "init";
                    symbols.push(Symbol {
                        id: id.clone(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind: SymbolKind::Function,
                        lang: Language::Go,
                        file: rel_path.to_string(),
                        span,
                        visibility: vis,
                        is_test,
                        is_entrypoint,
                        is_dynamic_candidate: false,
                        package_or_module: pkg.clone(),
                    });
                    if let Some(body) = child.child_by_field_name("body") {
                        collect_refs_and_bridges(body, src, &id, &mut refs, &mut bridges);
                        let params = extract_go_params(child, src);
                        let ir_body = lower_go_node(body, src);
                        ir_funcs.push(IrFunc {
                            symbol_id: id,
                            name,
                            file: rel_path.to_string(),
                            lang: Language::Go,
                            span,
                            params,
                            body: ir_body,
                        });
                    }
                }
            }
            "method_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, src).to_string();
                    let recv = extract_receiver_type(child, src);
                    let qname = if recv.is_empty() {
                        name.clone()
                    } else {
                        format!("{}.{}", recv, name)
                    };
                    let span = node_span(child, 0);
                    let id = make_symbol_id(rel_path, &qname, span.start_line);
                    let vis = go_visibility(&name);
                    symbols.push(Symbol {
                        id: id.clone(),
                        name: name.clone(),
                        qualified_name: qname.clone(),
                        kind: SymbolKind::Method,
                        lang: Language::Go,
                        file: rel_path.to_string(),
                        span,
                        visibility: vis,
                        is_test: is_test_file,
                        is_entrypoint: false,
                        is_dynamic_candidate: vis == Visibility::Public,
                        package_or_module: pkg.clone(),
                    });
                    if let Some(body) = child.child_by_field_name("body") {
                        collect_refs_and_bridges(body, src, &id, &mut refs, &mut bridges);
                        let params = extract_go_params(child, src);
                        let ir_body = lower_go_node(body, src);
                        ir_funcs.push(IrFunc {
                            symbol_id: id,
                            name: qname,
                            file: rel_path.to_string(),
                            lang: Language::Go,
                            span,
                            params,
                            body: ir_body,
                        });
                    }
                }
            }
            "type_declaration" => {
                let mut tc = child.walk();
                for spec in child.children(&mut tc) {
                    if spec.kind() == "type_spec" {
                        if let Some(name_node) = spec.child_by_field_name("name") {
                            let name = node_text(name_node, src).to_string();
                            let span = node_span(spec, 0);
                            let id = make_symbol_id(rel_path, &name, span.start_line);
                            let kind = spec
                                .child_by_field_name("type")
                                .map(|t| match t.kind() {
                                    "struct_type" => SymbolKind::Struct,
                                    "interface_type" => SymbolKind::Interface,
                                    _ => SymbolKind::Type,
                                })
                                .unwrap_or(SymbolKind::Type);
                            symbols.push(Symbol {
                                id: id.clone(),
                                name: name.clone(),
                                qualified_name: name.clone(),
                                kind,
                                lang: Language::Go,
                                file: rel_path.to_string(),
                                span,
                                visibility: go_visibility(&name),
                                is_test: is_test_file,
                                is_entrypoint: false,
                                is_dynamic_candidate: kind == SymbolKind::Interface,
                                package_or_module: pkg.clone(),
                            });
                            // Emit WireContract type bridge for exported structs with json tags
                            if kind == SymbolKind::Struct && node_text(spec, src).contains("json:") {
                                bridges.push(BridgeHint {
                                    from_symbol_id: id.clone(),
                                    bridge_kind: BridgeKind::WireContract,
                                    key: format!("type:{}", name),
                                    role: BridgeRole::Both,
                                    line: span.start_line,
                                    confidence: Confidence::Heuristic,
                                });
                            }
                            collect_refs_and_bridges(spec, src, &id, &mut refs, &mut bridges);
                        }
                    }
                }
            }
            "const_declaration" | "var_declaration" => {
                let mut vc = child.walk();
                for spec in child.children(&mut vc) {
                    if spec.kind() == "const_spec" || spec.kind() == "var_spec" {
                        if let Some(name_node) = spec.child_by_field_name("name") {
                            let name = node_text(name_node, src).to_string();
                            let span = node_span(spec, 0);
                            let id = make_symbol_id(rel_path, &name, span.start_line);
                            symbols.push(Symbol {
                                id: id.clone(),
                                name: name.clone(),
                                qualified_name: name.clone(),
                                kind: SymbolKind::Const,
                                lang: Language::Go,
                                file: rel_path.to_string(),
                                span,
                                visibility: go_visibility(&name),
                                is_test: is_test_file,
                                is_entrypoint: false,
                                is_dynamic_candidate: false,
                                package_or_module: pkg.clone(),
                            });
                            collect_refs_and_bridges(spec, src, &id, &mut refs, &mut bridges);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(FileExtract {
        file: rel_path.to_string(),
        lang: Language::Go,
        blake3: blake3_hex,
        package_or_module: pkg,
        imports,
        symbols,
        refs,
        bridges,
        ir_funcs,
    })
}

fn go_visibility(name: &str) -> Visibility {
    if name.chars().next().map_or(false, |c| c.is_ascii_uppercase()) {
        Visibility::Public
    } else {
        Visibility::Private
    }
}

fn find_child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        if ch.kind() == kind {
            return Some(ch);
        }
    }
    None
}

fn collect_imports(node: Node<'_>, src: &str, imports: &mut Vec<String>) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "import_spec" {
            let alias = n.child_by_field_name("name").map(|a| node_text(a, src).trim().to_string());
            if let Some(path_node) = n.child_by_field_name("path") {
                let path = strip_quotes(node_text(path_node, src));
                if let Some(a) = alias {
                    if a != "_" && a != "." {
                        imports.push(format!("{}={}", a, path));
                        continue;
                    }
                }
                imports.push(path);
            }
        } else {
            let mut c = n.walk();
            for ch in n.children(&mut c) {
                stack.push(ch);
            }
        }
    }
}

fn extract_receiver_type(node: Node<'_>, src: &str) -> String {
    let Some(recv) = node.child_by_field_name("receiver") else {
        return String::new();
    };
    let mut stack = vec![recv];
    while let Some(n) = stack.pop() {
        if n.kind() == "type_identifier" {
            return node_text(n, src).to_string();
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    String::new()
}

fn extract_go_params(node: Node<'_>, src: &str) -> Vec<String> {
    let mut params = Vec::new();
    if let Some(p) = node.child_by_field_name("parameters") {
        let mut c = p.walk();
        for ch in p.children(&mut c) {
            if ch.kind() == "parameter_declaration" {
                let mut pc = ch.walk();
                for sub in ch.children(&mut pc) {
                    if sub.kind() == "identifier" {
                        params.push(node_text(sub, src).to_string());
                    }
                }
            }
        }
    }
    params
}

fn collect_refs_and_bridges(
    node: Node<'_>,
    src: &str,
    from_id: &str,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "call_expression" => {
                if let Some(func) = n.child_by_field_name("function") {
                    let line = n.start_position().row + 1;
                    match func.kind() {
                        "identifier" => {
                            let target = node_text(func, src).to_string();
                            refs.push(RawRef {
                                from_symbol_id: from_id.to_string(),
                                target_name: target,
                                qualifier: None,
                                kind: EdgeKind::Calls,
                                line,
                            });
                        }
                        "selector_expression" => {
                            let operand = func.child_by_field_name("operand").map(|o| node_text(o, src).to_string());
                            let field = func.child_by_field_name("field").map(|f| node_text(f, src).to_string());
                            if let Some(target) = field {
                                // Check os.Getenv / exec.Command
                                if operand.as_deref() == Some("os") && (target == "Getenv" || target == "LookupEnv") {
                                    if let Some(arg) = first_string_arg(n, src) {
                                        bridges.push(BridgeHint {
                                            from_symbol_id: from_id.to_string(),
                                            bridge_kind: BridgeKind::EnvKey,
                                            key: arg,
                                            role: BridgeRole::Consumer,
                                            line,
                                            confidence: Confidence::Resolved,
                                        });
                                    }
                                } else if operand.as_deref() == Some("exec") && (target == "Command" || target == "CommandContext") {
                                    if let Some(arg) = first_string_arg(n, src) {
                                        bridges.push(BridgeHint {
                                            from_symbol_id: from_id.to_string(),
                                            bridge_kind: BridgeKind::ExecTarget,
                                            key: arg,
                                            role: BridgeRole::Consumer,
                                            line,
                                            confidence: Confidence::Resolved,
                                        });
                                    }
                                }
                                refs.push(RawRef {
                                    from_symbol_id: from_id.to_string(),
                                    target_name: target,
                                    qualifier: operand,
                                    kind: EdgeKind::Calls,
                                    line,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "interpreted_string_literal" | "raw_string_literal" => {
                let raw = node_text(n, src);
                if looks_like_wire_method(raw) {
                    bridges.push(BridgeHint {
                        from_symbol_id: from_id.to_string(),
                        bridge_kind: BridgeKind::WireContract,
                        key: format!("rpc:{}", strip_quotes(raw)),
                        role: BridgeRole::Both,
                        line: n.start_position().row + 1,
                        confidence: Confidence::Heuristic,
                    });
                }
            }
            "keyed_element" => {
                let mut kc = n.walk();
                let named: Vec<_> = n.children(&mut kc).filter(|ch| ch.is_named()).collect();
                if let Some(val_elem) = named.last() {
                    let val = if val_elem.kind() == "literal_element" {
                        val_elem.named_child(0).unwrap_or(*val_elem)
                    } else {
                        *val_elem
                    };
                    let line = val.start_position().row + 1;
                    match val.kind() {
                        "identifier" => {
                            let target = node_text(val, src);
                            if !matches!(target, "true" | "false" | "nil" | "iota") {
                                refs.push(RawRef {
                                    from_symbol_id: from_id.to_string(),
                                    target_name: target.to_string(),
                                    qualifier: None,
                                    kind: EdgeKind::Calls,
                                    line,
                                });
                            }
                        }
                        "selector_expression" => {
                            let operand = val.child_by_field_name("operand").map(|o| node_text(o, src).to_string());
                            let field = val.child_by_field_name("field").map(|f| node_text(f, src).to_string());
                            if let Some(target) = field {
                                refs.push(RawRef {
                                    from_symbol_id: from_id.to_string(),
                                    target_name: target,
                                    qualifier: operand,
                                    kind: EdgeKind::Calls,
                                    line,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "argument_list" => {
                let mut ac = n.walk();
                for ch in n.children(&mut ac) {
                    let line = ch.start_position().row + 1;
                    match ch.kind() {
                        "identifier" => {
                            let target = node_text(ch, src);
                            if !matches!(target, "true" | "false" | "nil" | "err" | "ctx" | "i" | "k" | "v") {
                                refs.push(RawRef {
                                    from_symbol_id: from_id.to_string(),
                                    target_name: target.to_string(),
                                    qualifier: None,
                                    kind: EdgeKind::Calls,
                                    line,
                                });
                            }
                        }
                        "selector_expression" => {
                            let operand = ch.child_by_field_name("operand").map(|o| node_text(o, src).to_string());
                            let field = ch.child_by_field_name("field").map(|f| node_text(f, src).to_string());
                            if let Some(target) = field {
                                refs.push(RawRef {
                                    from_symbol_id: from_id.to_string(),
                                    target_name: target,
                                    qualifier: operand,
                                    kind: EdgeKind::Calls,
                                    line,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "type_identifier" => {
                let target = node_text(n, src);
                if !matches!(
                    target,
                    "string" | "int" | "int64" | "int32" | "uint" | "uint64" | "float64" | "bool" | "byte" | "rune" | "error" | "any"
                ) {
                    refs.push(RawRef {
                        from_symbol_id: from_id.to_string(),
                        target_name: target.to_string(),
                        qualifier: None,
                        kind: EdgeKind::TypeRef,
                        line: n.start_position().row + 1,
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

fn first_string_arg(call: Node<'_>, src: &str) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    let mut c = args.walk();
    for ch in args.children(&mut c) {
        if ch.kind() == "interpreted_string_literal" || ch.kind() == "raw_string_literal" {
            return Some(strip_quotes(node_text(ch, src)));
        }
    }
    None
}

pub fn lower_go_node(node: Node<'_>, src: &str) -> IrNode {
    match node.kind() {
        "block" | "statement_list" => {
            let mut stmts = Vec::new();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.is_named() && ch.kind() != "comment" {
                    stmts.push(lower_go_node(ch, src));
                }
            }
            IrNode::Seq(stmts)
        }
        "for_statement" => lower_go_for(node, src),
        "if_statement" => {
            let cond = node
                .child_by_field_name("condition")
                .map(|n| lower_go_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("cond".into()));
            let then_br = node
                .child_by_field_name("consequence")
                .map(|n| lower_go_node(n, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            let else_br = node
                .child_by_field_name("alternative")
                .map(|n| Box::new(lower_go_node(n, src)));
            let br = IrNode::Branch {
                cond: Box::new(cond),
                then_br: Box::new(then_br),
                else_br,
            };
            if let Some(init) = node.child_by_field_name("initializer") {
                IrNode::Seq(vec![lower_go_node(init, src), br])
            } else {
                br
            }
        }
        "expression_switch_statement" => {
            let scrutinee = node
                .child_by_field_name("value")
                .map(|n| Box::new(lower_go_node(n, src)));
            let mut arms = Vec::new();
            let mut default = None;
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.kind() == "expression_case" {
                    let pat = ch
                        .child_by_field_name("value")
                        .map(|n| lower_go_node(n, src))
                        .unwrap_or_else(|| IrNode::Opaque("case".into()));
                    let body = lower_go_case_body(ch, src);
                    arms.push((pat, body));
                } else if ch.kind() == "default_case" {
                    default = Some(Box::new(lower_go_case_body(ch, src)));
                }
            }
            IrNode::MultiBranch {
                scrutinee,
                arms,
                default,
            }
        }
        "return_statement" => {
            let mut c = node.walk();
            let val = node
                .children(&mut c)
                .find(|ch| ch.is_named())
                .map(|ch| Box::new(lower_go_node(ch, src)));
            IrNode::Return(val)
        }
        "break_statement" => IrNode::Break,
        "continue_statement" => IrNode::Continue,
        "short_var_declaration" | "assignment_statement" => {
            let lhs = node
                .child_by_field_name("left")
                .map(|n| node_text(n, src).trim().to_string())
                .unwrap_or_else(|| "_".into());
            let rhs = node
                .child_by_field_name("right")
                .map(|n| lower_go_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("rhs".into()));
            IrNode::Assign {
                dst: lhs,
                val: Box::new(rhs),
            }
        }
        "var_declaration" => {
            let mut stmts = Vec::new();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.kind() == "var_spec" {
                    let dst = ch
                        .child_by_field_name("name")
                        .map(|n| node_text(n, src).to_string())
                        .unwrap_or_else(|| "_".into());
                    let val = ch
                        .child_by_field_name("value")
                        .map(|n| lower_go_node(n, src))
                        .unwrap_or(IrNode::Lit(LitClass::Zero));
                    stmts.push(IrNode::Assign {
                        dst,
                        val: Box::new(val),
                    });
                }
            }
            IrNode::Seq(stmts)
        }
        "inc_statement" => {
            let target = node.named_child(0).map(|n| node_text(n, src).to_string()).unwrap_or_default();
            IrNode::Assign {
                dst: target.clone(),
                val: Box::new(IrNode::BinOp {
                    op: OpClass::Add,
                    lhs: Box::new(IrNode::Var(target)),
                    rhs: Box::new(IrNode::Lit(LitClass::One)),
                }),
            }
        }
        "dec_statement" => {
            let target = node.named_child(0).map(|n| node_text(n, src).to_string()).unwrap_or_default();
            IrNode::Assign {
                dst: target.clone(),
                val: Box::new(IrNode::BinOp {
                    op: OpClass::Sub,
                    lhs: Box::new(IrNode::Var(target)),
                    rhs: Box::new(IrNode::Lit(LitClass::One)),
                }),
            }
        }
        "call_expression" => {
            let callee = node
                .child_by_field_name("function")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            let mut args = Vec::new();
            if let Some(arg_list) = node.child_by_field_name("arguments") {
                let mut c = arg_list.walk();
                for ch in arg_list.children(&mut c) {
                    if ch.is_named() {
                        args.push(lower_go_node(ch, src));
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
                .map(|n| lower_go_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("lhs".into()));
            let rhs = node
                .child_by_field_name("right")
                .map(|n| lower_go_node(n, src))
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
                .child_by_field_name("operand")
                .map(|n| lower_go_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("unop".into()));
            IrNode::UnOp {
                op,
                expr: Box::new(expr),
            }
        }
        "selector_expression" => {
            let obj = node
                .child_by_field_name("operand")
                .map(|n| lower_go_node(n, src))
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
                .child_by_field_name("operand")
                .map(|n| lower_go_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("arr".into()));
            let idx = node
                .child_by_field_name("index")
                .map(|n| lower_go_node(n, src))
                .unwrap_or_else(|| IrNode::Opaque("idx".into()));
            IrNode::Index {
                obj: Box::new(obj),
                idx: Box::new(idx),
            }
        }
        "expression_list" => {
            if node.named_child_count() == 1 {
                lower_go_node(node.named_child(0).unwrap(), src)
            } else {
                let mut items = Vec::new();
                let mut c = node.walk();
                for ch in node.children(&mut c) {
                    if ch.is_named() {
                        items.push(lower_go_node(ch, src));
                    }
                }
                IrNode::Seq(items)
            }
        }
        "parenthesized_expression" => {
            if let Some(inner) = node.named_child(0) {
                lower_go_node(inner, src)
            } else {
                IrNode::Opaque("paren".into())
            }
        }
        "identifier" | "field_identifier" => IrNode::Var(node_text(node, src).to_string()),
        "int_literal"
        | "float_literal"
        | "interpreted_string_literal"
        | "raw_string_literal"
        | "rune_literal"
        | "true"
        | "false"
        | "nil" => literal_node(node_text(node, src)),
        _ => IrNode::Opaque(node.kind().to_string()),
    }
}

fn lower_go_case_body(case_node: Node<'_>, src: &str) -> IrNode {
    let mut stmts = Vec::new();
    let mut c = case_node.walk();
    for ch in case_node.children(&mut c) {
        if !ch.is_named() || ch.kind() == "comment" {
            continue;
        }
        if case_node.child_by_field_name("value").map(|v| v.id()) == Some(ch.id()) {
            continue;
        }
        stmts.push(lower_go_node(ch, src));
    }
    IrNode::Seq(stmts)
}

fn lower_go_for(node: Node<'_>, src: &str) -> IrNode {
    let body = node
        .child_by_field_name("body")
        .map(|n| lower_go_node(n, src))
        .unwrap_or_else(|| IrNode::Seq(vec![]));

    let mut c = node.walk();
    for ch in node.children(&mut c) {
        match ch.kind() {
            "for_clause" => {
                // Detect canonical counting loop: for i := 0; i < n; i++
                let init = ch.child_by_field_name("initializer");
                let cond = ch.child_by_field_name("condition");
                let update = ch.child_by_field_name("update");
                if let (Some(init), Some(cond), Some(update)) = (init, cond, update) {
                    if let Some((var, start, end, step)) = extract_go_counting_loop(init, cond, update, src) {
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
                let cond_ir = cond
                    .map(|n| lower_go_node(n, src))
                    .unwrap_or(IrNode::Lit(LitClass::Bool(true)));
                return IrNode::Loop {
                    kind: LoopKind::While {
                        cond: Box::new(cond_ir),
                    },
                    var: None,
                    body: Box::new(body),
                };
            }
            "range_clause" => {
                let left = ch.child_by_field_name("left");
                let right = ch
                    .child_by_field_name("right")
                    .map(|n| lower_go_node(n, src))
                    .unwrap_or_else(|| IrNode::Opaque("range".into()));
                let vars: Vec<String> = left
                    .map(|l| {
                        node_text(l, src)
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                if vars.len() == 1 {
                    // Go 1.22+ `for i := range n` (or single-index range) -> IterRange(0, n, 1)
                    return IrNode::Loop {
                        kind: LoopKind::IterRange {
                            start: Box::new(IrNode::Lit(LitClass::Zero)),
                            end: Box::new(right),
                            step: Box::new(IrNode::Lit(LitClass::One)),
                        },
                        var: Some(vars[0].clone()),
                        body: Box::new(body),
                    };
                } else {
                    let v = vars.last().cloned();
                    return IrNode::Loop {
                        kind: LoopKind::IterSeq {
                            seq: Box::new(right),
                        },
                        var: v,
                        body: Box::new(body),
                    };
                }
            }
            _ => {}
        }
    }

    // Check condition-only for loop (`for cond { ... }`)
    if let Some(first) = node.named_child(0) {
        if first.kind() != "block" {
            return IrNode::Loop {
                kind: LoopKind::While {
                    cond: Box::new(lower_go_node(first, src)),
                },
                var: None,
                body: Box::new(body),
            };
        }
    }

    IrNode::Loop {
        kind: LoopKind::Infinite,
        var: None,
        body: Box::new(body),
    }
}

fn extract_go_counting_loop(
    init: Node<'_>,
    cond: Node<'_>,
    update: Node<'_>,
    src: &str,
) -> Option<(String, IrNode, IrNode, IrNode)> {
    // init: i := 0 or i = 0
    if init.kind() != "short_var_declaration" && init.kind() != "assignment_statement" {
        return None;
    }
    let var = node_text(init.child_by_field_name("left")?, src).trim().to_string();
    let start = lower_go_node(init.child_by_field_name("right")?, src);
    // cond: i < n
    if cond.kind() != "binary_expression" {
        return None;
    }
    let op = node_text(cond.child_by_field_name("operator")?, src).trim();
    if op != "<" {
        return None;
    }
    let lhs = node_text(cond.child_by_field_name("left")?, src).trim();
    if lhs != var {
        return None;
    }
    let end = lower_go_node(cond.child_by_field_name("right")?, src);
    // update: i++ or i += 1
    let step = if update.kind() == "inc_statement" {
        IrNode::Lit(LitClass::One)
    } else if update.kind() == "assignment_statement" && node_text(update, src).contains("+=") {
        lower_go_node(update.child_by_field_name("right")?, src)
    } else {
        return None;
    };
    Some((var, start, end, step))
}
