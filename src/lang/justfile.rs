use crate::ir::IrFunc;
use crate::lang::{bash, make_symbol_id};
use crate::model::{
    BridgeHint, BridgeKind, BridgeRole, Confidence, EdgeKind, FileExtract, Language, RawRef,
    Span, Symbol, SymbolKind, Visibility,
};
use anyhow::Result;

pub fn extract(rel_path: &str, src: &str, blake3_hex: String) -> Result<FileExtract> {
    let lines: Vec<&str> = src.lines().collect();
    let mut symbols = Vec::new();
    let mut refs = Vec::new();
    let mut bridges = Vec::new();
    let mut ir_funcs = Vec::new();

    let mut i = 0;
    let mut pending_private = false;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        let line_num = i + 1;

        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if trimmed.contains("private") {
                pending_private = true;
            }
            i += 1;
            continue;
        }
        // Exported env var: export FOO := ...
        if let Some(rest) = trimmed.strip_prefix("export ") {
            if let Some((var, _)) = rest.split_once(":=") {
                let key = var.trim().to_string();
                let id = make_symbol_id(rel_path, &key, line_num);
                symbols.push(Symbol {
                    id: id.clone(),
                    name: key.clone(),
                    qualified_name: key.clone(),
                    kind: SymbolKind::Const,
                    lang: Language::Just,
                    file: rel_path.to_string(),
                    span: Span { start_line: line_num, end_line: line_num },
                    visibility: Visibility::Public,
                    is_test: false,
                    is_entrypoint: true,
                    is_dynamic_candidate: false,
                    package_or_module: "justfile".to_string(),
                });
                bridges.push(BridgeHint {
                    from_symbol_id: id,
                    bridge_kind: BridgeKind::EnvKey,
                    key,
                    role: BridgeRole::Producer,
                    line: line_num,
                    confidence: Confidence::Resolved,
                });
            }
            pending_private = false;
            i += 1;
            continue;
        }
        // Variable assignment (:=) or setting (set ...)
        if trimmed.starts_with("set ") || trimmed.contains(":=") {
            pending_private = false;
            i += 1;
            continue;
        }
        // Alias: alias b := build
        if let Some(rest) = trimmed.strip_prefix("alias ") {
            if let Some((lhs, rhs)) = rest.split_once(":=") {
                let alias = lhs.trim().to_string();
                let target = rhs.trim().to_string();
                let id = make_symbol_id(rel_path, &alias, line_num);
                symbols.push(Symbol {
                    id: id.clone(),
                    name: alias.clone(),
                    qualified_name: alias,
                    kind: SymbolKind::Recipe,
                    lang: Language::Just,
                    file: rel_path.to_string(),
                    span: Span { start_line: line_num, end_line: line_num },
                    visibility: Visibility::Public,
                    is_test: false,
                    is_entrypoint: true,
                    is_dynamic_candidate: false,
                    package_or_module: "justfile".to_string(),
                });
                refs.push(RawRef {
                    from_symbol_id: id,
                    target_name: target,
                    qualifier: None,
                    kind: EdgeKind::RecipeDep,
                    line: line_num,
                    receiver_chain: None,
                });
            }
            pending_private = false;
            i += 1;
            continue;
        }

        // Recipe header: name [params...]: [deps...]
        if !line.starts_with(' ') && !line.starts_with('\t') {
            if let Some((lhs, rhs)) = trimmed.split_once(':') {
                let mut tokens = lhs.split_whitespace();
                if let Some(raw_name) = tokens.next() {
                    let name = raw_name.trim_start_matches('@').to_string();
                    if is_valid_recipe_name(&name) {
                        let is_private = pending_private || name.starts_with('_');
                        let start_line = line_num;
                        let mut body_lines = Vec::new();
                        let mut j = i + 1;
                        while j < lines.len() {
                            let bl = lines[j];
                            if bl.starts_with(' ') || bl.starts_with('\t') || bl.trim().is_empty() {
                                body_lines.push(bl.trim_start().trim_start_matches('@'));
                                j += 1;
                            } else {
                                break;
                            }
                        }
                        let end_line = j.max(start_line);
                        let id = make_symbol_id(rel_path, &name, start_line);
                        symbols.push(Symbol {
                            id: id.clone(),
                            name: name.clone(),
                            qualified_name: name.clone(),
                            kind: SymbolKind::Recipe,
                            lang: Language::Just,
                            file: rel_path.to_string(),
                            span: Span { start_line, end_line },
                            visibility: if is_private { Visibility::Private } else { Visibility::Public },
                            is_test: name.contains("test"),
                            is_entrypoint: !is_private,
                            is_dynamic_candidate: false,
                            package_or_module: "justfile".to_string(),
                        });
                        bridges.push(BridgeHint {
                            from_symbol_id: id.clone(),
                            bridge_kind: BridgeKind::ExecTarget,
                            key: format!("just:{}", name),
                            role: BridgeRole::Producer,
                            line: start_line,
                            confidence: Confidence::Resolved,
                        });
                        // Parse dependencies on RHS of ':'
                        for dep in rhs.split_whitespace() {
                            let dep_clean = dep.trim_matches(|c| c == '(' || c == ')');
                            if is_valid_recipe_name(dep_clean) && !dep_clean.starts_with('$') {
                                refs.push(RawRef {
                                    from_symbol_id: id.clone(),
                                    target_name: dep_clean.to_string(),
                                    qualifier: None,
                                    kind: EdgeKind::RecipeDep,
                                    line: start_line,
                                    receiver_chain: None,
                                });
                            }
                        }
                        // Delegate recipe body to bash extractor (replacing {{ ... }} templates)
                        let body_src = body_lines
                            .join("\n")
                            .replace("{{", "$JUST_")
                            .replace("}}", "");
                        if !body_src.trim().is_empty() {
                            if let Ok(sub) = bash::extract(rel_path, &body_src, String::new(), start_line, Some(&id)) {
                                refs.extend(sub.refs);
                                bridges.extend(sub.bridges);
                            }
                            let mut parser = tree_sitter::Parser::new();
                            if parser.set_language(&tree_sitter_bash::LANGUAGE.into()).is_ok() {
                                if let Some(tree) = parser.parse(&body_src, None) {
                                    ir_funcs.push(IrFunc {
                                        symbol_id: id.clone(),
                                        name: name.clone(),
                                        file: rel_path.to_string(),
                                        lang: Language::Just,
                                        span: Span { start_line, end_line },
                                        params: Vec::new(),
                                        body: bash::lower_bash_node(tree.root_node(), &body_src),
                                    });
                                }
                            }
                        }
                        pending_private = false;
                        i = j;
                        continue;
                    }
                }
            }
        }
        pending_private = false;
        i += 1;
    }

    Ok(FileExtract {
        file: rel_path.to_string(),
        lang: Language::Just,
        blake3: blake3_hex,
        package_or_module: "justfile".to_string(),
        imports: Vec::new(),
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

fn is_valid_recipe_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().map_or(false, |c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
