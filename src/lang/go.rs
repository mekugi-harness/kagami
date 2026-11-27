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
    parser.set_language(&tree_sitter_go::LANGUAGE.into())?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("failed to parse Go file {}", rel_path))?;

    let is_test_file = rel_path.ends_with("_test.go")
        || rel_path.starts_with("testsupport/")
        || rel_path.contains("/testsupport/");
    let dir_prefix = rel_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut pkg = dir_prefix.to_string();
    let mut imports = Vec::new();
    let mut symbols = Vec::new();
    let mut refs = Vec::new();
    let mut bridges = Vec::new();
    let mut ir_funcs = Vec::new();
    let mut interface_methods: HashMap<String, Vec<String>> = HashMap::new();
    let mut embedded_types: HashMap<String, Vec<String>> = HashMap::new();

    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "package_clause" => {
                if let Some(id) = find_child_by_kind(child, "package_identifier") {
                    let pname = node_text(id, src);
                    pkg = if dir_prefix.is_empty() {
                        pname.to_string()
                    } else {
                        format!("{}::{}", dir_prefix, pname)
                    };
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
                        let params = extract_go_params(child, src);
                        let locals = collect_go_locals(body, src, &params);
                        collect_signature_type_refs(child, src, &id, &mut refs);
                        collect_refs_and_bridges(body, src, &id, &locals, &mut refs, &mut bridges);
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
                        let params = extract_go_params(child, src);
                        let mut seed_locals = params.clone();
                        if let Some(recv_name) = extract_receiver_var_name(child, src) {
                            seed_locals.push(recv_name);
                        }
                        let locals = collect_go_locals(body, src, &seed_locals);
                        collect_signature_type_refs(child, src, &id, &mut refs);
                        collect_refs_and_bridges(body, src, &id, &locals, &mut refs, &mut bridges);
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
                            if let Some(type_node) = spec.child_by_field_name("type") {
                                match type_node.kind() {
                                    "interface_type" => {
                                        let (methods, embeds) =
                                            extract_interface_members(type_node, src);
                                        if !methods.is_empty() {
                                            interface_methods.insert(id.clone(), methods);
                                        }
                                        if !embeds.is_empty() {
                                            embedded_types.insert(id.clone(), embeds);
                                        }
                                    }
                                    "struct_type" => {
                                        let embeds = extract_struct_embeds(type_node, src);
                                        if !embeds.is_empty() {
                                            embedded_types.insert(id.clone(), embeds);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            let empty_locals = HashMap::new();
                            collect_refs_and_bridges(spec, src, &id, &empty_locals, &mut refs, &mut bridges);
                        }
                    }
                }
            }
            "const_declaration" | "var_declaration" => {
                let mut specs = Vec::new();
                let mut vc = child.walk();
                for ch in child.children(&mut vc) {
                    match ch.kind() {
                        "const_spec" | "var_spec" => specs.push(ch),
                        "var_spec_list" => {
                            let mut lc = ch.walk();
                            for sub in ch.children(&mut lc) {
                                if sub.kind() == "var_spec" {
                                    specs.push(sub);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                for spec in specs {
                    if let Some(name_node) = spec.child_by_field_name("name") {
                        let name = node_text(name_node, src).to_string();
                        let span = node_span(spec, 0);
                        let id = make_symbol_id(rel_path, &name, span.start_line);
                        let is_pkg_init_var = spec.kind() == "var_spec"
                            && spec
                                .child_by_field_name("value")
                                .map(contains_call_outside_func_literal)
                                .unwrap_or(false);
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
                            is_entrypoint: is_pkg_init_var,
                            is_dynamic_candidate: false,
                            package_or_module: pkg.clone(),
                        });
                        let empty_locals = HashMap::new();
                        collect_refs_and_bridges(
                            spec,
                            src,
                            &id,
                            &empty_locals,
                            &mut refs,
                            &mut bridges,
                        );
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
        interface_methods,
        embedded_types,
    })
}

fn contains_call_outside_func_literal(node: Node<'_>) -> bool {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "call_expression" => return true,
            "func_literal" => continue,
            _ => {
                let mut c = n.walk();
                for ch in n.children(&mut c) {
                    stack.push(ch);
                }
            }
        }
    }
    false
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
    let mut c = recv.walk();
    for ch in recv.children(&mut c) {
        if ch.kind() == "parameter_declaration"
            && let Some(mut ty) = ch.child_by_field_name("type")
        {
            loop {
                match ty.kind() {
                    "pointer_type" => {
                        let Some(inner) = ty.named_child(0) else {
                            break;
                        };
                        ty = inner;
                    }
                    "generic_type" => {
                        let Some(base) = ty.child_by_field_name("type") else {
                            break;
                        };
                        ty = base;
                    }
                    "type_identifier" => return node_text(ty, src).to_string(),
                    _ => break,
                }
            }
        }
    }
    String::new()
}

fn extract_receiver_var_name(node: Node<'_>, src: &str) -> Option<String> {
    let recv = node.child_by_field_name("receiver")?;
    let mut c = recv.walk();
    for ch in recv.children(&mut c) {
        if ch.kind() == "parameter_declaration"
            && let Some(id) = find_child_by_kind(ch, "identifier")
        {
            let name = node_text(id, src).trim();
            if !name.is_empty() && name != "_" {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn extract_embedded_type_name(node: Node<'_>, src: &str) -> Option<String> {
    let mut cur = node;
    loop {
        match cur.kind() {
            "pointer_type" => {
                cur = cur.named_child(0)?;
            }
            "generic_type" => {
                cur = cur.child_by_field_name("type")?;
            }
            "type_identifier" => {
                let t = node_text(cur, src).trim();
                return (!t.is_empty()).then(|| t.to_string());
            }
            "qualified_type" => {
                let pkg = cur
                    .child_by_field_name("package")
                    .map(|p| node_text(p, src).trim())?;
                let name = cur
                    .child_by_field_name("name")
                    .map(|n| node_text(n, src).trim())?;
                if !pkg.is_empty() && !name.is_empty() {
                    return Some(format!("{}.{}", pkg, name));
                }
                return None;
            }
            _ => return None,
        }
    }
}

fn extract_interface_members(node: Node<'_>, src: &str) -> (Vec<String>, Vec<String>) {
    let mut methods = Vec::new();
    let mut embeds = Vec::new();
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        match ch.kind() {
            "method_elem" => {
                if let Some(name) = ch.child_by_field_name("name") {
                    let t = node_text(name, src).trim();
                    if !t.is_empty() {
                        methods.push(t.to_string());
                    }
                }
            }
            "type_elem" => {
                let mut tc = ch.walk();
                for sub in ch.children(&mut tc) {
                    if let Some(embed) = extract_embedded_type_name(sub, src) {
                        embeds.push(embed);
                    }
                }
            }
            _ => {}
        }
    }
    (methods, embeds)
}

fn extract_struct_embeds(node: Node<'_>, src: &str) -> Vec<String> {
    let mut embeds = Vec::new();
    let Some(fields) = find_child_by_kind(node, "field_declaration_list") else {
        return embeds;
    };
    let mut c = fields.walk();
    for ch in fields.children(&mut c) {
        if ch.kind() == "field_declaration"
            && ch.child_by_field_name("name").is_none()
            && let Some(ty) = ch.child_by_field_name("type")
            && let Some(embed) = extract_embedded_type_name(ty, src)
        {
            embeds.push(embed);
        }
    }
    embeds
}

fn collect_signature_type_refs(node: Node<'_>, src: &str, from_id: &str, refs: &mut Vec<RawRef>) {
    let mut stack = Vec::new();
    if let Some(p) = node.child_by_field_name("parameters") {
        stack.push(p);
    }
    if let Some(r) = node.child_by_field_name("result") {
        stack.push(r);
    }
    while let Some(n) = stack.pop() {
        match n.kind() {
            "qualified_type" => {
                emit_qualified_type_ref(n, src, from_id, refs);
                continue;
            }
            "type_identifier" => {
                emit_type_identifier_ref(n, src, from_id, refs);
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

fn emit_qualified_type_ref(node: Node<'_>, src: &str, from_id: &str, refs: &mut Vec<RawRef>) {
    let pkg = node
        .child_by_field_name("package")
        .map(|p| node_text(p, src).trim().to_string());
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, src).trim().to_string());
    if let Some(target_name) = name.filter(|s| !s.is_empty()) {
        refs.push(RawRef {
            from_symbol_id: from_id.to_string(),
            target_name,
            qualifier: pkg.filter(|s| !s.is_empty()),
            kind: EdgeKind::TypeRef,
            line: node.start_position().row + 1,
        });
    }
}

fn emit_type_identifier_ref(node: Node<'_>, src: &str, from_id: &str, refs: &mut Vec<RawRef>) {
    let target = node_text(node, src);
    if !matches!(
        target,
        "string"
            | "int"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uintptr"
            | "float32"
            | "float64"
            | "complex64"
            | "complex128"
            | "bool"
            | "byte"
            | "rune"
            | "error"
            | "any"
            | "comparable"
    ) {
        refs.push(RawRef {
            from_symbol_id: from_id.to_string(),
            target_name: target.to_string(),
            qualifier: None,
            kind: EdgeKind::TypeRef,
            line: node.start_position().row + 1,
        });
    }
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

fn enclosing_scope_end(node: Node<'_>, fallback_end: usize) -> usize {
    let mut cur = node.parent();
    while let Some(p) = cur {
        if matches!(
            p.kind(),
            "block"
                | "for_statement"
                | "if_statement"
                | "expression_switch_statement"
                | "type_switch_statement"
                | "select_statement"
                | "communication_case"
                | "expression_case"
                | "default_case"
                | "type_case"
        ) {
            return p.end_byte();
        }
        cur = p.parent();
    }
    fallback_end
}

fn collect_go_locals(
    body: Node<'_>,
    src: &str,
    params: &[String],
) -> HashMap<String, Vec<(usize, usize)>> {
    let body_end = body.end_byte();
    let mut locals: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    for p in params {
        locals.entry(p.clone()).or_default().push((0, body_end));
    }
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "short_var_declaration" => {
                let start = n.end_byte();
                let end = enclosing_scope_end(n, body_end);
                if let Some(lhs) = n.child_by_field_name("left") {
                    for part in node_text(lhs, src).split(',') {
                        let t = part.trim();
                        if !t.is_empty() && t != "_" {
                            locals.entry(t.to_string()).or_default().push((start, end));
                        }
                    }
                }
            }
            "range_clause" => {
                let txt = node_text(n, src);
                if txt.contains(":=")
                    && let Some(lhs) = n.child_by_field_name("left")
                {
                    let start = n.end_byte();
                    let end = enclosing_scope_end(n, body_end);
                    for part in node_text(lhs, src).split(',') {
                        let t = part.trim();
                        if !t.is_empty() && t != "_" {
                            locals.entry(t.to_string()).or_default().push((start, end));
                        }
                    }
                }
            }
            "var_spec" => {
                if let Some(name) = n.child_by_field_name("name") {
                    let t = node_text(name, src).trim().to_string();
                    if !t.is_empty() && t != "_" {
                        let start = n.end_byte();
                        let end = enclosing_scope_end(n, body_end);
                        locals.entry(t).or_default().push((start, end));
                    }
                }
            }
            "func_literal" => {
                let start = n.start_byte();
                let end = n.end_byte();
                for p in extract_go_params(n, src) {
                    locals.entry(p).or_default().push((start, end));
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

fn emit_value_ref(
    mut expr: Node<'_>,
    src: &str,
    from_id: &str,
    locals: &HashMap<String, Vec<(usize, usize)>>,
    refs: &mut Vec<RawRef>,
) {
    while expr.kind() == "parenthesized_expression" {
        let Some(inner) = expr.named_child(0) else {
            break;
        };
        expr = inner;
    }
    let line = expr.start_position().row + 1;
    match expr.kind() {
        "identifier" => {
            let target = node_text(expr, src);
            let pos = expr.start_byte();
            let shadowed = locals
                .get(target)
                .map(|ranges| ranges.iter().any(|&(s, e)| pos >= s && pos < e))
                .unwrap_or(false);
            if !matches!(target, "true" | "false" | "nil" | "iota" | "_") && !shadowed {
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
            let operand = expr
                .child_by_field_name("operand")
                .map(|o| node_text(o, src).to_string());
            let field = expr
                .child_by_field_name("field")
                .map(|f| node_text(f, src).to_string());
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

fn emit_expr_list_refs(
    node: Node<'_>,
    src: &str,
    from_id: &str,
    locals: &HashMap<String, Vec<(usize, usize)>>,
    refs: &mut Vec<RawRef>,
) {
    if node.kind() == "expression_list" {
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            if ch.is_named() {
                emit_value_ref(ch, src, from_id, locals, refs);
            }
        }
    } else {
        emit_value_ref(node, src, from_id, locals, refs);
    }
}

fn collect_refs_and_bridges(
    node: Node<'_>,
    src: &str,
    from_id: &str,
    locals: &HashMap<String, Vec<(usize, usize)>>,
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
            "assignment_statement" | "short_var_declaration" => {
                if let Some(rhs) = n.child_by_field_name("right") {
                    emit_expr_list_refs(rhs, src, from_id, locals, refs);
                }
            }
            "var_spec" => {
                if let Some(val) = n.child_by_field_name("value") {
                    emit_expr_list_refs(val, src, from_id, locals, refs);
                }
            }
            "return_statement" => {
                let mut rc = n.walk();
                for ch in n.children(&mut rc) {
                    if ch.is_named() {
                        emit_expr_list_refs(ch, src, from_id, locals, refs);
                    }
                }
            }
            "send_statement" => {
                if let Some(val) = n.child_by_field_name("value") {
                    emit_value_ref(val, src, from_id, locals, refs);
                }
            }
            "keyed_element" => {
                if let Some(val_elem) = n.child_by_field_name("value") {
                    let val = if val_elem.kind() == "literal_element" {
                        val_elem.named_child(0).unwrap_or(val_elem)
                    } else {
                        val_elem
                    };
                    emit_value_ref(val, src, from_id, locals, refs);
                }
            }
            "literal_value" => {
                let mut lc = n.walk();
                for ch in n.children(&mut lc) {
                    if ch.kind() == "literal_element"
                        && let Some(inner) = ch.named_child(0)
                    {
                        emit_value_ref(inner, src, from_id, locals, refs);
                    }
                }
            }
            "argument_list" => {
                let mut ac = n.walk();
                for ch in n.children(&mut ac) {
                    if ch.is_named() {
                        emit_value_ref(ch, src, from_id, locals, refs);
                    }
                }
            }
            "qualified_type" => {
                emit_qualified_type_ref(n, src, from_id, refs);
                continue;
            }
            "type_identifier" => {
                emit_type_identifier_ref(n, src, from_id, refs);
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
            let op_txt = node
                .child_by_field_name("operator")
                .map(|o| node_text(o, src).trim())
                .unwrap_or("=");
            let val = if let Some(base_op) = op_txt.strip_suffix('=').filter(|s| !s.is_empty() && *s != ":") {
                IrNode::BinOp {
                    op: map_binop(base_op),
                    lhs: Box::new(IrNode::Var(lhs.clone())),
                    rhs: Box::new(rhs),
                }
            } else {
                rhs
            };
            IrNode::Assign {
                dst: lhs,
                val: Box::new(val),
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
