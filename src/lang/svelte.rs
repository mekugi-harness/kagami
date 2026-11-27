use crate::lang::make_symbol_id;
use crate::lang::ts_js::{self, TemplateExpr, TemplateExprKind, TsRefContext};
use crate::model::{EdgeKind, FileExtract, Language, RawRef, Span, Symbol, SymbolKind, Visibility};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;

struct ScriptBlock {
    body_start: usize,
    body_end: usize,
    line_offset: usize,
    is_module: bool,
}

pub fn extract(rel_path: &str, src: &str, blake3_hex: String) -> Result<FileExtract> {
    let comp_name = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Component")
        .to_string();

    let is_route = rel_path.contains("/routes/")
        && (comp_name.starts_with("+page")
            || comp_name.starts_with("+layout")
            || comp_name.starts_with("+error"));

    let total_lines = src.lines().count().max(1);
    let comp_id = make_symbol_id(rel_path, &comp_name, 1);

    let mut symbols = vec![Symbol {
        id: comp_id.clone(),
        name: comp_name.clone(),
        qualified_name: comp_name,
        kind: SymbolKind::Component,
        lang: Language::Svelte,
        file: rel_path.to_string(),
        span: Span {
            start_line: 1,
            end_line: total_lines,
        },
        visibility: Visibility::Public,
        is_test: false,
        is_entrypoint: is_route,
        is_dynamic_candidate: false,
        package_or_module: rel_path.to_string(),
    }];
    let mut imports = Vec::new();
    let mut refs = Vec::new();
    let mut bridges = Vec::new();
    let mut ir_funcs = Vec::new();
    let mut embedded_types: HashMap<String, Vec<String>> = HashMap::new();

    // 1. Locate all <script> and <style> ranges (HTML comments are handled inline in markup)
    let mut script_blocks = Vec::new();
    let mut skip_ranges = Vec::new();
    collect_tag_blocks(
        src,
        "<script",
        "</script>",
        |abs_open, body_start, body_end, abs_end| {
            let line_offset = src[..body_start].bytes().filter(|&b| b == b'\n').count();
            let is_module = src[abs_open..body_start].contains("module");
            script_blocks.push(ScriptBlock {
                body_start,
                body_end,
                line_offset,
                is_module,
            });
            skip_ranges.push((abs_open, abs_end));
        },
    );
    collect_tag_blocks(src, "<style", "</style>", |abs_open, _, _, abs_end| {
        skip_ranges.push((abs_open, abs_end));
    });
    skip_ranges.sort_by_key(|&(s, _)| s);

    // 2. Collect combined script context (imports, namespace imports, local callables)
    let mut shared_ctx = TsRefContext {
        file: rel_path.to_string(),
        ..Default::default()
    };
    for sb in &script_blocks {
        let script_src = &src[sb.body_start..sb.body_end];
        let _ = ts_js::collect_svelte_script_context(rel_path, script_src, &mut shared_ctx);
    }

    // 3. Scan template markup for component tags, bind:this bindings, directives, and expressions
    let mut template_exprs = Vec::new();
    let mut bind_this_map: HashMap<String, String> = HashMap::new();
    scan_template(
        src,
        &skip_ranges,
        &comp_id,
        &shared_ctx,
        &mut refs,
        &mut template_exprs,
        &mut bind_this_map,
    );
    shared_ctx.svelte_bind_this.extend(bind_this_map);

    // 4. Extract <script> blocks with shared context (including bind:this component bindings)
    for sb in &script_blocks {
        let script_src = &src[sb.body_start..sb.body_end];
        if let Ok(mut sub) = ts_js::extract_with_context(
            rel_path,
            script_src,
            String::new(),
            Language::Svelte,
            sb.line_offset,
            Some(&shared_ctx),
        ) {
            let mut renamed_mod_id: Option<(String, String)> = None;
            for sym in &mut sub.symbols {
                if sym.kind == SymbolKind::Script && sym.name == "<module>" {
                    if sb.is_module {
                        let old_id = sym.id.clone();
                        let new_id =
                            make_symbol_id(rel_path, "<script:module>", sym.span.start_line);
                        sym.id = new_id.clone();
                        sym.name = "<script:module>".to_string();
                        sym.qualified_name = "<script:module>".to_string();
                        renamed_mod_id = Some((old_id, new_id));
                    }
                    refs.push(RawRef {
                        from_symbol_id: comp_id.clone(),
                        target_name: sym.id.clone(),
                        qualifier: None,
                        kind: EdgeKind::Calls,
                        line: sym.span.start_line,
                        receiver_chain: None,
                    });
                }
            }
            if let Some((old_id, new_id)) = renamed_mod_id {
                for r in &mut sub.refs {
                    if r.from_symbol_id == old_id {
                        r.from_symbol_id = new_id.clone();
                    }
                }
                for b in &mut sub.bridges {
                    if b.from_symbol_id == old_id {
                        b.from_symbol_id = new_id.clone();
                    }
                }
            }
            imports.extend(sub.imports);
            symbols.extend(sub.symbols);
            refs.extend(sub.refs);
            bridges.extend(sub.bridges);
            ir_funcs.extend(sub.ir_funcs);
            for (k, v) in sub.embedded_types {
                embedded_types.entry(k).or_default().extend(v);
            }
        }
    }

    // 5. Parse all collected Svelte 5 template expressions via TypeScript parser
    let _ = ts_js::extract_template_exprs(
        &comp_id,
        &template_exprs,
        &shared_ctx,
        &mut refs,
        &mut bridges,
    );

    Ok(FileExtract {
        file: rel_path.to_string(),
        lang: Language::Svelte,
        blake3: blake3_hex,
        package_or_module: rel_path.to_string(),
        imports,
        symbols,
        refs,
        bridges,
        ir_funcs,
        interface_methods: HashMap::new(),
        embedded_types,
        struct_fields: HashMap::new(),
        func_return_types: HashMap::new(),
        trait_impls: Vec::new(),
        impl_macros: Vec::new(),
        cfg_gated_symbols: HashMap::new(),
        type_aliases: HashMap::new(),
        rust_uses: Vec::new(),
        symbol_mod_scopes: HashMap::new(),
        func_callback_params: HashMap::new(),
    })
}

