pub mod blast;
pub mod bridges;
pub mod dead;
pub mod scip;

use crate::cache::{CacheData, CacheStore, PARSER_VERSION};
use crate::lang;
use crate::model::{Confidence, Edge, EdgeKind, FileExtract, Language, Symbol, SymbolKind};
use crate::walker::DiscoveredFile;
use anyhow::Result;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

pub struct CodeGraph {
    pub extracts: Vec<FileExtract>,
    pub symbols: HashMap<String, Symbol>,
    pub edges: Vec<Edge>,
    pub outgoing: HashMap<String, Vec<usize>>,
    pub incoming: HashMap<String, Vec<usize>>,
}

impl CodeGraph {
    pub fn build(root: &Path, files: &[DiscoveredFile], use_cache: bool) -> Result<Self> {
        let store = CacheStore::new(root);
        let prev_cache = if use_cache {
            store.load()
        } else {
            CacheData {
                version: PARSER_VERSION,
                files: HashMap::new(),
            }
        };

        let extracts: Vec<FileExtract> = files
            .par_iter()
            .filter_map(|df| {
                let bytes = fs::read(&df.abs_path).ok()?;
                let hash = blake3::hash(&bytes).to_hex().to_string();
                if let Some(cached) = prev_cache.files.get(&df.rel_path) {
                    if cached.blake3 == hash && cached.lang == df.lang {
                        return Some(cached.clone());
                    }
                }
                let src = String::from_utf8_lossy(&bytes);
                lang::extract_file(&df.rel_path, df.lang, &src, hash).ok()
            })
            .collect();

        if use_cache {
            let mut next_cache = prev_cache;
            next_cache.version = PARSER_VERSION;
            next_cache.files.retain(|rel, _| root.join(rel).exists());
            for ext in &extracts {
                next_cache.files.insert(ext.file.clone(), ext.clone());
            }
            store.save(&next_cache);
        }

        Ok(Self::from_extracts(extracts))
    }

