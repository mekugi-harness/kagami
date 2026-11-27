pub mod blast;
pub mod bridges;
pub mod dead;
pub mod scip;

use crate::cache::{CacheData, CacheStore, PARSER_VERSION};
use crate::lang;
use crate::model::{
    Confidence, Edge, EdgeKind, FileExtract, Language, Symbol, SymbolKind, Visibility,
};
use crate::walker::DiscoveredFile;
use anyhow::Result;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

/// Conditional interface dispatch link: when both `receiver_type_id` and `interface_id`
/// are reachable, the implementing methods in `method_ids` become reachable.
#[derive(Debug, Clone)]
pub struct InterfaceImpl {
    pub receiver_type_id: String,
    pub interface_id: String,
    pub method_ids: Vec<String>,
}

pub struct CodeGraph {
    pub extracts: Vec<FileExtract>,
    pub symbols: HashMap<String, Symbol>,
    pub edges: Vec<Edge>,
    pub outgoing: HashMap<String, Vec<usize>>,
    pub incoming: HashMap<String, Vec<usize>>,
    pub interface_impls: Vec<InterfaceImpl>,
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

    pub fn apply_test_paths(&mut self, test_paths: &globset::GlobSet) {
        for sym in self.symbols.values_mut() {
            if test_paths.is_match(&sym.file) {
                sym.is_test = true;
            }
        }
    }