fn collect_tag_blocks(
    src: &str,
    open_tag: &str,
    close_tag: &str,
    mut on_block: impl FnMut(usize, usize, usize, usize),
) {
    let mut search_pos = 0;
    while let Some(open_idx) = src[search_pos..].find(open_tag) {
        let abs_open = search_pos + open_idx;
        let after = abs_open + open_tag.len();
        if after < src.len() {
            let next_ch = src.as_bytes()[after];
            if !next_ch.is_ascii_whitespace() && next_ch != b'>' {
                search_pos = after;
                continue;
            }
        }
        let Some(tag_end_rel) = src[abs_open..].find('>') else {
            break;
        };
        let body_start = abs_open + tag_end_rel + 1;
        let Some(close_rel) = src[body_start..].find(close_tag) else {
            break;
        };
        let body_end = body_start + close_rel;
        let abs_end = body_end + close_tag.len();
        on_block(abs_open, body_start, body_end, abs_end);
        search_pos = abs_end;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateScopeKind {
    Root,
    Block,
    ComponentTag(String),
}

fn flatten_scopes(scopes: &[(TemplateScopeKind, HashSet<String>)]) -> HashSet<String> {
    let mut merged = HashSet::new();
    for (_, s) in scopes {
        merged.extend(s.iter().cloned());
    }
    merged
}

fn scan_template(
    src: &str,
    skip_ranges: &[(usize, usize)],
    comp_id: &str,
    ctx: &TsRefContext,
    refs: &mut Vec<RawRef>,
    exprs: &mut Vec<TemplateExpr>,
    bind_this_map: &mut HashMap<String, String>,
) {
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut line_num = 1usize;
    let mut template_scopes: Vec<(TemplateScopeKind, HashSet<String>)> =
        vec![(TemplateScopeKind::Root, HashSet::new())];

    while i < bytes.len() {
        if let Some(&(_, end)) = skip_ranges.iter().find(|&&(s, e)| i >= s && i < e) {
            line_num += src[i..end].bytes().filter(|&b| b == b'\n').count();
            i = end;
            continue;
        }
        match bytes[i] {
            b'\n' => {
                line_num += 1;
                i += 1;
            }
            b'<' => {
                if src[i..].starts_with("<!--") {
                    let end = src[i + 4..]
                        .find("-->")
                        .map(|rel| i + 4 + rel + 3)
                        .unwrap_or(bytes.len());
                    line_num += src[i..end].bytes().filter(|&b| b == b'\n').count();
                    i = end;
                    continue;
                }
                let shadowed = flatten_scopes(&template_scopes);
                let (next_i, tag_event) = scan_opening_tag(
                    src,
                    i,
                    &mut line_num,
                    comp_id,
                    ctx,
                    &shadowed,
                    refs,
                    exprs,
                    bind_this_map,
                );
                match tag_event {
                    TagScopeEvent::OpenComponent(name) => {
                        template_scopes
                            .push((TemplateScopeKind::ComponentTag(name), HashSet::new()));
                    }
                    TagScopeEvent::CloseComponent(name) => {
                        if let Some(pos) = template_scopes
                            .iter()
                            .rposition(|(k, _)| k == &TemplateScopeKind::ComponentTag(name.clone()))
                        {
                            template_scopes.truncate(pos);
                        }
                    }
                    TagScopeEvent::None => {}
                }
                i = next_i;
            }
            b'{' => {
                let start_line = line_num;
                if let Some(close_idx) = scan_balanced_braces(src, i) {
                    let inner = &src[i + 1..close_idx];
                    push_mustache_exprs(inner, start_line, &mut template_scopes, exprs);
                    line_num += src[i..=close_idx].bytes().filter(|&b| b == b'\n').count();
                    i = close_idx + 1;
                } else {
                    i += 1;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
}

enum TagScopeEvent {
    None,
    OpenComponent(String),
    CloseComponent(String),
}

#[allow(clippy::too_many_arguments)]
fn scan_opening_tag(
    src: &str,
    lt_idx: usize,
    line_num: &mut usize,
    comp_id: &str,
    ctx: &TsRefContext,
    shadowed: &HashSet<String>,
    refs: &mut Vec<RawRef>,
    exprs: &mut Vec<TemplateExpr>,
    bind_this_map: &mut HashMap<String, String>,
) -> (usize, TagScopeEvent) {
    let bytes = src.as_bytes();
    let mut i = lt_idx + 1;
    if i >= bytes.len() {
        return (i, TagScopeEvent::None);
    }
    // Closing tag </...> or declaration <!...>
    if bytes[i] == b'/' || bytes[i] == b'!' {
        let is_close = bytes[i] == b'/';
        let close_start = i + 1;
        while i < bytes.len() && bytes[i] != b'>' {
            if bytes[i] == b'\n' {
                *line_num += 1;
            }
            i += 1;
        }
        let event = if is_close && close_start < i {
            let close_name = src[close_start..i].trim();
            if close_name
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_uppercase())
                || matches!(close_name, "svelte:fragment" | "svelte:boundary")
            {
                TagScopeEvent::CloseComponent(close_name.to_string())
            } else {
                TagScopeEvent::None
            }
        } else {
            TagScopeEvent::None
        };
        return ((i + 1).min(bytes.len()), event);
    }
    if !bytes[i].is_ascii_alphabetic() {
        return (i, TagScopeEvent::None);
    }
    let tag_start = i;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric()
            || bytes[i] == b'_'
            || bytes[i] == b'-'
            || bytes[i] == b':'
            || bytes[i] == b'.')
    {
        i += 1;
    }
    let tag_name = &src[tag_start..i];
    let tag_line = *line_num;

    // Resolve component tag references (<MyComp>, <Ns.SubComp>)
    let mut comp_svelte_file: Option<String> = None;
    let is_component_tag = tag_name
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_uppercase());
    let opens_const_scope =
        is_component_tag || matches!(tag_name, "svelte:fragment" | "svelte:boundary");
    let root_tag_ident = tag_name.split('.').next().unwrap_or(tag_name);
    if is_component_tag && !shadowed.contains(root_tag_ident) {
        if let Some((ns, member)) = tag_name.split_once('.') {
            if let Some(ns_file) = ctx.namespace_imports.get(ns) {
                refs.push(RawRef {
                    from_symbol_id: comp_id.to_string(),
                    target_name: member.to_string(),
                    qualifier: Some(ns_file.clone()),
                    kind: EdgeKind::TemplateRef,
                    line: tag_line,
                    receiver_chain: None,
                });
            } else {
                refs.push(RawRef {
                    from_symbol_id: comp_id.to_string(),
                    target_name: tag_name.to_string(),
                    qualifier: None,
                    kind: EdgeKind::TemplateRef,
                    line: tag_line,
                    receiver_chain: None,
                });
            }
        } else if let Some(binding) = ctx.imported_bindings.get(tag_name) {
            if let Some(ref rf) = binding.resolved_file
                && rf.ends_with(".svelte")
            {
                comp_svelte_file = Some(rf.clone());
            }
            refs.push(RawRef {
                from_symbol_id: comp_id.to_string(),
                target_name: binding.target_name.clone(),
                qualifier: binding.resolved_file.clone(),
                kind: EdgeKind::TemplateRef,
                line: tag_line,
                receiver_chain: None,
            });
        } else {
            refs.push(RawRef {
                from_symbol_id: comp_id.to_string(),
                target_name: tag_name.to_string(),
                qualifier: None,
                kind: EdgeKind::TemplateRef,
                line: tag_line,
                receiver_chain: None,
            });
        }
    }

    // Scan attributes inside the opening tag
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            if bytes[i] == b'\n' {
                *line_num += 1;
            }
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'>' {
            let ev = if opens_const_scope {
                TagScopeEvent::OpenComponent(tag_name.to_string())
            } else {
                TagScopeEvent::None
            };
            return (i + 1, ev);
        }
        if bytes[i] == b'/' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                return (i + 2, TagScopeEvent::None);
            }
            i += 1;
            continue;
        }
        // Shorthand attribute {ident}, spread {...expr}, or {@attach expr}
        if bytes[i] == b'{' {
            let expr_line = *line_num;
            if let Some(close_idx) = scan_balanced_braces(src, i) {
                let inner = src[i + 1..close_idx].trim();
                let code = if let Some(rest) = strip_tag_keyword(inner, "@attach") {
                    rest
                } else if let Some(rest) = inner.strip_prefix("...") {
                    rest.trim()
                } else {
                    inner
                };
                if !code.is_empty() {
                    exprs.push(TemplateExpr {
                        line: expr_line,
                        kind: TemplateExprKind::Expr,
                        code: code.to_string(),
                        shadowed: shadowed.clone(),
                    });
                }
                *line_num += src[i..=close_idx].bytes().filter(|&b| b == b'\n').count();
                i = close_idx + 1;
            } else {
                i += 1;
            }
            continue;
        }
        // Named attribute or directive (e.g. onclick, bind:this, use:followBottom, transition:fade|global)
        let attr_start = i;
        let attr_line = *line_num;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let attr_name = &src[attr_start..i];
        if attr_name.is_empty() {
            i += 1;
            continue;
        }

        // Directives that directly reference a function/action: use:, in:, out:, transition:, animate:
        for prefix in ["use:", "in:", "out:", "transition:", "animate:"] {
            if let Some(rest) = attr_name.strip_prefix(prefix) {
                let fn_expr = rest.split('|').next().unwrap_or("").trim();
                if !fn_expr.is_empty() {
                    exprs.push(TemplateExpr {
                        line: attr_line,
                        kind: TemplateExprKind::Expr,
                        code: fn_expr.to_string(),
                        shadowed: shadowed.clone(),
                    });
                }
            }
        }

        // Check for = value
        let mut j = i;
        let mut ws_lines = 0usize;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            if bytes[j] == b'\n' {
                ws_lines += 1;
            }
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'=' {
            *line_num += ws_lines;
            i = j + 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                if bytes[i] == b'\n' {
                    *line_num += 1;
                }
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'{' => {
                    let val_line = *line_num;
                    if let Some(close_idx) = scan_balanced_braces(src, i) {
                        let inner = src[i + 1..close_idx].trim();
                        if attr_name == "bind:this"
                            && let Some(ref rf) = comp_svelte_file
                            && is_simple_ident(inner)
                        {
                            bind_this_map.insert(inner.to_string(), rf.clone());
                        }
                        if !inner.is_empty() {
                            exprs.push(TemplateExpr {
                                line: val_line,
                                kind: TemplateExprKind::Expr,
                                code: inner.to_string(),
                                shadowed: shadowed.clone(),
                            });
                        }
                        *line_num += src[i..=close_idx].bytes().filter(|&b| b == b'\n').count();
                        i = close_idx + 1;
                    } else {
                        i += 1;
                    }
                }
                b'"' | b'\'' => {
                    let quote = bytes[i];
                    i += 1;
                    while i < bytes.len() && bytes[i] != quote {
                        if bytes[i] == b'\n' {
                            *line_num += 1;
                            i += 1;
                        } else if bytes[i] == b'{' {
                            let interp_line = *line_num;
                            if let Some(close_idx) = scan_balanced_braces(src, i) {
                                let inner = src[i + 1..close_idx].trim();
                                if !inner.is_empty() {
                                    exprs.push(TemplateExpr {
                                        line: interp_line,
                                        kind: TemplateExprKind::Expr,
                                        code: inner.to_string(),
                                        shadowed: shadowed.clone(),
                                    });
                                }
                                *line_num +=
                                    src[i..=close_idx].bytes().filter(|&b| b == b'\n').count();
                                i = close_idx + 1;
                            } else {
                                i += 1;
                            }
                        } else {
                            i += 1;
                        }
                    }
                    if i < bytes.len() && bytes[i] == quote {
                        i += 1;
                    }
                }
                _ => {
                    while i < bytes.len()
                        && !bytes[i].is_ascii_whitespace()
                        && bytes[i] != b'>'
                        && bytes[i] != b'/'
                    {
                        i += 1;
                    }
                }
            }
        }
    }
    (i, TagScopeEvent::None)
}