    pub fn from_extracts(extracts: Vec<FileExtract>) -> Self {
        let mut symbols: HashMap<String, Symbol> = HashMap::new();
        let mut by_file_name: HashMap<(&str, &str), String> = HashMap::new();
        let mut by_pkg_name: HashMap<(Language, &str, &str), Vec<String>> = HashMap::new();
        let mut by_lang_name: HashMap<(Language, &str), Vec<String>> = HashMap::new();
        let mut by_lang_qname: HashMap<(Language, &str), Vec<String>> = HashMap::new();

        for ext in &extracts {
            for sym in &ext.symbols {
                symbols.insert(sym.id.clone(), sym.clone());
            }
        }

        for ext in &extracts {
            for sym in &ext.symbols {
                by_file_name.entry((ext.file.as_str(), sym.name.as_str())).or_insert_with(|| sym.id.clone());
                by_file_name.entry((ext.file.as_str(), sym.qualified_name.as_str())).or_insert_with(|| sym.id.clone());
                if !sym.package_or_module.is_empty() {
                    by_pkg_name
                        .entry((sym.lang, sym.package_or_module.as_str(), sym.name.as_str()))
                        .or_default()
                        .push(sym.id.clone());
                    if let Some((_, short_pkg)) = sym.package_or_module.rsplit_once("::") {
                        by_pkg_name
                            .entry((sym.lang, short_pkg, sym.name.as_str()))
                            .or_default()
                            .push(sym.id.clone());
                    }
                }
                by_lang_name
                    .entry((sym.lang, sym.name.as_str()))
                    .or_default()
                    .push(sym.id.clone());
                by_lang_qname
                    .entry((sym.lang, sym.qualified_name.as_str()))
                    .or_default()
                    .push(sym.id.clone());
                if sym.lang == Language::Svelte {
                    by_lang_name
                        .entry((Language::TypeScript, sym.name.as_str()))
                        .or_default()
                        .push(sym.id.clone());
                } else if sym.lang == Language::TypeScript || sym.lang == Language::JavaScript {
                    by_lang_name
                        .entry((Language::Svelte, sym.name.as_str()))
                        .or_default()
                        .push(sym.id.clone());
                }
            }
        }

        let mut edges: Vec<Edge> = Vec::new();
        let mut seen: HashSet<(String, String, EdgeKind)> = HashSet::new();

        for ext in &extracts {
            let mut alias_map: HashMap<&str, &str> = HashMap::new();
            for imp in &ext.imports {
                if let Some((alias, path)) = imp.split_once('=') {
                    alias_map.insert(alias, path);
                } else if let Some(last) = imp.rsplit('/').next() {
                    alias_map.insert(last, imp.as_str());
                    if last.starts_with('v') && last[1..].chars().all(|c| c.is_ascii_digit()) {
                        if let Some(prev) = imp.rsplit('/').nth(1) {
                            alias_map.insert(prev, imp.as_str());
                        }
                    }
                }
            }

            for r in &ext.refs {
                if !symbols.contains_key(&r.from_symbol_id) {
                    continue;
                }
                let resolved = resolve_ref(
                    ext,
                    r,
                    &alias_map,
                    &symbols,
                    &by_file_name,
                    &by_pkg_name,
                    &by_lang_name,
                    &by_lang_qname,
                );
                for (target_id, conf) in resolved {
                    if target_id == r.from_symbol_id {
                        continue;
                    }
                    let cross_lang = symbols
                        .get(&target_id)
                        .map(|t| t.lang != ext.lang)
                        .unwrap_or(false);
                    if seen.insert((r.from_symbol_id.clone(), target_id.clone(), r.kind)) {
                        edges.push(Edge {
                            from: r.from_symbol_id.clone(),
                            to: target_id,
                            kind: r.kind,
                            confidence: conf,
                            file: ext.file.clone(),
                            line: r.line,
                            cross_lang,
                            via_bridge: None,
                        });
                    }
                }
            }
        }

        bridges::materialize_bridges(&extracts, &symbols, &mut edges);

        let mut outgoing: HashMap<String, Vec<usize>> = HashMap::new();
        let mut incoming: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, e) in edges.iter().enumerate() {
            outgoing.entry(e.from.clone()).or_default().push(idx);
            incoming.entry(e.to.clone()).or_default().push(idx);
        }

