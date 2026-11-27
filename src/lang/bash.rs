use crate::ir::{IrFunc, IrNode, LoopKind};
use crate::lang::{literal_node, make_symbol_id, node_span, node_text, strip_quotes};
use crate::model::{
    BridgeHint, BridgeKind, BridgeRole, Confidence, EdgeKind, FileExtract, Language, RawRef,
    Span, Symbol, SymbolKind, Visibility,
};
use anyhow::Result;
use std::path::Path;
use tree_sitter::{Node, Parser};

pub fn extract(
    rel_path: &str,
    src: &str,
    blake3_hex: String,
    line_offset: usize,
    owner_symbol_id: Option<&str>,
) -> Result<FileExtract> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_bash::LANGUAGE.into())?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("failed to parse bash file {}", rel_path))?;

    let mut symbols = Vec::new();
    let mut refs = Vec::new();
    let mut bridges = Vec::new();
    let mut ir_funcs = Vec::new();

    let script_id = if let Some(owner) = owner_symbol_id {
        owner.to_string()
    } else {
        let name = Path::new(rel_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(rel_path)
            .to_string();
        let id = make_symbol_id(rel_path, &name, 1 + line_offset);
        symbols.push(Symbol {
            id: id.clone(),
            name: name.clone(),
            qualified_name: name,
            kind: SymbolKind::Script,
            lang: Language::Bash,
            file: rel_path.to_string(),
            span: Span {
                start_line: 1 + line_offset,
                end_line: src.lines().count().max(1) + line_offset,
            },
            visibility: Visibility::Public,
            is_test: rel_path.contains("test"),
            is_entrypoint: true,
            is_dynamic_candidate: false,
            package_or_module: rel_path.to_string(),
        });
        // Register script path as ExecTarget producer
        bridges.push(BridgeHint {
            from_symbol_id: id.clone(),
            bridge_kind: BridgeKind::ExecTarget,
            key: rel_path.to_string(),
            role: BridgeRole::Producer,
            line: 1 + line_offset,
            confidence: Confidence::Resolved,
        });
        id
    };

    let root = tree.root_node();
    collect_bash_walk(
        root,
        src,
        rel_path,
        line_offset,
        &script_id,
        &mut symbols,
        &mut refs,
        &mut bridges,
        &mut ir_funcs,
    );

    Ok(FileExtract {
        file: rel_path.to_string(),
        lang: Language::Bash,
        blake3: blake3_hex,
        package_or_module: rel_path.to_string(),
        imports: Vec::new(),
        symbols,
        refs,
        bridges,
        ir_funcs,
        interface_methods: std::collections::HashMap::new(),
        embedded_types: std::collections::HashMap::new(),
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_bash_walk(
    node: Node<'_>,
    src: &str,
    rel_path: &str,
    line_offset: usize,
    current_sym: &str,
    symbols: &mut Vec<Symbol>,
    refs: &mut Vec<RawRef>,
    bridges: &mut Vec<BridgeHint>,
    ir_funcs: &mut Vec<IrFunc>,
) {
    let mut cursor = node.walk();
    for ch in node.children(&mut cursor) {
        let line = ch.start_position().row + 1 + line_offset;
        match ch.kind() {
            "function_definition" => {
                if let Some(name_node) = ch.child_by_field_name("name") {
                    let name = node_text(name_node, src).to_string();
                    let span = node_span(ch, line_offset);
                    let id = make_symbol_id(rel_path, &name, span.start_line);
                    symbols.push(Symbol {
                        id: id.clone(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind: SymbolKind::Function,
                        lang: Language::Bash,
                        file: rel_path.to_string(),
                        span,
                        visibility: Visibility::Public,
                        is_test: rel_path.contains("test"),
                        is_entrypoint: false,
                        is_dynamic_candidate: false,
                        package_or_module: rel_path.to_string(),
                    });
                    if let Some(body) = ch.child_by_field_name("body") {
                        collect_bash_walk(body, src, rel_path, line_offset, &id, symbols, refs, bridges, ir_funcs);
                        ir_funcs.push(IrFunc {
                            symbol_id: id,
                            name,
                            file: rel_path.to_string(),
                            lang: Language::Bash,
                            span,
                            params: Vec::new(),
                            body: lower_bash_node(body, src),
                        });
                    }
                    continue;
                }
            }
            "command" => {
                if let Some(cmd_name) = ch.child_by_field_name("name") {
                    let cmd = strip_quotes(node_text(cmd_name, src));
                    let mut args = Vec::new();
                    let mut ac = ch.walk();
                    for a in ch.children(&mut ac) {
                        if a.id() != cmd_name.id() && a.is_named() {
                            args.push(strip_quotes(node_text(a, src)));
                        }
                    }
                    // Emit call ref (for shell functions)
                    refs.push(RawRef {
                        from_symbol_id: current_sym.to_string(),
                        target_name: cmd.clone(),
                        qualifier: None,
                        kind: EdgeKind::Calls,
                        line,
                    });
                    // Emit ExecTarget bridges for scripts, binaries, go run, bun run, just
                    emit_bash_exec_bridges(&cmd, &args, current_sym, line, bridges);
                }
            }
            "declaration_command" => {
                let txt = node_text(ch, src);
                if txt.starts_with("export ") {
                    for part in txt.trim_start_matches("export ").split_whitespace() {
                        if let Some((k, _)) = part.split_once('=') {
                            bridges.push(BridgeHint {
                                from_symbol_id: current_sym.to_string(),
                                bridge_kind: BridgeKind::EnvKey,
                                key: k.to_string(),
                                role: BridgeRole::Producer,
                                line,
                                confidence: Confidence::Resolved,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        collect_bash_walk(ch, src, rel_path, line_offset, current_sym, symbols, refs, bridges, ir_funcs);
    }
}

fn emit_bash_exec_bridges(
    cmd: &str,
    args: &[String],
    from_id: &str,
    line: usize,
    bridges: &mut Vec<BridgeHint>,
) {
    let norm = cmd.trim_start_matches("./");
    if norm.ends_with(".sh") || norm.ends_with(".bash") || norm.ends_with(".py") {
        bridges.push(BridgeHint {
            from_symbol_id: from_id.to_string(),
            bridge_kind: BridgeKind::ExecTarget,
            key: norm.to_string(),
            role: BridgeRole::Consumer,
            line,
            confidence: Confidence::Resolved,
        });
    } else if matches!(cmd, "source" | "." | "bash" | "sh" | "python3") {
        if let Some(target) = args.iter().find(|a| !a.starts_with('-')) {
            bridges.push(BridgeHint {
                from_symbol_id: from_id.to_string(),
                bridge_kind: BridgeKind::ExecTarget,
                key: target.trim_start_matches("./").to_string(),
                role: BridgeRole::Consumer,
                line,
                confidence: Confidence::Resolved,
            });
        }
    } else if cmd == "go" && args.first().map(|s| s.as_str()) == Some("run") {
        if let Some(pkg) = args.iter().skip(1).find(|a| a.starts_with("./")) {
            bridges.push(BridgeHint {
                from_symbol_id: from_id.to_string(),
                bridge_kind: BridgeKind::ExecTarget,
                key: pkg.trim_start_matches("./").to_string(),
                role: BridgeRole::Consumer,
                line,
                confidence: Confidence::Resolved,
            });
        }
    } else if cmd == "just" {
        if let Some(recipe) = args.iter().find(|a| !a.starts_with('-')) {
            bridges.push(BridgeHint {
                from_symbol_id: from_id.to_string(),
                bridge_kind: BridgeKind::ExecTarget,
                key: format!("just:{}", recipe),
                role: BridgeRole::Consumer,
                line,
                confidence: Confidence::Resolved,
            });
        }
    } else if matches!(cmd, "mekugi" | "mekugi-app-server" | "codex-code-mode-host" | "codex") {
        bridges.push(BridgeHint {
            from_symbol_id: from_id.to_string(),
            bridge_kind: BridgeKind::ExecTarget,
            key: cmd.to_string(),
            role: BridgeRole::Consumer,
            line,
            confidence: Confidence::Resolved,
        });
    }
}

pub fn lower_bash_node(node: Node<'_>, src: &str) -> IrNode {
    match node.kind() {
        "program" | "compound_statement" | "do_group" => {
            let mut stmts = Vec::new();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.is_named() && ch.kind() != "comment" {
                    stmts.push(lower_bash_node(ch, src));
                }
            }
            IrNode::Seq(stmts)
        }
        "if_statement" => {
            let cond = node
                .child_by_field_name("condition")
                .map(|c| lower_bash_node(c, src))
                .unwrap_or_else(|| IrNode::Opaque("cond".into()));
            let mut then_stmts = Vec::new();
            let mut else_br = None;
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if !ch.is_named() || ch.kind() == "comment" {
                    continue;
                }
                if node.child_by_field_name("condition").map(|x| x.id()) == Some(ch.id()) {
                    continue;
                }
                if ch.kind() == "else_clause" || ch.kind() == "elif_clause" {
                    else_br = Some(Box::new(lower_bash_node(ch, src)));
                } else {
                    then_stmts.push(lower_bash_node(ch, src));
                }
            }
            IrNode::Branch {
                cond: Box::new(cond),
                then_br: Box::new(IrNode::Seq(then_stmts)),
                else_br,
            }
        }
        "case_statement" => {
            let scrutinee = node
                .child_by_field_name("value")
                .map(|v| Box::new(lower_bash_node(v, src)));
            let mut arms = Vec::new();
            let mut default = None;
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.kind() == "case_item" {
                    let pat = ch.child_by_field_name("value").map(|v| node_text(v, src).trim().to_string()).unwrap_or_default();
                    let body = lower_bash_case_body(ch, src);
                    if pat == "*" {
                        default = Some(Box::new(body));
                    } else {
                        arms.push((literal_node(&pat), body));
                    }
                }
            }
            IrNode::MultiBranch {
                scrutinee,
                arms,
                default,
            }
        }
        "for_statement" => {
            let var = node.named_child(0).map(|n| node_text(n, src).to_string());
            let body = node
                .child_by_field_name("body")
                .map(|b| lower_bash_node(b, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            IrNode::Loop {
                kind: LoopKind::IterSeq {
                    seq: Box::new(IrNode::Opaque("for_words".into())),
                },
                var,
                body: Box::new(body),
            }
        }
        "while_statement" => {
            let cond = node
                .child_by_field_name("condition")
                .map(|c| lower_bash_node(c, src))
                .unwrap_or_else(|| IrNode::Opaque("cond".into()));
            let body = node
                .child_by_field_name("body")
                .map(|b| lower_bash_node(b, src))
                .unwrap_or_else(|| IrNode::Seq(vec![]));
            IrNode::Loop {
                kind: LoopKind::While { cond: Box::new(cond) },
                var: None,
                body: Box::new(body),
            }
        }
        "command" => {
            let callee = node
                .child_by_field_name("name")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_default();
            let mut args = Vec::new();
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                if ch.is_named() && node.child_by_field_name("name").map(|n| n.id()) != Some(ch.id()) {
                    args.push(lower_bash_node(ch, src));
                }
            }
            IrNode::Call { callee, args }
        }
        "variable_assignment" => {
            let dst = node
                .child_by_field_name("name")
                .map(|n| node_text(n, src).to_string())
                .unwrap_or_else(|| "_".into());
            let val = node
                .child_by_field_name("value")
                .map(|v| lower_bash_node(v, src))
                .unwrap_or_else(|| literal_node(r#""""#));
            IrNode::Assign {
                dst,
                val: Box::new(val),
            }
        }
        "simple_expansion" | "expansion" | "variable_name" => {
            IrNode::Var(node_text(node, src).trim_start_matches('$').to_string())
        }
        "word" | "string" | "raw_string" | "number" => literal_node(node_text(node, src)),
        _ => IrNode::Opaque(node.kind().to_string()),
    }
}

fn lower_bash_case_body(item: Node<'_>, src: &str) -> IrNode {
    let mut stmts = Vec::new();
    let mut c = item.walk();
    for ch in item.children(&mut c) {
        if !ch.is_named() || ch.kind() == "comment" {
            continue;
        }
        if item.child_by_field_name("value").map(|v| v.id()) == Some(ch.id()) {
            continue;
        }
        stmts.push(lower_bash_node(ch, src));
    }
    IrNode::Seq(stmts)
}