fn push_mustache_exprs(
    raw_inner: &str,
    line: usize,
    template_scopes: &mut Vec<(TemplateScopeKind, HashSet<String>)>,
    exprs: &mut Vec<TemplateExpr>,
) {
    let inner = raw_inner.trim();
    if inner.is_empty() {
        return;
    }
    if inner.starts_with('/') {
        if let Some(pos) = template_scopes
            .iter()
            .rposition(|(k, _)| *k == TemplateScopeKind::Block)
        {
            template_scopes.truncate(pos);
        }
        return;
    }
    if let Some(rest) = strip_tag_keyword(inner, "#if") {
        let shadowed = flatten_scopes(template_scopes);
        if !rest.is_empty() {
            exprs.push(TemplateExpr {
                line,
                kind: TemplateExprKind::Expr,
                code: rest.to_string(),
                shadowed,
            });
        }
        template_scopes.push((TemplateScopeKind::Block, HashSet::new()));
        return;
    }
    if let Some(rest) = strip_tag_keyword(inner, ":else if") {
        if let Some(pos) = template_scopes
            .iter()
            .rposition(|(k, _)| *k == TemplateScopeKind::Block)
        {
            template_scopes.truncate(pos);
        }
        let shadowed = flatten_scopes(template_scopes);
        if !rest.is_empty() {
            exprs.push(TemplateExpr {
                line,
                kind: TemplateExprKind::Expr,
                code: rest.to_string(),
                shadowed,
            });
        }
        template_scopes.push((TemplateScopeKind::Block, HashSet::new()));
        return;
    }
    if strip_tag_keyword(inner, ":else").is_some() {
        if let Some(pos) = template_scopes
            .iter()
            .rposition(|(k, _)| *k == TemplateScopeKind::Block)
        {
            template_scopes.truncate(pos);
        }
        template_scopes.push((TemplateScopeKind::Block, HashSet::new()));
        return;
    }
    if let Some(rest) = strip_tag_keyword(inner, "#key") {
        let shadowed = flatten_scopes(template_scopes);
        if !rest.is_empty() {
            exprs.push(TemplateExpr {
                line,
                kind: TemplateExprKind::Expr,
                code: rest.to_string(),
                shadowed,
            });
        }
        template_scopes.push((TemplateScopeKind::Block, HashSet::new()));
        return;
    }
    if let Some(rest) = strip_tag_keyword(inner, "@html")
        .or_else(|| strip_tag_keyword(inner, "@render"))
        .or_else(|| strip_tag_keyword(inner, "@attach"))
        .or_else(|| strip_tag_keyword(inner, "@debug"))
    {
        if !rest.is_empty() {
            exprs.push(TemplateExpr {
                line,
                kind: TemplateExprKind::Expr,
                code: rest.to_string(),
                shadowed: flatten_scopes(template_scopes),
            });
        }
        return;
    }
    if let Some(rest) = strip_tag_keyword(inner, "@const") {
        if !rest.is_empty() {
            exprs.push(TemplateExpr {
                line,
                kind: TemplateExprKind::ConstDecl,
                code: rest.to_string(),
                shadowed: flatten_scopes(template_scopes),
            });
            if let Some((lhs, _)) = split_top_level_char(rest, b'=')
                && let Some((_, top)) = template_scopes.last_mut()
            {
                top.extend(ts_js::extract_ts_binding_names(lhs));
            }
        }
        return;
    }
    if let Some(rest) = strip_tag_keyword(inner, "#each") {
        let outer_shadowed = flatten_scopes(template_scopes);
        if let Some((list_expr, tail)) = split_top_level_keyword(rest, "as") {
            let list_expr = list_expr.trim();
            if !list_expr.is_empty() {
                exprs.push(TemplateExpr {
                    line,
                    kind: TemplateExprKind::Expr,
                    code: list_expr.to_string(),
                    shadowed: outer_shadowed,
                });
            }
            let (pat_part, key_opt) = split_trailing_paren_expr(tail.trim());
            let loop_bindings = ts_js::extract_ts_binding_names(pat_part);
            template_scopes.push((TemplateScopeKind::Block, loop_bindings));
            if let Some(key_expr) = key_opt
                && !key_expr.is_empty()
            {
                exprs.push(TemplateExpr {
                    line,
                    kind: TemplateExprKind::Expr,
                    code: key_expr.to_string(),
                    shadowed: flatten_scopes(template_scopes),
                });
            }
        } else {
            if !rest.is_empty() {
                exprs.push(TemplateExpr {
                    line,
                    kind: TemplateExprKind::Expr,
                    code: rest.to_string(),
                    shadowed: outer_shadowed,
                });
            }
            template_scopes.push((TemplateScopeKind::Block, HashSet::new()));
        }
        return;
    }
    if let Some(rest) = strip_tag_keyword(inner, "#await") {
        let outer_shadowed = flatten_scopes(template_scopes);
        let (promise, bound) = if let Some((lhs, rhs)) = split_top_level_keyword(rest, "then") {
            (lhs.trim(), ts_js::extract_ts_binding_names(rhs))
        } else if let Some((lhs, rhs)) = split_top_level_keyword(rest, "catch") {
            (lhs.trim(), ts_js::extract_ts_binding_names(rhs))
        } else {
            (rest, HashSet::new())
        };
        if !promise.is_empty() {
            exprs.push(TemplateExpr {
                line,
                kind: TemplateExprKind::Expr,
                code: promise.to_string(),
                shadowed: outer_shadowed,
            });
        }
        template_scopes.push((TemplateScopeKind::Block, bound));
        return;
    }
    if let Some(rest) =
        strip_tag_keyword(inner, ":then").or_else(|| strip_tag_keyword(inner, ":catch"))
    {
        if let Some(pos) = template_scopes
            .iter()
            .rposition(|(k, _)| *k == TemplateScopeKind::Block)
        {
            template_scopes.truncate(pos);
        }
        template_scopes.push((
            TemplateScopeKind::Block,
            ts_js::extract_ts_binding_names(rest),
        ));
        return;
    }
    if let Some(rest) = strip_tag_keyword(inner, "#snippet") {
        let mut bound = HashSet::new();
        if let Some(open) = rest.find('(')
            && let Some(close) = rest.rfind(')')
            && close > open
        {
            bound.extend(ts_js::extract_ts_binding_names(&rest[open + 1..close]));
        }
        template_scopes.push((TemplateScopeKind::Block, bound));
        return;
    }
    exprs.push(TemplateExpr {
        line,
        kind: TemplateExprKind::Expr,
        code: inner.to_string(),
        shadowed: flatten_scopes(template_scopes),
    });
}

