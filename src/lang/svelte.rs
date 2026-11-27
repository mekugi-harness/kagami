use crate::lang::{make_symbol_id, ts_js};
use crate::model::{
    EdgeKind, FileExtract, Language, RawRef, Span, Symbol, SymbolKind, Visibility,
};
use anyhow::Result;
use std::path::Path;

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
    let mut comp_aliases: std::collections::HashMap<String, (String, Option<String>)> =
        std::collections::HashMap::new();

    // Extract all <script ...>...</script> blocks
    let mut search_pos = 0;
    let mut script_ranges = Vec::new();
    while let Some(open_idx) = src[search_pos..].find("<script") {
        let abs_open = search_pos + open_idx;
        let Some(tag_end_rel) = src[abs_open..].find('>') else { break };
        let body_start = abs_open + tag_end_rel + 1;
        let Some(close_rel) = src[body_start..].find("</script>") else { break };
        let body_end = body_start + close_rel;
        let line_offset = src[..body_start].bytes().filter(|&b| b == b'\n').count();
        let script_src = &src[body_start..body_end];
        let is_module_script = src[abs_open..body_start].contains("module");
        script_ranges.push((abs_open, body_end + "</script>".len()));
        // Parse default Svelte component imports: import PrimaryButton from './Button.svelte'
        for sline in script_src.lines() {
            let t = sline.trim();
            if let Some(rest) = t.strip_prefix("import ") {
                if let Some((alias_part, path_part)) = rest.split_once(" from ") {
                    let alias = alias_part.split(',').next().unwrap_or("").trim();
                    let path = path_part.trim().trim_end_matches(';').trim_matches('"').trim_matches('\'');
                    if !alias.is_empty()
                        && !alias.starts_with('{')
                        && !alias.starts_with('*')
                        && !alias.starts_with("type ")
                        && path.ends_with(".svelte")
                    {
                        if let Some(stem) = Path::new(path).file_stem().and_then(|s| s.to_str()) {
                            let resolved_file = ts_js::resolve_relative_path(rel_path, path);
                            comp_aliases.insert(alias.to_string(), (stem.to_string(), resolved_file));
                        }
                    }
                }
            }
        }

        if let Ok(mut sub) = ts_js::extract(rel_path, script_src, String::new(), Language::Svelte, line_offset) {
            let mut renamed_mod_id: Option<(String, String)> = None;
            for sym in &mut sub.symbols {
                if sym.kind == SymbolKind::Script && sym.name == "<module>" {
                    if is_module_script {
                        let old_id = sym.id.clone();
                        let new_id = make_symbol_id(rel_path, "<script:module>", sym.span.start_line);
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
        }
        search_pos = body_end + "</script>".len();
    }

    // Scan template markup outside <script> blocks for <PascalCaseComponent> and handler refs
    let mut line_num = 1usize;
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if script_ranges.iter().any(|&(s, e)| i >= s && i < e) {
            if bytes[i] == b'\n' {
                line_num += 1;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'\n' {
            line_num += 1;
            i += 1;
            continue;
        }
        // Component tag: <MyComponent
        if bytes[i] == b'<' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_uppercase() {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'.')
            {
                end += 1;
            }
            let tag = &src[start..end];
            if !tag.is_empty() {
                let (target, qualifier) = comp_aliases
                    .get(tag)
                    .cloned()
                    .unwrap_or_else(|| (tag.to_string(), None));
                refs.push(RawRef {
                    from_symbol_id: comp_id.clone(),
                    target_name: target,
                    qualifier,
                    kind: EdgeKind::TemplateRef,
                    line: line_num,
                });
            }
            i = end;
            continue;
        }
        // Event handler or expression: ={handler} or {handler(
        if bytes[i] == b'{' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if end > start && end < bytes.len() && (bytes[end] == b'}' || bytes[end] == b'(') {
                let ident = &src[start..end];
                if !matches!(ident, "if" | "each" | "await" | "key" | "snippet" | "html" | "const" | "debug" | "true" | "false") {
                    refs.push(RawRef {
                        from_symbol_id: comp_id.clone(),
                        target_name: ident.to_string(),
                        qualifier: None,
                        kind: EdgeKind::TemplateRef,
                        line: line_num,
                    });
                }
            }
        }
        i += 1;
    }

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
    })
}
