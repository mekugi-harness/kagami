use crate::ir::{IrFunc, IrNode, LitClass, LoopKind, OpClass};
use crate::lang::{
    literal_node, looks_like_wire_method, make_symbol_id, map_binop, map_unop, node_span,
    node_text, strip_quotes,
};
use crate::model::{
    BridgeHint, BridgeKind, BridgeRole, Confidence, EdgeKind, FileExtract, Language, RawRef,
    ReceiverStep, Symbol, SymbolKind, Visibility,
};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use tree_sitter::{Node, Parser};

const MAX_RECEIVER_CHAIN_LEN: usize = 12;

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
    let mut struct_fields: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut func_return_types: HashMap<String, Vec<String>> = HashMap::new();

    let root = tree.root_node();
    // Pre-pass: collect imports and top-level package var types so function/method bodies
    // can distinguish imported package aliases from local variables and resolve package vars.
    {
        let mut pre_cursor = root.walk();
        for child in root.children(&mut pre_cursor) {
            if child.kind() == "import_declaration" {
                collect_imports(child, src, &mut imports);
            }
        }
    }
    let pkg_aliases = build_pkg_alias_set(&imports);
    let mut pkg_var_types: HashMap<String, Vec<ReceiverStep>> = HashMap::new();
    {
        let empty_vars = HashMap::new();
        let empty_env = GoLocalEnv::new(&pkg_aliases, &empty_vars);
        let mut pre_cursor = root.walk();
        for child in root.children(&mut pre_cursor) {
            if child.kind() == "var_declaration" {
                for spec in collect_var_const_specs(child) {
                    bind_var_spec_to_map(spec, src, &empty_env, &mut pkg_var_types);
                }
            }
        }
    }

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
                // Already collected in pre-pass
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
                    let ret_types = extract_go_return_types(child, src);
                    if ret_types.iter().any(|t| !t.is_empty()) {
                        func_return_types.insert(id.clone(), ret_types);
                    }
                    let mut env = GoLocalEnv::new(&pkg_aliases, &pkg_var_types);
                    collect_signature_type_refs(child, src, &id, &env, &mut refs, &mut bridges);
                    if let Some(body) = child.child_by_field_name("body") {
                        let params = extract_go_params(child, src);
                        let mut seed_locals = params.clone();
                        seed_locals.extend(extract_go_named_results(child, src));
                        let locals = collect_go_locals(body, src, &seed_locals);
                        env.seed_typed_params(child, src, body.end_byte());
                        env.seed_typed_named_results(child, src, body.end_byte());
                        env.collect_body(body, src, body.end_byte());
                        collect_refs_and_bridges(
                            body,
                            src,
                            &id,
                            &locals,
                            &env,
                            &mut refs,
                            &mut bridges,
                        );
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
                    let ret_types = extract_go_return_types(child, src);
                    if ret_types.iter().any(|t| !t.is_empty()) {
                        func_return_types.insert(id.clone(), ret_types);
                    }
                    let mut env = GoLocalEnv::new(&pkg_aliases, &pkg_var_types);
                    collect_signature_type_refs(child, src, &id, &env, &mut refs, &mut bridges);
                    if let Some(body) = child.child_by_field_name("body") {
                        let params = extract_go_params(child, src);
                        let mut seed_locals = params.clone();
                        seed_locals.extend(extract_go_named_results(child, src));
                        if let Some(recv_name) = extract_receiver_var_name(child, src) {
                            if !recv.is_empty() {
                                env.bind(
                                    recv_name.clone(),
                                    0,
                                    body.end_byte(),
                                    vec![ReceiverStep::Type(recv.clone())],
                                );
                            }
                            seed_locals.push(recv_name);
                        }
                        let locals = collect_go_locals(body, src, &seed_locals);
                        env.seed_typed_params(child, src, body.end_byte());
                        env.seed_typed_named_results(child, src, body.end_byte());
                        env.collect_body(body, src, body.end_byte());
                        collect_refs_and_bridges(
                            body,
                            src,
                            &id,
                            &locals,
                            &env,
                            &mut refs,
                            &mut bridges,
                        );
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
                    if matches!(spec.kind(), "type_spec" | "type_alias") {
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
                                        let (fields, embeds) =
                                            extract_struct_fields_and_embeds(type_node, src);
                                        if !fields.is_empty() {
                                            struct_fields.insert(id.clone(), fields);
                                        }
                                        if !embeds.is_empty() {
                                            embedded_types.insert(id.clone(), embeds);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            let empty_locals = HashMap::new();
                            let empty_env = GoLocalEnv::new(&pkg_aliases, &pkg_var_types);
                            if let Some(tp) = spec.child_by_field_name("type_parameters") {
                                collect_refs_and_bridges(
                                    tp,
                                    src,
                                    &id,
                                    &empty_locals,
                                    &empty_env,
                                    &mut refs,
                                    &mut bridges,
                                );
                            }
                            if let Some(type_node) = spec.child_by_field_name("type") {
                                collect_refs_and_bridges(
                                    type_node,
                                    src,
                                    &id,
                                    &empty_locals,
                                    &empty_env,
                                    &mut refs,
                                    &mut bridges,
                                );
                            }
                        }
                    }
                }
            }
            "const_declaration" | "var_declaration" => {
                let specs = collect_var_const_specs(child);
                let mut inherited_const_type: Option<Node<'_>> = None;
                let mut inherited_const_value: Option<Node<'_>> = None;
                for spec in specs {
                    let is_var = spec.kind() == "var_spec";
                    if !is_var {
                        if let Some(t) = spec.child_by_field_name("type") {
                            inherited_const_type = Some(t);
                        } else if spec.child_by_field_name("value").is_some() {
                            inherited_const_type = None;
                        }
                        if let Some(v) = spec.child_by_field_name("value") {
                            inherited_const_value = Some(v);
                        }
                    }
                    let names = collect_spec_names(spec, src);
                    let is_pkg_init_var = is_var
                        && spec
                            .child_by_field_name("value")
                            .map(contains_call_outside_func_literal)
                            .unwrap_or(false);
                    for name in names {
                        let span = node_span(spec, 0);
                        let id = make_symbol_id(rel_path, &name, span.start_line);
                        symbols.push(Symbol {
                            id: id.clone(),
                            name: name.clone(),
                            qualified_name: name.clone(),
                            kind: if is_var {
                                SymbolKind::Var
                            } else {
                                SymbolKind::Const
                            },
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
                        let empty_env = GoLocalEnv::new(&pkg_aliases, &pkg_var_types);
                        if let Some(ty) = spec
                            .child_by_field_name("type")
                            .or(if !is_var { inherited_const_type } else { None })
                        {
                            collect_refs_and_bridges(
                                ty,
                                src,
                                &id,
                                &empty_locals,
                                &empty_env,
                                &mut refs,
                                &mut bridges,
                            );
                        }
                        if !is_var
                            && spec.child_by_field_name("value").is_none()
                            && let Some(val) = inherited_const_value
                        {
                            emit_expr_list_refs(
                                val,
                                src,
                                &id,
                                &empty_locals,
                                &empty_env,
                                &mut refs,
                            );
                            collect_refs_and_bridges(
                                val,
                                src,
                                &id,
                                &empty_locals,
                                &empty_env,
                                &mut refs,
                                &mut bridges,
                            );
                        }
                        collect_refs_and_bridges(
                            spec,
                            src,
                            &id,
                            &empty_locals,
                            &empty_env,
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
        struct_fields,
        func_return_types,
        trait_impls: Vec::new(),
        impl_macros: Vec::new(),
        cfg_gated_symbols: HashMap::new(),
        type_aliases: HashMap::new(),
        rust_uses: Vec::new(),
        symbol_mod_scopes: HashMap::new(),
        func_callback_params: HashMap::new(),
        ts_star_reexports: Vec::new(),
    })
}

fn collect_var_const_specs<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut specs = Vec::new();
    let mut vc = node.walk();
    for ch in node.children(&mut vc) {
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
    specs
}

fn build_pkg_alias_set(imports: &[String]) -> HashSet<String> {
    let mut aliases = HashSet::new();
    for imp in imports {
        if let Some((alias, _)) = imp.split_once('=') {
            aliases.insert(alias.to_string());
        } else if let Some(last) = imp.rsplit('/').next() {
            aliases.insert(last.to_string());
            if last.starts_with('v')
                && last[1..].chars().all(|c| c.is_ascii_digit())
                && let Some(prev) = imp.rsplit('/').nth(1)
            {
                aliases.insert(prev.to_string());
            }
        }
    }
    aliases
}

fn contains_call_outside_func_literal(node: Node<'_>) -> bool {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "call_expression" => {
                // Ignore compile-time pointer type conversions like `(*MyStruct)(nil)`
                // used in `var _ MyInterface = (*MyStruct)(nil)`, while still inspecting
                // their single argument for nested runtime calls (e.g. `(*Config)(load())`).
                let is_ptr_conversion = n
                    .child_by_field_name("function")
                    .filter(|f| f.kind() == "parenthesized_expression")
                    .and_then(|p| p.named_child(0))
                    .is_some_and(|inner| {
                        matches!(inner.kind(), "pointer_type" | "unary_expression")
                    })
                    && n.child_by_field_name("arguments")
                        .is_some_and(|args| args.named_child_count() == 1);
                if !is_ptr_conversion {
                    return true;
                }
                if let Some(args) = n.child_by_field_name("arguments") {
                    stack.push(args);
                }
            }
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

fn is_go_builtin_type(target: &str) -> bool {
    matches!(
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
    )
}

fn extract_go_type_name(node: Node<'_>, src: &str) -> Option<String> {
    let mut cur = node;
    loop {
        match cur.kind() {
            "pointer_type" => {
                cur = cur.named_child(0)?;
            }
            "generic_type" => {
                cur = cur.child_by_field_name("type")?;
            }
            "slice_type" | "array_type" => {
                let elem = cur.child_by_field_name("element")?;
                let inner = extract_go_type_name(elem, src)?;
                return Some(format!("[]{}", inner));
            }
            "map_type" => {
                let val = cur.child_by_field_name("value")?;
                let inner = extract_go_type_name(val, src)?;
                return Some(format!("[]{}", inner));
            }
            "type_identifier" => {
                let t = node_text(cur, src).trim();
                return (!t.is_empty() && !is_go_builtin_type(t)).then(|| t.to_string());
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

fn extract_struct_fields_and_embeds(
    node: Node<'_>,
    src: &str,
) -> (HashMap<String, String>, Vec<String>) {
    let mut field_map = HashMap::new();
    let mut embeds = Vec::new();
    let Some(fields) = find_child_by_kind(node, "field_declaration_list") else {
        return (field_map, embeds);
    };
    let mut c = fields.walk();
    for ch in fields.children(&mut c) {
        if ch.kind() != "field_declaration" {
            continue;
        }
        let Some(ty) = ch.child_by_field_name("type") else {
            continue;
        };
        let mut names = Vec::new();
        let mut fc = ch.walk();
        for sub in ch.children(&mut fc) {
            if sub.kind() == "field_identifier" {
                let t = node_text(sub, src).trim();
                if !t.is_empty() && t != "_" {
                    names.push(t.to_string());
                }
            }
        }
        if ch.child_by_field_name("name").is_none() {
            if let Some(embed) = extract_embedded_type_name(ty, src) {
                let field_name = embed.rsplit('.').next().unwrap_or(&embed).to_string();
                if !field_name.is_empty() {
                    field_map.insert(field_name, embed.clone());
                }
                embeds.push(embed);
            }
        } else {
            // Record field presence even when its type is unsupported (e.g. func/anon struct)
            // so shallower fields still shadow deeper promoted methods in Go's selector namespace.
            let field_ty = extract_go_type_name(ty, src).unwrap_or_default();
            for fname in names {
                field_map.insert(fname, field_ty.clone());
            }
        }
    }
    (field_map, embeds)
}

fn extract_go_return_types(node: Node<'_>, src: &str) -> Vec<String> {
    let Some(res) = node.child_by_field_name("result") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if res.kind() == "parameter_list" {
        let mut c = res.walk();
        for ch in res.children(&mut c) {
            if ch.kind() == "parameter_declaration" {
                let ty = ch
                    .child_by_field_name("type")
                    .and_then(|t| extract_go_type_name(t, src))
                    .unwrap_or_default();
                let mut name_count = 0usize;
                let mut pc = ch.walk();
                for sub in ch.children(&mut pc) {
                    if sub.kind() == "identifier" {
                        name_count += 1;
                    }
                }
                for _ in 0..name_count.max(1) {
                    out.push(ty.clone());
                }
            }
        }
    } else {
        out.push(extract_go_type_name(res, src).unwrap_or_default());
    }
    out
}

fn collect_signature_type_refs(
    node: Node<'_>,
    src: &str,
    from_id: &str,
    env: &GoLocalEnv<'_>,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let empty_locals = HashMap::new();
    for field in ["receiver", "type_parameters", "parameters", "result"] {
        if let Some(ch) = node.child_by_field_name(field) {
            collect_refs_and_bridges(ch, src, from_id, &empty_locals, env, refs, bridges);
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
            receiver_chain: None,
        });
    }
}

fn emit_type_identifier_ref(node: Node<'_>, src: &str, from_id: &str, refs: &mut Vec<RawRef>) {
    let target = node_text(node, src);
    if !is_go_builtin_type(target) {
        refs.push(RawRef {
            from_symbol_id: from_id.to_string(),
            target_name: target.to_string(),
            qualifier: None,
            kind: EdgeKind::TypeRef,
            line: node.start_position().row + 1,
            receiver_chain: None,
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

fn extract_go_named_results(node: Node<'_>, src: &str) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(res) = node.child_by_field_name("result")
        && res.kind() == "parameter_list"
    {
        let mut c = res.walk();
        for ch in res.children(&mut c) {
            if ch.kind() == "parameter_declaration" {
                let mut pc = ch.walk();
                for sub in ch.children(&mut pc) {
                    if sub.kind() == "identifier" {
                        let t = node_text(sub, src).trim();
                        if !t.is_empty() && t != "_" {
                            names.push(t.to_string());
                        }
                    }
                }
            }
        }
    }
    names
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

struct GoLocalEnv<'a> {
    pkg_aliases: &'a HashSet<String>,
    pkg_var_types: &'a HashMap<String, Vec<ReceiverStep>>,
    locals: HashMap<String, Vec<(usize, usize, Option<Vec<ReceiverStep>>)>>,
}

impl<'a> GoLocalEnv<'a> {
    fn new(
        pkg_aliases: &'a HashSet<String>,
        pkg_var_types: &'a HashMap<String, Vec<ReceiverStep>>,
    ) -> Self {
        Self {
            pkg_aliases,
            pkg_var_types,
            locals: HashMap::new(),
        }
    }

    fn bind(&mut self, name: String, start: usize, end: usize, chain: Vec<ReceiverStep>) {
        let opt_chain = (!chain.is_empty() && chain.len() <= MAX_RECEIVER_CHAIN_LEN).then_some(chain);
        self.bind_opt(name, start, end, opt_chain);
    }

    fn bind_opt(
        &mut self,
        name: String,
        start: usize,
        end: usize,
        chain: Option<Vec<ReceiverStep>>,
    ) {
        if !name.is_empty() && name != "_" {
            let valid = chain.filter(|c| !c.is_empty() && c.len() <= MAX_RECEIVER_CHAIN_LEN);
            self.locals
                .entry(name)
                .or_default()
                .push((start, end, valid));
        }
    }

    fn lookup(&self, name: &str, pos: usize) -> Option<&[ReceiverStep]> {
        if let Some(entries) = self.locals.get(name)
            && let Some((_, _, chain)) = entries
                .iter()
                .filter(|(s, e, _)| pos >= *s && pos < *e)
                .min_by_key(|(s, e, _)| (e.saturating_sub(*s), usize::MAX - *s))
        {
            return chain.as_deref();
        }
        self.pkg_var_types.get(name).map(|v| v.as_slice())
    }

    fn is_local_at(&self, name: &str, pos: usize) -> bool {
        self.locals
            .get(name)
            .map(|entries| entries.iter().any(|(s, e, _)| pos >= *s && pos < *e))
            .unwrap_or(false)
    }

    fn is_pkg_alias(&self, name: &str, pos: usize) -> bool {
        self.pkg_aliases.contains(name) && !self.is_local_at(name, pos)
    }

    fn seed_param_list(&mut self, param_list: Node<'_>, src: &str, start: usize, end: usize) {
        let mut c = param_list.walk();
        for ch in param_list.children(&mut c) {
            match ch.kind() {
                "parameter_declaration" => {
                    let chain = ch
                        .child_by_field_name("type")
                        .and_then(|t| extract_go_type_name(t, src))
                        .map(|ty| vec![ReceiverStep::Type(ty)]);
                    let mut pc = ch.walk();
                    for sub in ch.children(&mut pc) {
                        if sub.kind() == "identifier" {
                            let name = node_text(sub, src).trim().to_string();
                            self.bind_opt(name, start, end, chain.clone());
                        }
                    }
                }
                "variadic_parameter_declaration" => {
                    let chain = ch
                        .child_by_field_name("type")
                        .and_then(|t| extract_go_type_name(t, src))
                        .map(|ty| vec![ReceiverStep::Type(format!("[]{}", ty))]);
                    if let Some(name_node) = ch.child_by_field_name("name") {
                        let name = node_text(name_node, src).trim().to_string();
                        self.bind_opt(name, start, end, chain);
                    }
                }
                _ => {}
            }
        }
    }

    fn seed_typed_params(&mut self, func_node: Node<'_>, src: &str, end: usize) {
        if let Some(p) = func_node.child_by_field_name("parameters") {
            self.seed_param_list(p, src, 0, end);
        }
    }

    fn seed_typed_named_results(&mut self, func_node: Node<'_>, src: &str, end: usize) {
        if let Some(res) = func_node.child_by_field_name("result")
            && res.kind() == "parameter_list"
        {
            self.seed_param_list(res, src, 0, end);
        }
    }

    fn collect_body(&mut self, node: Node<'_>, src: &str, body_end: usize) {
        match node.kind() {
            "short_var_declaration" => {
                let start = node.end_byte();
                let end = enclosing_scope_end(node, body_end);
                if let (Some(lhs), Some(rhs)) = (
                    node.child_by_field_name("left"),
                    node.child_by_field_name("right"),
                ) {
                    let lhs_names = collect_lhs_identifiers(lhs, src);
                    let rhs_exprs = collect_expr_list(rhs);
                    self.bind_paired(&lhs_names, &rhs_exprs, src, start, end);
                }
            }
            "var_spec" => {
                let start = node.end_byte();
                let end = enclosing_scope_end(node, body_end);
                let names = collect_spec_names(node, src);
                if let Some(ty_node) = node.child_by_field_name("type") {
                    // Explicit type annotation present: never fall back to initializer's concrete type.
                    let chain = extract_go_type_name(ty_node, src)
                        .map(|ty| vec![ReceiverStep::Type(ty)]);
                    for name in names {
                        self.bind_opt(name, start, end, chain.clone());
                    }
                } else if let Some(val) = node.child_by_field_name("value") {
                    let rhs_exprs = collect_expr_list(val);
                    self.bind_paired(&names, &rhs_exprs, src, start, end);
                } else {
                    for name in names {
                        self.bind_opt(name, start, end, None);
                    }
                }
            }
            "range_clause" => {
                if node_text(node, src).contains(":=")
                    && let (Some(lhs), Some(rhs)) = (
                        node.child_by_field_name("left"),
                        node.child_by_field_name("right"),
                    )
                {
                    let lhs_names = collect_lhs_identifiers(lhs, src);
                    let start = node.end_byte();
                    let end = enclosing_scope_end(node, body_end);
                    if let Some(first) = lhs_names.first() {
                        self.bind_opt(first.clone(), start, end, None);
                    }
                    if let Some(second) = lhs_names.get(1) {
                        let elem_chain = resolve_expr_chain(rhs, 0, src, self).and_then(|mut c| {
                            (c.len() < MAX_RECEIVER_CHAIN_LEN).then(|| {
                                c.push(ReceiverStep::Elem);
                                c
                            })
                        });
                        self.bind_opt(second.clone(), start, end, elem_chain);
                    }
                }
            }
            "func_literal" => {
                if let Some(p) = node.child_by_field_name("parameters") {
                    self.seed_param_list(p, src, node.start_byte(), node.end_byte());
                }
            }
            _ => {}
        }
        // Pre-order traversal so earlier declarations are visible to later statements.
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            self.collect_body(ch, src, body_end);
        }
    }

    fn has_same_scope_binding(&self, name: &str, start: usize, end: usize) -> bool {
        self.locals
            .get(name)
            .map(|entries| entries.iter().any(|(s, e, _)| *e == end && *s <= start))
            .unwrap_or(false)
    }

    fn bind_paired(
        &mut self,
        lhs_names: &[String],
        rhs_exprs: &[Node<'_>],
        src: &str,
        start: usize,
        end: usize,
    ) {
        if rhs_exprs.len() == lhs_names.len() {
            for (name, &expr) in lhs_names.iter().zip(rhs_exprs.iter()) {
                let chain = resolve_expr_chain(expr, 0, src, self);
                if chain.is_some() || !self.has_same_scope_binding(name, start, end) {
                    self.bind_opt(name.clone(), start, end, chain);
                }
            }
        } else if rhs_exprs.len() == 1 {
            let expr = rhs_exprs[0];
            for (idx, name) in lhs_names.iter().enumerate() {
                let chain = resolve_expr_chain(expr, idx, src, self);
                if chain.is_some() || !self.has_same_scope_binding(name, start, end) {
                    self.bind_opt(name.clone(), start, end, chain);
                }
            }
        } else {
            for name in lhs_names {
                if !self.has_same_scope_binding(name, start, end) {
                    self.bind_opt(name.clone(), start, end, None);
                }
            }
        }
    }
}

fn collect_lhs_identifiers(node: Node<'_>, src: &str) -> Vec<String> {
    if node.kind() == "expression_list" {
        let mut names = Vec::new();
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            if ch.is_named() {
                names.push(if ch.kind() == "identifier" {
                    node_text(ch, src).trim().to_string()
                } else {
                    String::new()
                });
            }
        }
        names
    } else if node.kind() == "identifier" {
        vec![node_text(node, src).trim().to_string()]
    } else {
        Vec::new()
    }
}

fn collect_expr_list<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    if node.kind() == "expression_list" {
        let mut exprs = Vec::new();
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            if ch.is_named() {
                exprs.push(ch);
            }
        }
        exprs
    } else {
        vec![node]
    }
}

fn collect_spec_names(spec: Node<'_>, src: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut c = spec.walk();
    for ch in spec.children(&mut c) {
        if ch.kind() == "identifier"
            && spec.child_by_field_name("type").map(|t| t.id()) != Some(ch.id())
            && spec.child_by_field_name("value").map(|v| v.id()) != Some(ch.id())
        {
            let t = node_text(ch, src).trim();
            if !t.is_empty() {
                names.push(t.to_string());
            }
        }
    }
    names
}

fn bind_var_spec_to_map(
    spec: Node<'_>,
    src: &str,
    env: &GoLocalEnv<'_>,
    out: &mut HashMap<String, Vec<ReceiverStep>>,
) {
    let names = collect_spec_names(spec, src);
    if let Some(ty_node) = spec.child_by_field_name("type") {
        if let Some(ty) = extract_go_type_name(ty_node, src) {
            for name in names {
                if name != "_" {
                    out.insert(name, vec![ReceiverStep::Type(ty.clone())]);
                }
            }
        }
    } else if let Some(val) = spec.child_by_field_name("value") {
        let rhs_exprs = collect_expr_list(val);
        if rhs_exprs.len() == names.len() {
            for (name, &expr) in names.iter().zip(rhs_exprs.iter()) {
                if name != "_"
                    && let Some(chain) = resolve_expr_chain(expr, 0, src, env)
                {
                    out.insert(name.clone(), chain);
                }
            }
        } else if rhs_exprs.len() == 1 {
            for (idx, name) in names.iter().enumerate() {
                if name != "_"
                    && let Some(chain) = resolve_expr_chain(rhs_exprs[0], idx, src, env)
                {
                    out.insert(name.clone(), chain);
                }
            }
        }
    }
}

fn resolve_expr_chain(
    mut expr: Node<'_>,
    result_index: usize,
    src: &str,
    env: &GoLocalEnv<'_>,
) -> Option<Vec<ReceiverStep>> {
    loop {
        match expr.kind() {
            "parenthesized_expression" => {
                expr = expr.named_child(0)?;
            }
            "unary_expression" => {
                let op = expr
                    .child_by_field_name("operator")
                    .map(|o| node_text(o, src).trim());
                if matches!(op, Some("&" | "*")) {
                    expr = expr.child_by_field_name("operand")?;
                } else {
                    return None;
                }
            }
            _ => break,
        }
    }
    match expr.kind() {
        "identifier" => {
            if result_index != 0 {
                return None;
            }
            let name = node_text(expr, src).trim();
            if env.is_pkg_alias(name, expr.start_byte()) {
                return None;
            }
            env.lookup(name, expr.start_byte()).map(|s| s.to_vec())
        }
        "composite_literal" => {
            if result_index != 0 {
                return None;
            }
            let ty = expr
                .child_by_field_name("type")
                .and_then(|t| extract_go_type_name(t, src))?;
            Some(vec![ReceiverStep::Type(ty)])
        }
        "type_assertion_expression" => {
            if result_index != 0 {
                return None;
            }
            let ty = expr
                .child_by_field_name("type")
                .and_then(|t| extract_go_type_name(t, src))?;
            Some(vec![ReceiverStep::Type(ty)])
        }
        "selector_expression" => {
            if result_index != 0 {
                return None;
            }
            let operand = expr.child_by_field_name("operand")?;
            let field = expr
                .child_by_field_name("field")
                .map(|f| node_text(f, src).trim().to_string())?;
            if field.is_empty() {
                return None;
            }
            let mut chain = resolve_expr_chain(operand, 0, src, env)?;
            if chain.len() >= MAX_RECEIVER_CHAIN_LEN {
                return None;
            }
            chain.push(ReceiverStep::Field(field));
            Some(chain)
        }
        "index_expression" => {
            if result_index != 0 {
                return None;
            }
            let operand = expr.child_by_field_name("operand")?;
            let mut chain = resolve_expr_chain(operand, 0, src, env)?;
            if chain.len() >= MAX_RECEIVER_CHAIN_LEN {
                return None;
            }
            chain.push(ReceiverStep::Elem);
            Some(chain)
        }
        "call_expression" => {
            let func = expr.child_by_field_name("function")?;
            match func.kind() {
                "identifier" => {
                    let fname = node_text(func, src).trim();
                    if env.is_local_at(fname, func.start_byte()) {
                        return None;
                    }
                    if (fname == "new" || fname == "make") && result_index == 0 {
                        let args = expr.child_by_field_name("arguments")?;
                        let first_arg = args.named_child(0)?;
                        let ty = extract_go_type_name(first_arg, src)?;
                        return Some(vec![ReceiverStep::Type(ty)]);
                    }
                    if fname.is_empty() || is_go_builtin_type(fname) {
                        return None;
                    }
                    Some(vec![ReceiverStep::Call {
                        qualifier: None,
                        name: fname.to_string(),
                        result_index,
                    }])
                }
                "selector_expression" => {
                    let operand = func.child_by_field_name("operand")?;
                    let field = func
                        .child_by_field_name("field")
                        .map(|f| node_text(f, src).trim().to_string())?;
                    if field.is_empty() {
                        return None;
                    }
                    if operand.kind() == "identifier" {
                        let op_name = node_text(operand, src).trim();
                        if env.is_pkg_alias(op_name, operand.start_byte()) {
                            return Some(vec![ReceiverStep::Call {
                                qualifier: Some(op_name.to_string()),
                                name: field,
                                result_index,
                            }]);
                        }
                    }
                    let mut chain = resolve_expr_chain(operand, 0, src, env)?;
                    if chain.len() >= MAX_RECEIVER_CHAIN_LEN {
                        return None;
                    }
                    chain.push(ReceiverStep::MethodCall {
                        name: field,
                        result_index,
                    });
                    Some(chain)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn emit_value_ref(
    mut expr: Node<'_>,
    src: &str,
    from_id: &str,
    locals: &HashMap<String, Vec<(usize, usize)>>,
    env: &GoLocalEnv<'_>,
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
                    receiver_chain: None,
                });
            }
        }
        "selector_expression" => {
            let operand_node = expr.child_by_field_name("operand");
            let operand = operand_node.map(|o| node_text(o, src).to_string());
            let field = expr
                .child_by_field_name("field")
                .map(|f| node_text(f, src).to_string());
            if let Some(target) = field {
                let receiver_chain =
                    operand_node.and_then(|o| resolve_expr_chain(o, 0, src, env));
                refs.push(RawRef {
                    from_symbol_id: from_id.to_string(),
                    target_name: target,
                    qualifier: operand,
                    kind: EdgeKind::Calls,
                    line,
                    receiver_chain,
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
    env: &GoLocalEnv<'_>,
    refs: &mut Vec<RawRef>,
) {
    if node.kind() == "expression_list" {
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            if ch.is_named() {
                emit_value_ref(ch, src, from_id, locals, env, refs);
            }
        }
    } else {
        emit_value_ref(node, src, from_id, locals, env, refs);
    }
}

fn collect_refs_and_bridges(
    node: Node<'_>,
    src: &str,
    from_id: &str,
    locals: &HashMap<String, Vec<(usize, usize)>>,
    env: &GoLocalEnv<'_>,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "call_expression" => {
                if let Some(func) = n.child_by_field_name("function") {
                    let line = n.start_position().row + 1;
                    let func = if func.kind() == "type_instantiation_expression" {
                        func.child_by_field_name("function")
                            .or_else(|| func.named_child(0))
                            .unwrap_or(func)
                    } else {
                        func
                    };
                    match func.kind() {
                        "identifier" => {
                            let target = node_text(func, src).to_string();
                            refs.push(RawRef {
                                from_symbol_id: from_id.to_string(),
                                target_name: target,
                                qualifier: None,
                                kind: EdgeKind::Calls,
                                line,
                                receiver_chain: None,
                            });
                        }
                        "selector_expression" => {
                            let operand_node = func.child_by_field_name("operand");
                            let operand = operand_node.map(|o| node_text(o, src).to_string());
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
                                let receiver_chain = operand_node
                                    .and_then(|o| resolve_expr_chain(o, 0, src, env));
                                refs.push(RawRef {
                                    from_symbol_id: from_id.to_string(),
                                    target_name: target,
                                    qualifier: operand,
                                    kind: EdgeKind::Calls,
                                    line,
                                    receiver_chain,
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
                if n.kind() == "assignment_statement"
                    && let Some(lhs) = n.child_by_field_name("left")
                {
                    emit_expr_list_refs(lhs, src, from_id, locals, env, refs);
                }
                if let Some(rhs) = n.child_by_field_name("right") {
                    emit_expr_list_refs(rhs, src, from_id, locals, env, refs);
                }
            }
            "var_spec" | "const_spec" => {
                if let Some(val) = n.child_by_field_name("value") {
                    emit_expr_list_refs(val, src, from_id, locals, env, refs);
                }
            }
            "binary_expression" => {
                if let Some(left) = n.child_by_field_name("left") {
                    emit_value_ref(left, src, from_id, locals, env, refs);
                }
                if let Some(right) = n.child_by_field_name("right") {
                    emit_value_ref(right, src, from_id, locals, env, refs);
                }
            }
            "unary_expression" => {
                if let Some(op) = n.child_by_field_name("operand") {
                    emit_value_ref(op, src, from_id, locals, env, refs);
                }
            }
            "index_expression" => {
                if let Some(op) = n.child_by_field_name("operand") {
                    emit_value_ref(op, src, from_id, locals, env, refs);
                }
                if let Some(idx) = n.child_by_field_name("index") {
                    emit_value_ref(idx, src, from_id, locals, env, refs);
                }
            }
            "slice_expression" => {
                for field in ["operand", "start", "end", "capacity"] {
                    if let Some(ch) = n.child_by_field_name(field) {
                        emit_value_ref(ch, src, from_id, locals, env, refs);
                    }
                }
            }
            "expression_case" => {
                if let Some(val) = n.child_by_field_name("value") {
                    emit_expr_list_refs(val, src, from_id, locals, env, refs);
                }
            }
            "if_statement" | "for_statement" => {
                if let Some(cond) = n.child_by_field_name("condition") {
                    emit_value_ref(cond, src, from_id, locals, env, refs);
                }
            }
            "expression_switch_statement" => {
                if let Some(val) = n.child_by_field_name("value") {
                    emit_value_ref(val, src, from_id, locals, env, refs);
                }
            }
            "range_clause" => {
                if let Some(rhs) = n.child_by_field_name("right") {
                    emit_value_ref(rhs, src, from_id, locals, env, refs);
                }
            }
            "inc_statement" | "dec_statement" => {
                if let Some(op) = n.named_child(0) {
                    emit_value_ref(op, src, from_id, locals, env, refs);
                }
            }
            "selector_expression" => {
                if let Some(op) = n.child_by_field_name("operand") {
                    emit_value_ref(op, src, from_id, locals, env, refs);
                }
            }
            "return_statement" => {
                let mut rc = n.walk();
                for ch in n.children(&mut rc) {
                    if ch.is_named() {
                        emit_expr_list_refs(ch, src, from_id, locals, env, refs);
                    }
                }
            }
            "send_statement" => {
                if let Some(val) = n.child_by_field_name("value") {
                    emit_value_ref(val, src, from_id, locals, env, refs);
                }
            }
            "keyed_element" => {
                if let Some(key_elem) = n.child_by_field_name("key") {
                    let key = if key_elem.kind() == "literal_element" {
                        key_elem.named_child(0).unwrap_or(key_elem)
                    } else {
                        key_elem
                    };
                    let is_non_struct_literal = n
                        .parent()
                        .and_then(|p| p.parent())
                        .filter(|p| p.kind() == "composite_literal")
                        .and_then(|cl| cl.child_by_field_name("type"))
                        .is_some_and(|t| matches!(t.kind(), "map_type" | "slice_type" | "array_type"));
                    if key.kind() == "selector_expression" || is_non_struct_literal {
                        emit_value_ref(key, src, from_id, locals, env, refs);
                    }
                }
                if let Some(val_elem) = n.child_by_field_name("value") {
                    let val = if val_elem.kind() == "literal_element" {
                        val_elem.named_child(0).unwrap_or(val_elem)
                    } else {
                        val_elem
                    };
                    emit_value_ref(val, src, from_id, locals, env, refs);
                }
            }
            "literal_value" => {
                let mut lc = n.walk();
                for ch in n.children(&mut lc) {
                    if ch.kind() == "literal_element"
                        && let Some(inner) = ch.named_child(0)
                    {
                        emit_value_ref(inner, src, from_id, locals, env, refs);
                    }
                }
            }
            "argument_list" => {
                let mut ac = n.walk();
                for ch in n.children(&mut ac) {
                    if ch.is_named() {
                        emit_value_ref(ch, src, from_id, locals, env, refs);
                    }
                }
            }
            "array_type" => {
                if let Some(len) = n.child_by_field_name("length") {
                    emit_value_ref(len, src, from_id, locals, env, refs);
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