fn strip_tag_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(kw)?;
    if rest.is_empty() || rest.as_bytes()[0].is_ascii_whitespace() {
        Some(rest.trim())
    } else {
        None
    }
}

fn is_simple_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        }
        _ => false,
    }
}

fn split_top_level_keyword<'a>(s: &'a str, kw: &str) -> Option<(&'a str, &'a str)> {
    let bytes = s.as_bytes();
    let kw_bytes = kw.as_bytes();
    let mut depth_paren = 0usize;
    let mut depth_brace = 0usize;
    let mut depth_bracket = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                i = skip_quoted(bytes, i);
                continue;
            }
            b'`' => {
                i = skip_template_literal(bytes, i);
                continue;
            }
            b'(' => depth_paren += 1,
            b')' => depth_paren = depth_paren.saturating_sub(1),
            b'{' => depth_brace += 1,
            b'}' => depth_brace = depth_brace.saturating_sub(1),
            b'[' => depth_bracket += 1,
            b']' => depth_bracket = depth_bracket.saturating_sub(1),
            _ => {
                if depth_paren == 0
                    && depth_brace == 0
                    && depth_bracket == 0
                    && i > 0
                    && bytes[i - 1].is_ascii_whitespace()
                    && bytes[i..].starts_with(kw_bytes)
                    && i + kw_bytes.len() < bytes.len()
                    && bytes[i + kw_bytes.len()].is_ascii_whitespace()
                {
                    return Some((&s[..i], &s[i + kw_bytes.len()..]));
                }
            }
        }
        i += 1;
    }
    None
}

