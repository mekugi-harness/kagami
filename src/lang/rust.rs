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
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

pub fn extract(rel_path: &str, src: &str, blake3_hex: String) -> Result<FileExtract> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("failed to parse Rust file {}", rel_path))?;

    let is_test_file = rel_path.contains("/tests/") || rel_path.ends_with("_test.rs") || rel_path.ends_with("tests.rs");
    let pkg = rel_path
        .split('/')
        .take(2)
        .collect::<Vec<_>>()
        .join("/");

    let mut imports = Vec::new();
    let mut symbols = Vec::new();
    let mut refs = Vec::new();
    let mut bridges = Vec::new();
    let mut ir_funcs = Vec::new();

    walk_rust_items(
        tree.root_node(),
        src,
        rel_path,
        &pkg,
        None,
        is_test_file,
        false,
        &mut imports,
        &mut symbols,
        &mut refs,
        &mut bridges,
        &mut ir_funcs,
    );

    Ok(FileExtract {
        file: rel_path.to_string(),
        lang: Language::Rust,
        blake3: blake3_hex,
        package_or_module: pkg,
        imports,
        symbols,
        refs,
        bridges,
        ir_funcs,
        interface_methods: std::collections::HashMap::new(),
        embedded_types: std::collections::HashMap::new(),
        struct_fields: std::collections::HashMap::new(),
        func_return_types: std::collections::HashMap::new(),
    })
}