        Self {
            extracts,
            symbols,
            edges,
            outgoing,
            incoming,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_ref(
    ext: &FileExtract,
    r: &crate::model::RawRef,
    alias_map: &HashMap<&str, &str>,
    symbols: &HashMap<String, Symbol>,
    by_file_name: &HashMap<(&str, &str), String>,
    by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
    by_lang_name: &HashMap<(Language, &str), Vec<String>>,
    by_lang_qname: &HashMap<(Language, &str), Vec<String>>,
) -> Vec<(String, Confidence)> {
    // 0. Direct symbol ID match (e.g. Svelte component -> <module> script block)
    if r.qualifier.is_none() && symbols.contains_key(&r.target_name) {
        return vec![(r.target_name.clone(), Confidence::Resolved)];
    }

    // 1. O(1) same-file match
    if r.qualifier.is_none() {
        if let Some(id) = by_file_name.get(&(ext.file.as_str(), r.target_name.as_str())) {
            return vec![(id.clone(), Confidence::Resolved)];
        }
    }

    // 2. O(1) qualified match (Type.Method or Type::method)
    if let Some(ref qual) = r.qualifier {
        // Exact file-path qualifier (e.g. relative Svelte default component import)
        if let Some(id) = by_file_name.get(&(qual.as_str(), r.target_name.as_str())) {
            return vec![(id.clone(), Confidence::Resolved)];
        }
        if qual.ends_with(".svelte") {
            return Vec::new();
        }

        for sep in [".", "::"] {
            let q = format!("{}{}{}", qual, sep, r.target_name);
            if let Some(ids) = by_lang_qname.get(&(ext.lang, q.as_str())) {
                if ids.len() <= 3 {
                    return ids.iter().map(|id| (id.clone(), Confidence::Resolved)).collect();
                }
            }
        }

        // Package-qualified lookup (e.g. launch.Run(), composition.NativeRenderer())
        if let Some(imp_path) = alias_map.get(qual.as_str()).copied() {
            let mut pkg_keys = Vec::new();
            if let Some(last) = imp_path.rsplit('/').next() {
                pkg_keys.push(last);
                if last.starts_with('v') && last[1..].chars().all(|c| c.is_ascii_digit()) {
                    if let Some(prev) = imp_path.rsplit('/').nth(1) {
                        pkg_keys.push(prev);
                    }
                }
            }
            if !pkg_keys.contains(&qual.as_str()) {
                pkg_keys.push(qual.as_str());
            }
            let imp_tail = imp_path
                .split('/')
                .skip(1) // drop domain/module prefix when present
                .collect::<Vec<_>>()
                .join("/");
            for pkg_key in pkg_keys {
                if let Some(ids) = by_pkg_name.get(&(ext.lang, pkg_key, r.target_name.as_str())) {
                    let narrowed: Vec<String> = ids
                        .iter()
                        .filter(|id| {
                            symbols
                                .get(*id)
                                .map(|s| {
                                    if s.kind == SymbolKind::Method {
                                        return false;
                                    }
                                    let dir = s.file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                                    if imp_path.contains('/') {
                                        dir == imp_path
                                            || (!imp_tail.is_empty() && (dir == imp_tail || dir.ends_with(&format!("/{}", imp_tail))))
                                            || (!dir.is_empty() && imp_path.ends_with(&format!("/{}", dir)))
                                            || (dir.is_empty() && imp_path.split('/').count() <= 3)
                                    } else {
                                        dir == imp_path
                                    }
                                })
                                .unwrap_or(false)
                        })
                        .cloned()
                        .collect();
                    if !narrowed.is_empty() && narrowed.len() <= 4 {
                        return narrowed
                            .into_iter()
                            .map(|id| (id, Confidence::Resolved))
                            .collect();
                    }
                }
            }
            return Vec::new();
        }

        // Non-package qualifier (receiver variable like c.Close()): prefer same-file method match first
        if let Some(id) = by_file_name.get(&(ext.file.as_str(), r.target_name.as_str())) {
            if symbols.get(id).map(|s| s.kind == SymbolKind::Method).unwrap_or(false) {
                return vec![(id.clone(), Confidence::Resolved)];
            }
        }
    }

    // 3. O(1) same-package/module match
    if !ext.package_or_module.is_empty() {
        if let Some(ids) = by_pkg_name.get(&(ext.lang, ext.package_or_module.as_str(), r.target_name.as_str())) {
            if ids.len() <= 4 {
                return ids.iter().map(|id| (id.clone(), Confidence::Resolved)).collect();
            }
        }
    }

    // 4. Bounded global/import lookup (only when candidate set is small)
    if let Some(cands) = by_lang_name.get(&(ext.lang, r.target_name.as_str())) {
        if cands.len() == 1 {
            return vec![(cands[0].clone(), Confidence::Heuristic)];
        }
        if cands.len() <= 16 && !ext.imports.is_empty() {
            let imported: Vec<String> = cands
                .iter()
                .filter(|id| {
                    symbols
                        .get(*id)
                        .map(|s| {
                            ext.imports.iter().any(|imp| {
                                let clean = imp.trim_start_matches("./").trim_start_matches("../");
                                !clean.is_empty() && (s.file.contains(clean) || s.package_or_module.contains(clean))
                            })
                        })
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            if !imported.is_empty() && imported.len() <= 4 {
                return imported
                    .into_iter()
                    .map(|id| (id, Confidence::Resolved))
                    .collect();
            }
        }
        if cands.len() <= 3 && r.target_name.len() >= 5 {
            return cands
                .iter()
                .map(|id| (id.clone(), Confidence::Heuristic))
                .collect();
        }
    }

    Vec::new()
}