fn split_top_level_char(s: &str, target: u8) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut depth_paren = 0usize;
    let mut depth_brace = 0usize;
    let mut depth_bracket = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                i = skip_quoted(bytes, i);
                continue;
            }
            b'`' => {
                i = skip_template_literal(bytes, i);
                continue;
            }
            b'(' => depth_paren += 1,
            b')' => depth_paren = depth_paren.saturating_sub(1),
            b'{' => depth_brace += 1,
            b'}' => depth_brace = depth_brace.saturating_sub(1),
            b'[' => depth_bracket += 1,
            b']' => depth_bracket = depth_bracket.saturating_sub(1),
            b if b == target && depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 => {
                return Some((&s[..i], &s[i + 1..]));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_trailing_paren_expr(s: &str) -> (&str, Option<&str>) {
    if !s.ends_with(')') {
        return (s, None);
    }
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b')' => depth += 1,
            b'(' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return (s[..i].trim(), Some(s[i + 1..bytes.len() - 1].trim()));
                }
            }
            _ => {}
        }
    }
    (s, None)
}

fn scan_balanced_braces(src: &str, open_idx: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    if bytes.get(open_idx) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut i = open_idx;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            b'\'' | b'"' => {
                i = skip_quoted(bytes, i);
            }
            b'`' => {
                i = skip_template_literal(bytes, i);
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

fn skip_quoted(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
        } else if bytes[i] == quote {
            return i + 1;
        } else {
            i += 1;
        }
    }
    bytes.len()
}

fn skip_template_literal(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
        } else if bytes[i] == b'`' {
            return i + 1;
        } else if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                match bytes[i] {
                    b'{' => {
                        depth += 1;
                        i += 1;
                    }
                    b'}' => {
                        depth -= 1;
                        i += 1;
                    }
                    b'\'' | b'"' => i = skip_quoted(bytes, i),
                    b'`' => i = skip_template_literal(bytes, i),
                    _ => i += 1,
                }
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}