#[allow(clippy::too_many_arguments)]
fn walk_rust_items(
    parent: Node<'_>,
    src: &str,
    rel_path: &str,
    pkg: &str,
    impl_type: Option<&str>,
    in_test_mod: bool,
    in_trait_impl: bool,
    imports: &mut Vec<String>,
    symbols: &mut Vec<Symbol>,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
    ir_funcs: &mut Vec<IrFunc>,
) {
    let mut cursor = parent.walk();
    let mut pending_attrs: Vec<String> = Vec::new();

    for child in parent.children(&mut cursor) {
        if child.kind() == "attribute_item" {
            pending_attrs.push(node_text(child, src).to_string());
            continue;
        }
        let attrs = std::mem::take(&mut pending_attrs);
        let has_test_attr = attrs.iter().any(|a| a.contains("test]") || a.contains("cfg(test)"));
        let has_export_attr = attrs
            .iter()
            .any(|a| a.contains("no_mangle") || a.contains("export_name") || a.contains("proc_macro"));
        let has_serde_attr = attrs.iter().any(|a| a.contains("serde") || a.contains("Serialize") || a.contains("Deserialize"));

        match child.kind() {
            "use_declaration" => {
                imports.push(node_text(child, src).to_string());
            }
            "mod_item" => {
                let sub_test = in_test_mod || has_test_attr;
                if let Some(body) = child.child_by_field_name("body") {
                    walk_rust_items(
                        body,
                        src,
                        rel_path,
                        pkg,
                        impl_type,
                        sub_test,
                        in_trait_impl,
                        imports,
                        symbols,
                        refs,
                        bridges,
                        ir_funcs,
                    );
                }
            }
            "impl_item" => {
                let ty = child
                    .child_by_field_name("type")
                    .map(|t| node_text(t, src).split('<').next().unwrap_or("").trim().to_string());
                let is_trait_impl = child.child_by_field_name("trait").is_some();
                if let Some(body) = child.child_by_field_name("body") {
                    walk_rust_items(
                        body,
                        src,
                        rel_path,
                        pkg,
                        ty.as_deref(),
                        in_test_mod || has_test_attr,
                        in_trait_impl || is_trait_impl,
                        imports,
                        symbols,
                        refs,
                        bridges,
                        ir_funcs,
                    );
                }
            }
            "function_item" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, src).to_string();
                    let qname = match impl_type {
                        Some(t) if !t.is_empty() => format!("{}::{}", t, name),
                        _ => name.clone(),
                    };
                    let span = node_span(child, 0);
                    let id = make_symbol_id(rel_path, &qname, span.start_line);
                    let vis = rust_visibility(child, src);
                    let is_test = in_test_mod || has_test_attr;
                    let is_entrypoint = name == "main" || has_export_attr;
                    symbols.push(Symbol {
                        id: id.clone(),
                        name: name.clone(),
                        qualified_name: qname.clone(),
                        kind: if impl_type.is_some() {
                            SymbolKind::Method
                        } else {
                            SymbolKind::Function
                        },
                        lang: Language::Rust,
                        file: rel_path.to_string(),
                        span,
                        visibility: vis,
                        is_test,
                        is_entrypoint,
                        is_dynamic_candidate: in_trait_impl || (impl_type.is_some() && vis == Visibility::Public),
                        package_or_module: pkg.to_string(),
                    });
                    if let Some(body) = child.child_by_field_name("body") {
                        let params = extract_rust_params(child, src);
                        let locals = collect_rust_locals(body, src, &params);
                        collect_rust_refs_and_bridges(body, src, &id, &locals, refs, bridges);
                        let ir_body = lower_rust_node(body, src);
                        ir_funcs.push(IrFunc {
                            symbol_id: id,
                            name: qname,
                            file: rel_path.to_string(),
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
                    let name = node_text(name_node, src).to_string();
                    let span = node_span(child, 0);
                    let id = make_symbol_id(rel_path, &name, span.start_line);
                    let kind = match child.kind() {
                        "struct_item" => SymbolKind::Struct,
                        "enum_item" => SymbolKind::Enum,
                        "trait_item" => SymbolKind::Trait,
                        _ => SymbolKind::Type,
                    };
                    symbols.push(Symbol {
                        id: id.clone(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind,
                        lang: Language::Rust,
                        file: rel_path.to_string(),
                        span,
                        visibility: rust_visibility(child, src),
                        is_test: in_test_mod,
                        is_entrypoint: false,
                        is_dynamic_candidate: kind == SymbolKind::Trait || has_serde_attr,
                        package_or_module: pkg.to_string(),
                    });
                    if has_serde_attr || rel_path.contains("protocol") {
                        bridges.push(BridgeHint {
                            from_symbol_id: id.clone(),
                            bridge_kind: BridgeKind::WireContract,
                            key: format!("type:{}", name),
                            role: BridgeRole::Both,
                            line: span.start_line,
                            confidence: Confidence::Heuristic,
                        });
                    }
                    let empty_locals = HashMap::new();
                    collect_rust_refs_and_bridges(child, src, &id, &empty_locals, refs, bridges);
                }
            }
            _ => {}
        }
    }
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

fn extract_rust_params(node: Node<'_>, src: &str) -> Vec<String> {
    let mut params = Vec::new();
    if let Some(plist) = node.child_by_field_name("parameters") {
        let mut c = plist.walk();
        for ch in plist.children(&mut c) {
            if ch.kind() == "parameter" {
                if let Some(pat) = ch.child_by_field_name("pattern") {
                    params.push(node_text(pat, src).trim().trim_start_matches("mut ").to_string());
                }
            } else if ch.kind() == "self_parameter" {
                params.push("self".to_string());
            }
        }
    }
    params
}

fn collect_rust_locals(body: Node<'_>, src: &str, params: &[String]) -> HashMap<String, usize> {
    let mut locals: HashMap<String, usize> = params.iter().map(|p| (p.clone(), 0usize)).collect();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        if n.kind() == "let_declaration" {
            let pos = n.start_byte();
            if let Some(pat) = n.child_by_field_name("pattern") {
                let name = node_text(pat, src).trim().trim_start_matches("mut ").trim();
                if !name.is_empty() && name != "_" {
                    locals
                        .entry(name.to_string())
                        .and_modify(|e| *e = (*e).min(pos))
                        .or_insert(pos);
                }
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    locals
}

fn collect_rust_refs_and_bridges(
    node: Node<'_>,
    src: &str,
    from_id: &str,
    locals: &HashMap<String, usize>,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let line = n.start_position().row + 1;
        match n.kind() {
            "call_expression" => {
                if let Some(func) = n.child_by_field_name("function") {
                    let ftxt = node_text(func, src);
                    if ftxt.ends_with("env::var") || ftxt.ends_with("env::var_os") {
                        if let Some(arg) = first_rust_str_arg(n, src) {
                            bridges.push(BridgeHint {
                                from_symbol_id: from_id.to_string(),
                                bridge_kind: BridgeKind::EnvKey,
                                key: arg,
                                role: BridgeRole::Consumer,
                                line,
                                confidence: Confidence::Resolved,
                            });
                        }
                    } else if ftxt.ends_with("Command::new") {
                        if let Some(arg) = first_rust_str_arg(n, src) {
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
                    match func.kind() {
                        "identifier" => {
                            refs.push(RawRef {
                                from_symbol_id: from_id.to_string(),
                                target_name: ftxt.to_string(),
                                qualifier: None,
                                kind: EdgeKind::Calls,
                                line,
                                receiver_chain: None,
                            });
                        }
                        "field_expression" => {
                            if let Some(field) = func.child_by_field_name("field") {
                                let qual = func.child_by_field_name("value").map(|v| node_text(v, src).to_string());
                                refs.push(RawRef {
                                    from_symbol_id: from_id.to_string(),
                                    target_name: node_text(field, src).to_string(),
                                    qualifier: qual,
                                    kind: EdgeKind::Calls,
                                    line,
                                    receiver_chain: None,
                                });
                            }
                        }
                        "scoped_identifier" => {
                            let parts: Vec<&str> = ftxt.split("::").collect();
                            if let Some(last) = parts.last() {
                                let qual = if parts.len() >= 2 {
                                    Some(parts[parts.len() - 2].to_string())
                                } else {
                                    None
                                };
                                refs.push(RawRef {
                                    from_symbol_id: from_id.to_string(),
                                    target_name: (*last).to_string(),
                                    qualifier: qual,
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
            "string_literal" | "raw_string_literal" => {
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
            "field_initializer" => {
                if let Some(val) = n.child_by_field_name("value") {
                    match val.kind() {
                        "identifier" => {
                            let target = node_text(val, src);
                            let shadowed = locals.get(target).map(|&p| p < val.start_byte()).unwrap_or(false);
                            if !shadowed {
                                refs.push(RawRef {
                                    from_symbol_id: from_id.to_string(),
                                    target_name: target.to_string(),
                                    qualifier: None,
                                    kind: EdgeKind::Calls,
                                    line,
                                    receiver_chain: None,
                                });
                            }
                        }
                        "scoped_identifier" => {
                            let ftxt = node_text(val, src);
                            let parts: Vec<&str> = ftxt.split("::").collect();
                            if let Some(last) = parts.last() {
                                let qual = if parts.len() >= 2 {
                                    Some(parts[parts.len() - 2].to_string())
                                } else {
                                    None
                                };
                                refs.push(RawRef {
                                    from_symbol_id: from_id.to_string(),
                                    target_name: (*last).to_string(),
                                    qualifier: qual,
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
            "type_identifier" => {
                let t = node_text(n, src);
                if !matches!(
                    t,
                    "String" | "Vec" | "Option" | "Result" | "Box" | "Arc" | "Mutex" | "HashMap" | "HashSet" | "Self" | "usize" | "isize" | "u8" | "u16" | "u32" | "u64" | "i8" | "i16" | "i32" | "i64" | "f32" | "f64" | "bool" | "str"
                ) {
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
                .map(|p| node_text(p, src).trim().trim_start_matches("mut ").to_string())
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
                        let pat = ch.child_by_field_name("pattern").map(|p| node_text(p, src).trim().to_string()).unwrap_or_default();
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
            let val = node.named_child(0).map(|n| Box::new(lower_rust_node(n, src)));
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
        "integer_literal"
        | "float_literal"
        | "string_literal"
        | "raw_string_literal"
        | "char_literal"
        | "boolean_literal" => literal_node(node_text(node, src)),
        _ => IrNode::Opaque(node.kind().to_string()),
    }
}