    pub fn from_extracts(extracts: Vec<FileExtract>) -> Self {
        let mut symbols: HashMap<String, Symbol> = HashMap::new();
        let mut known_files: HashSet<&str> = HashSet::new();
        let mut by_file_name: HashMap<(&str, &str), String> = HashMap::new();
        let mut by_pkg_name: HashMap<(Language, &str, &str), Vec<String>> = HashMap::new();
        let mut by_lang_name: HashMap<(Language, &str), Vec<String>> = HashMap::new();
        let mut by_lang_qname: HashMap<(Language, &str), Vec<String>> = HashMap::new();

        for ext in &extracts {
            known_files.insert(ext.file.as_str());
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
            let alias_map = build_import_alias_map(&ext.imports);
            let mut rel_import_paths: HashSet<String> = HashSet::new();
            for imp in &ext.imports {
                if matches!(
                    ext.lang,
                    Language::TypeScript | Language::JavaScript | Language::Svelte
                ) {
                    if let Some(resolved) =
                        crate::lang::ts_js::resolve_relative_path(&ext.file, imp)
                    {
                        rel_import_paths.insert(resolved);
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
                    &rel_import_paths,
                    &known_files,
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
        let interface_impls = materialize_go_interface_edges(
            &extracts,
            &symbols,
            &by_pkg_name,
            &mut edges,
            &mut seen,
        );

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
            interface_impls,
        }
    }
}

fn is_go_stdlib_interface_method(name: &str) -> bool {
    matches!(
        name,
        // error / errors
        "Error"
            | "Unwrap"
            | "Is"
            | "As"
            // fmt
            | "String"
            | "GoString"
            | "Format"
            | "Scan"
            // encoding / json / text / binary / xml / gob / yaml
            | "MarshalJSON"
            | "UnmarshalJSON"
            | "MarshalText"
            | "UnmarshalText"
            | "MarshalBinary"
            | "UnmarshalBinary"
            | "MarshalXML"
            | "UnmarshalXML"
            | "MarshalXMLAttr"
            | "UnmarshalXMLAttr"
            | "GobEncode"
            | "GobDecode"
            | "MarshalYAML"
            | "UnmarshalYAML"
            // io
            | "Read"
            | "Write"
            | "Close"
            | "ReadFrom"
            | "WriteTo"
            | "ReadAt"
            | "WriteAt"
            | "Seek"
            | "ReadByte"
            | "UnreadByte"
            | "ReadRune"
            | "UnreadRune"
            | "WriteByte"
            | "WriteString"
            // net/http
            | "ServeHTTP"
            | "RoundTrip"
            | "Flush"
            | "Hijack"
            // log/slog
            | "LogValue"
    )
}

fn is_go_concrete_utility_embed(embed: &str, alias_map: Option<&HashMap<&str, &str>>) -> bool {
    let canonical = if let Some((pkg_alias, type_name)) = embed.split_once('.') {
        if let Some(imp_path) = alias_map.and_then(|m| m.get(pkg_alias).copied()) {
            format!("{}.{}", imp_path, type_name)
        } else {
            embed.to_string()
        }
    } else {
        embed.to_string()
    };
    matches!(
        canonical.as_str(),
        "sync.Mutex"
            | "sync.RWMutex"
            | "sync.WaitGroup"
            | "sync.Once"
            | "sync.Cond"
            | "sync.Pool"
            | "sync.Map"
            | "bytes.Buffer"
            | "strings.Builder"
            | "sync/atomic.Int32"
            | "sync/atomic.Int64"
            | "sync/atomic.Uint32"
            | "sync/atomic.Uint64"
            | "sync/atomic.Bool"
            | "sync/atomic.Value"
            | "atomic.Int32"
            | "atomic.Int64"
            | "atomic.Uint32"
            | "atomic.Uint64"
            | "atomic.Bool"
            | "atomic.Value"
            | "embed.FS"
            | "testing.T"
            | "testing.B"
            | "testing.F"
    )
}

fn build_import_alias_map<'a>(imports: &'a [String]) -> HashMap<&'a str, &'a str> {
    let mut alias_map: HashMap<&'a str, &'a str> = HashMap::new();
    for imp in imports {
        if let Some((alias, path)) = imp.split_once('=') {
            alias_map.insert(alias, path);
        } else if let Some(last) = imp.rsplit('/').next() {
            alias_map.insert(last, imp.as_str());
            if last.starts_with('v')
                && last[1..].chars().all(|c| c.is_ascii_digit())
                && let Some(prev) = imp.rsplit('/').nth(1)
            {
                alias_map.insert(prev, imp.as_str());
            }
        }
    }
    alias_map
}

fn resolve_go_local_type(
    owner: &Symbol,
    embed_name: &str,
    file_aliases: &HashMap<&str, HashMap<&str, &str>>,
    go_types_by_pkg_name: &HashMap<(&str, &str), String>,
    by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
    symbols: &HashMap<String, Symbol>,
) -> Option<String> {
    if let Some((pkg_alias, type_name)) = embed_name.split_once('.') {
        let alias_map = file_aliases.get(owner.file.as_str())?;
        let imp_path = alias_map.get(pkg_alias).copied()?;
        for pkg_key in pkg_keys_for_import(imp_path, Some(pkg_alias)) {
            if let Some(ids) = by_pkg_name.get(&(Language::Go, pkg_key, type_name)) {
                for id in ids {
                    if let Some(s) = symbols.get(id)
                        && matches!(
                            s.kind,
                            SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Type
                        )
                    {
                        let dir = s.file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                        if import_path_matches_dir(imp_path, dir) {
                            return Some(id.clone());
                        }
                    }
                }
            }
        }
        None
    } else {
        go_types_by_pkg_name
            .get(&(owner.package_or_module.as_str(), embed_name))
            .cloned()
    }
}

fn materialize_go_interface_edges(
    extracts: &[FileExtract],
    symbols: &HashMap<String, Symbol>,
    by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
    edges: &mut Vec<Edge>,
    seen: &mut HashSet<(String, String, EdgeKind)>,
) -> Vec<InterfaceImpl> {
    let mut go_types_by_pkg_name: HashMap<(&str, &str), String> = HashMap::new();
    let mut go_methods_by_type: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut raw_iface_methods: HashMap<String, Vec<String>> = HashMap::new();
    let mut raw_embeds: HashMap<String, Vec<String>> = HashMap::new();
    let mut file_aliases: HashMap<&str, HashMap<&str, &str>> = HashMap::new();

    for ext in extracts {
        if ext.lang != Language::Go {
            continue;
        }
        file_aliases.insert(ext.file.as_str(), build_import_alias_map(&ext.imports));
        for (k, v) in &ext.interface_methods {
            raw_iface_methods.insert(k.clone(), v.clone());
        }
        for (k, v) in &ext.embedded_types {
            raw_embeds.insert(k.clone(), v.clone());
        }
        for sym in &ext.symbols {
            if matches!(
                sym.kind,
                SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Type
            ) {
                go_types_by_pkg_name
                    .entry((sym.package_or_module.as_str(), sym.name.as_str()))
                    .or_insert_with(|| sym.id.clone());
            }
        }
    }

    for sym in symbols.values() {
        if sym.lang != Language::Go || sym.kind != SymbolKind::Method {
            continue;
        }
        if let Some((recv_type, _)) = sym.qualified_name.split_once('.')
            && let Some(type_id) =
                go_types_by_pkg_name.get(&(sym.package_or_module.as_str(), recv_type))
        {
            let type_is_test = symbols.get(type_id).map(|s| s.is_test).unwrap_or(false);
            if sym.is_test && !type_is_test {
                continue;
            }
            go_methods_by_type
                .entry(type_id.clone())
                .or_default()
                .insert(sym.name.clone(), sym.id.clone());
        }
    }

    // Resolve transitive method sets for local Go interfaces (including embedded local interfaces).
    let mut iface_method_sets: HashMap<String, HashSet<String>> = HashMap::new();
    for sym in symbols.values() {
        if sym.lang != Language::Go || sym.kind != SymbolKind::Interface {
            continue;
        }
        let mut methods: HashSet<String> = HashSet::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack = vec![sym.id.clone()];
        while let Some(curr_id) = stack.pop() {
            if !visited.insert(curr_id.clone()) {
                continue;
            }
            if let Some(ms) = raw_iface_methods.get(&curr_id) {
                for m in ms {
                    methods.insert(m.clone());
                }
            }
            if let (Some(owner), Some(embeds)) = (symbols.get(&curr_id), raw_embeds.get(&curr_id)) {
                for emb in embeds {
                    if let Some(target_id) = resolve_go_local_type(
                        owner,
                        emb,
                        &file_aliases,
                        &go_types_by_pkg_name,
                        by_pkg_name,
                        symbols,
                    ) && symbols
                        .get(&target_id)
                        .map(|s| s.kind == SymbolKind::Interface)
                        .unwrap_or(false)
                    {
                        stack.push(target_id);
                    }
                }
            }
        }
        if !methods.is_empty() {
            iface_method_sets.insert(sym.id.clone(), methods);
        }
    }

    let mut push_impl_edge = |type_id: &str, method_id: &str, conf: Confidence| {
        if seen.insert((
            type_id.to_string(),
            method_id.to_string(),
            EdgeKind::Implements,
        )) {
            let (file, line) = symbols
                .get(method_id)
                .map(|s| (s.file.clone(), s.span.start_line))
                .unwrap_or_default();
            edges.push(Edge {
                from: type_id.to_string(),
                to: method_id.to_string(),
                kind: EdgeKind::Implements,
                confidence: conf,
                file,
                line,
                cross_lang: false,
                via_bridge: None,
            });
        }
    };

    // 1. Stdlib / built-in interface methods & embedded-interface overrides
    for (type_id, methods) in &go_methods_by_type {
        let has_sort = methods.contains_key("Len")
            && methods.contains_key("Less")
            && methods.contains_key("Swap");
        let has_heap = has_sort && methods.contains_key("Push") && methods.contains_key("Pop");
        let has_flag_value = methods.contains_key("String") && methods.contains_key("Set");
        let has_response_writer = methods.contains_key("Header")
            && methods.contains_key("Write")
            && methods.contains_key("WriteHeader");

        for (m_name, m_id) in methods {
            let is_stdlib = is_go_stdlib_interface_method(m_name)
                || (has_sort && matches!(m_name.as_str(), "Len" | "Less" | "Swap"))
                || (has_heap && matches!(m_name.as_str(), "Push" | "Pop"))
                || (has_flag_value && m_name == "Set")
                || (has_response_writer && matches!(m_name.as_str(), "Header" | "WriteHeader"));
            if is_stdlib {
                push_impl_edge(type_id, m_id, Confidence::Resolved);
            }
        }

        // Embedded interfaces/types on structs
        if let (Some(owner), Some(embeds)) = (symbols.get(type_id), raw_embeds.get(type_id))
            && owner.kind == SymbolKind::Struct
        {
            let owner_aliases = file_aliases.get(owner.file.as_str());
            let mut has_external_embed = false;
            let mut embedded_local_iface_methods: HashSet<String> = HashSet::new();
            for emb in embeds {
                if let Some(target_id) = resolve_go_local_type(
                    owner,
                    emb,
                    &file_aliases,
                    &go_types_by_pkg_name,
                    by_pkg_name,
                    symbols,
                ) {
                    if let Some(ms) = iface_method_sets.get(&target_id) {
                        embedded_local_iface_methods.extend(ms.iter().cloned());
                    }
                } else if !is_go_concrete_utility_embed(emb, owner_aliases) {
                    has_external_embed = true;
                }
            }
            for (m_name, m_id) in methods {
                if embedded_local_iface_methods.contains(m_name) {
                    push_impl_edge(type_id, m_id, Confidence::Resolved);
                } else if has_external_embed
                    && symbols
                        .get(m_id)
                        .map(|s| s.visibility == Visibility::Public)
                        .unwrap_or(false)
                {
                    push_impl_edge(type_id, m_id, Confidence::Heuristic);
                }
            }
        }
    }

    // 2. Local repo interface structural satisfaction (conditional RTA in dead.rs)
    // Compute effective method map per concrete Go type by BFS embedding depth:
    // shallower declarations win, same-depth conflicts are ambiguous and omitted,
    // and unambiguous promoted methods retain their declaring method symbol ID.
    #[derive(Clone, PartialEq, Eq)]
    enum PromotedMethod {
        Concrete(String),
        InterfaceOnly,
        Ambiguous,
    }
    fn merge_promoted(
        eff: &mut HashMap<String, (usize, PromotedMethod)>,
        name: &str,
        depth: usize,
        state: PromotedMethod,
    ) {
        match eff.get_mut(name) {
            None => {
                eff.insert(name.to_string(), (depth, state));
            }
            Some((prev_depth, prev_state)) if depth < *prev_depth => {
                *prev_depth = depth;
                *prev_state = state;
            }
            Some((prev_depth, prev_state)) if depth == *prev_depth => {
                *prev_state = PromotedMethod::Ambiguous;
            }
            Some(_) => {}
        }
    }
    let mut effective_methods_by_type: HashMap<String, HashMap<String, Option<String>>> =
        HashMap::new();
    let concrete_type_ids: Vec<String> = symbols
        .values()
        .filter(|s| {
            s.lang == Language::Go
                && matches!(s.kind, SymbolKind::Struct | SymbolKind::Type)
                && (go_methods_by_type.contains_key(&s.id) || raw_embeds.contains_key(&s.id))
        })
        .map(|s| s.id.clone())
        .collect();
    for type_id in &concrete_type_ids {
        let mut eff: HashMap<String, (usize, PromotedMethod)> = HashMap::new();
        let mut visit_state: HashMap<String, (usize, usize)> = HashMap::new();
        let mut queue: std::collections::VecDeque<(String, usize)> =
            std::collections::VecDeque::from([(type_id.clone(), 0usize)]);
        while let Some((curr_id, depth)) = queue.pop_front() {
            let entry = visit_state.entry(curr_id.clone()).or_insert((depth, 0));
            if depth > entry.0 || entry.1 >= 2 {
                continue;
            }
            entry.1 += 1;
            if let Some(ms) = go_methods_by_type.get(&curr_id) {
                for (m_name, m_id) in ms {
                    merge_promoted(
                        &mut eff,
                        m_name,
                        depth,
                        PromotedMethod::Concrete(m_id.clone()),
                    );
                }
            }
            if let (Some(owner), Some(embeds)) = (symbols.get(&curr_id), raw_embeds.get(&curr_id)) {
                for emb in embeds {
                    if let Some(target_id) = resolve_go_local_type(
                        owner,
                        emb,
                        &file_aliases,
                        &go_types_by_pkg_name,
                        by_pkg_name,
                        symbols,
                    ) {
                        if let Some(ms) = iface_method_sets.get(&target_id) {
                            for m_name in ms {
                                merge_promoted(
                                    &mut eff,
                                    m_name,
                                    depth + 1,
                                    PromotedMethod::InterfaceOnly,
                                );
                            }
                        } else {
                            queue.push_back((target_id, depth + 1));
                        }
                    }
                }
            }
        }
        let resolved: HashMap<String, Option<String>> = eff
            .into_iter()
            .filter_map(|(k, (_, state))| match state {
                PromotedMethod::Concrete(id) => Some((k, Some(id))),
                PromotedMethod::InterfaceOnly => Some((k, None)),
                PromotedMethod::Ambiguous => None,
            })
            .collect();
        if !resolved.is_empty() {
            effective_methods_by_type.insert(type_id.clone(), resolved);
        }
    }

    let mut types_by_method: HashMap<&str, Vec<&str>> = HashMap::new();
    for (type_id, eff) in &effective_methods_by_type {
        for m in eff.keys() {
            types_by_method
                .entry(m.as_str())
                .or_default()
                .push(type_id.as_str());
        }
    }

    let mut interface_impls = Vec::new();
    for (iface_id, iface_methods) in &iface_method_sets {
        let Some(iface_sym) = symbols.get(iface_id) else {
            continue;
        };
        let has_unexported = iface_methods
            .iter()
            .any(|m| m.chars().next().is_some_and(|c| !c.is_ascii_uppercase()));
        // Start from the rarest method in the interface for fast candidate pruning.
        let Some(rarest_method) = iface_methods
            .iter()
            .min_by_key(|m| types_by_method.get(m.as_str()).map(|v| v.len()).unwrap_or(0))
        else {
            continue;
        };
        let Some(candidates) = types_by_method.get(rarest_method.as_str()) else {
            continue;
        };
        for &type_id in candidates {
            let Some(type_sym) = symbols.get(type_id) else {
                continue;
            };
            if type_sym.kind == SymbolKind::Interface {
                continue;
            }
            if has_unexported && type_sym.package_or_module != iface_sym.package_or_module {
                continue;
            }
            let Some(eff) = effective_methods_by_type.get(type_id) else {
                continue;
            };
            if !iface_methods.iter().all(|m| eff.contains_key(m)) {
                continue;
            }
            let method_ids: Vec<String> = iface_methods
                .iter()
                .filter_map(|m| eff.get(m).and_then(|id| id.clone()))
                .collect();
            if !method_ids.is_empty() {
                interface_impls.push(InterfaceImpl {
                    receiver_type_id: type_id.to_string(),
                    interface_id: iface_id.clone(),
                    method_ids,
                });
            }
        }
    }

    interface_impls
}

#[allow(clippy::too_many_arguments)]
fn resolve_ref(
    ext: &FileExtract,
    r: &crate::model::RawRef,
    alias_map: &HashMap<&str, &str>,
    rel_import_paths: &HashSet<String>,
    known_files: &HashSet<&str>,
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
        // Exact file-path qualifier (relative Svelte component, <script module>, or TS/JS relative import)
        if let Some(id) = by_file_name.get(&(qual.as_str(), r.target_name.as_str())) {
            return vec![(id.clone(), Confidence::Resolved)];
        }
        if rel_import_paths.contains(qual.as_str()) {
            // Select the first existing target module by path priority before looking up the binding
            // so we never fall through to a lower-priority file (e.g. worker/index.ts when worker.ts exists).
            let target_file = if known_files.contains(qual.as_str()) {
                Some(qual.clone())
            } else {
                let mut candidates: Vec<String> = Vec::new();
                // TS NodeNext/ESM imports often use .js/.jsx/.mjs/.cjs specifiers backed by .ts/.tsx sources
                for (js_ext, ts_exts) in [
                    (".js", &[ ".ts", ".tsx" ][..]),
                    (".jsx", &[ ".tsx", ".ts" ][..]),
                    (".mjs", &[ ".mts" ][..]),
                    (".cjs", &[ ".cts" ][..]),
                ] {
                    if let Some(stem) = qual.strip_suffix(js_ext) {
                        for ts_ext in ts_exts {
                            candidates.push(format!("{}{}", stem, ts_ext));
                        }
                    }
                }
                for suffix in [
                    ".ts",
                    ".tsx",
                    ".js",
                    ".jsx",
                    ".mjs",
                    ".cjs",
                    "/index.ts",
                    "/index.tsx",
                    "/index.js",
                ] {
                    candidates.push(format!("{}{}", qual, suffix));
                }
                candidates
                    .into_iter()
                    .find(|cand| known_files.contains(cand.as_str()))
            };
            if let Some(tf) = target_file {
                if let Some(id) = by_file_name.get(&(tf.as_str(), r.target_name.as_str())) {
                    return vec![(id.clone(), Confidence::Resolved)];
                }
            }
            return Vec::new();
        }
        if qual.ends_with(".svelte")
            && (r.kind == EdgeKind::TemplateRef || r.target_name == "<script:module>")
        {
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
            let pkg_keys = pkg_keys_for_import(imp_path, Some(qual.as_str()));
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
                                    import_path_matches_dir(imp_path, dir)
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
        let mut results = Vec::new();
        let mut found_method = false;
        // If qualifier is a same-file or same-package var/const (e.g. Go hooks.Run()),
        // link the var itself so callbacks registered in its initializer remain reachable.
        if let Some(id) = by_file_name.get(&(ext.file.as_str(), qual.as_str())) {
            if symbols.get(id).map(|s| s.kind == SymbolKind::Const).unwrap_or(false) {
                results.push((id.clone(), Confidence::Resolved));
            }
        } else if !ext.package_or_module.is_empty() {
            if let Some(ids) = by_pkg_name.get(&(ext.lang, ext.package_or_module.as_str(), qual.as_str())) {
                for id in ids {
                    if symbols.get(id).map(|s| s.kind == SymbolKind::Const).unwrap_or(false) {
                        results.push((id.clone(), Confidence::Resolved));
                    }
                }
            }
        }
        if let Some(id) = by_file_name.get(&(ext.file.as_str(), r.target_name.as_str())) {
            if symbols.get(id).map(|s| s.kind == SymbolKind::Method).unwrap_or(false) {
                results.push((id.clone(), Confidence::Resolved));
                found_method = true;
            }
        }
        // Fall back to combined same-package + imported-package methods when same-file lookup finds
        // nothing, so an unrelated same-package method never shadows an imported receiver's method.
        if !found_method {
            let mut seen_methods: HashSet<String> = HashSet::new();
            let mut candidate_methods: Vec<String> = Vec::new();
            if !ext.package_or_module.is_empty() {
                if let Some(ids) = by_pkg_name.get(&(ext.lang, ext.package_or_module.as_str(), r.target_name.as_str())) {
                    for id in ids {
                        if symbols.get(id).map(|s| s.kind == SymbolKind::Method).unwrap_or(false)
                            && seen_methods.insert(id.clone())
                        {
                            candidate_methods.push(id.clone());
                        }
                    }
                }
            }
            let mut seen_imps: HashSet<&str> = HashSet::new();
            for &imp_path in alias_map.values() {
                if !seen_imps.insert(imp_path) {
                    continue;
                }
                for pkg_key in pkg_keys_for_import(imp_path, None) {
                    if let Some(ids) = by_pkg_name.get(&(ext.lang, pkg_key, r.target_name.as_str())) {
                        for id in ids {
                            let is_matching_method = symbols
                                .get(id)
                                .map(|s| {
                                    if s.kind != SymbolKind::Method {
                                        return false;
                                    }
                                    let dir = s.file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                                    import_path_matches_dir(imp_path, dir)
                                })
                                .unwrap_or(false);
                            if is_matching_method && seen_methods.insert(id.clone()) {
                                candidate_methods.push(id.clone());
                            }
                        }
                    }
                }
            }
            if !candidate_methods.is_empty() && candidate_methods.len() <= 4 {
                let conf = if candidate_methods.len() == 1 {
                    Confidence::Resolved
                } else {
                    Confidence::Heuristic
                };
                for id in candidate_methods {
                    results.push((id, conf));
                }
            }
        }
        if !results.is_empty() {
            return results;
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
    if r.target_name == "<default>" {
        return Vec::new();
    }
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

fn pkg_keys_for_import<'a>(imp_path: &'a str, extra: Option<&'a str>) -> Vec<&'a str> {
    let mut pkg_keys = Vec::new();
    if let Some(last) = imp_path.rsplit('/').next() {
        pkg_keys.push(last);
        if last.starts_with('v') && last[1..].chars().all(|c| c.is_ascii_digit()) {
            if let Some(prev) = imp_path.rsplit('/').nth(1) {
                pkg_keys.push(prev);
            }
        }
    }
    if let Some(q) = extra {
        if !pkg_keys.contains(&q) {
            pkg_keys.push(q);
        }
    }
    pkg_keys
}

fn import_path_matches_dir(imp_path: &str, dir: &str) -> bool {
    let imp_tail = imp_path
        .split('/')
        .skip(1) // drop domain/module prefix when present
        .collect::<Vec<_>>()
        .join("/");
    if imp_path.contains('/') {
        dir == imp_path
            || (!imp_tail.is_empty()
                && (dir == imp_tail || dir.ends_with(&format!("/{}", imp_tail))))
            || (!dir.is_empty() && imp_path.ends_with(&format!("/{}", dir)))
            || (dir.is_empty() && imp_path.split('/').count() <= 3)
    } else {
        dir == imp_path
    }
}
