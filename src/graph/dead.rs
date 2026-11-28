use crate::cache::GraphSnapshot;
use crate::graph::bridges::is_virtual_kind;
use crate::graph::CodeGraph;
use crate::model::{Confidence, EdgeKind, Language, Span, SymbolKind};
use globset::GlobSet;
use rustc_hash::FxHashMap;
use serde::Serialize;
use std::collections::{HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadScope {
    Prod,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeadKinds {
    #[default]
    Callables,
    Types,
    All,
}

impl DeadKinds {
    pub fn matches(self, kind: SymbolKind) -> bool {
        let is_callable = matches!(
            kind,
            SymbolKind::Function
                | SymbolKind::Method
                | SymbolKind::Recipe
                | SymbolKind::Component
        );
        let is_type_or_val = matches!(
            kind,
            SymbolKind::Type
                | SymbolKind::Interface
                | SymbolKind::Struct
                | SymbolKind::Enum
                | SymbolKind::Trait
                | SymbolKind::Const
                | SymbolKind::Var
        );
        match self {
            DeadKinds::Callables => is_callable,
            DeadKinds::Types => is_type_or_val,
            DeadKinds::All => is_callable || is_type_or_val,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeadOptions {
    pub scope: DeadScope,
    pub langs: Option<HashSet<Language>>,
    pub allowlist: HashSet<String>,
    pub include_clusters: bool,
    pub path_filter: Option<GlobSet>,
    pub min_confidence: Confidence,
    pub kinds: DeadKinds,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeadSymbol {
    pub symbol: String,
    pub kind: SymbolKind,
    pub lang: Language,
    pub file: String,
    pub span: Span,
    pub reason: String,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeadReport {
    pub total_symbols: usize,
    pub total_dead: usize,
    pub unreferenced: usize,
    pub unreachable_clusters: usize,
    pub symbols: Vec<DeadSymbol>,
}

pub fn analyze_dead(graph: &CodeGraph, opts: &DeadOptions) -> DeadReport {
    let n = graph.symbols.len();
    let mut sym_list: Vec<&crate::model::Symbol> = Vec::with_capacity(n);
    let mut id_to_idx: FxHashMap<&str, usize> =
        FxHashMap::with_capacity_and_hasher(n, Default::default());
    for sym in graph.symbols.values() {
        let idx = sym_list.len();
        id_to_idx.insert(sym.id.as_str(), idx);
        sym_list.push(sym);
    }

    // 1. Seed live roots
    let mut live: Vec<bool> = vec![false; n];
    let mut queue: VecDeque<usize> = VecDeque::new();
    let has_allowlist = !opts.allowlist.is_empty();

    for (idx, sym) in sym_list.iter().enumerate() {
        let is_allowlisted = has_allowlist
            && (opts.allowlist.contains(&sym.name)
                || opts.allowlist.contains(&sym.qualified_name)
                || opts.allowlist.contains(&format!("{}:{}", sym.file, sym.name)));
        let is_root = sym.is_entrypoint
            || is_allowlisted
            || is_virtual_kind(sym.kind)
            || (opts.scope == DeadScope::All && sym.is_test);

        if is_root && !live[idx] {
            live[idx] = true;
            queue.push_back(idx);
        }
    }

    // Pre-resolve interface_impls to dense indices for zero-allocation RTA iterations
    let indexed_impls: Vec<(usize, usize, Vec<usize>)> = graph
        .interface_impls
        .iter()
        .filter_map(|imp| {
            let r_idx = *id_to_idx.get(imp.receiver_type_id.as_str())?;
            let i_idx = *id_to_idx.get(imp.interface_id.as_str())?;
            let m_idxs: Vec<usize> = imp
                .method_ids
                .iter()
                .filter_map(|mid| id_to_idx.get(mid.as_str()).copied())
                .collect();
            Some((r_idx, i_idx, m_idxs))
        })
        .collect();

    // 2. Mark-and-sweep reachability over outgoing edges (including cross-language bridges!)
    loop {
        while let Some(curr_idx) = queue.pop_front() {
            let curr_sym = sym_list[curr_idx];
            if opts.scope == DeadScope::Prod && curr_sym.is_test {
                continue;
            }
            if let Some(out_edges) = graph.outgoing.get(&curr_sym.id) {
                for &eidx in out_edges {
                    let edge = &graph.edges[eidx];
                    if edge.kind == EdgeKind::EnvRef {
                        continue;
                    }
                    if let Some(&to_idx) = id_to_idx.get(edge.to.as_str())
                        && !live[to_idx]
                    {
                        live[to_idx] = true;
                        queue.push_back(to_idx);
                    }
                }
            }
        }
        let mut added = false;
        for (r_idx, i_idx, m_idxs) in &indexed_impls {
            if live[*r_idx] && live[*i_idx] {
                for &m_idx in m_idxs {
                    if !live[m_idx] {
                        live[m_idx] = true;
                        queue.push_back(m_idx);
                        added = true;
                    }
                }
            }
        }
        if !added {
            break;
        }
    }

    // 3. Collect dead symbols matching the requested kind filter
    let mut dead_list = Vec::new();
    let mut unreferenced = 0usize;
    let mut unreachable_clusters = 0usize;

    for (idx, sym) in sym_list.iter().enumerate() {
        if is_virtual_kind(sym.kind) || sym.is_entrypoint || sym.name == "_" {
            continue;
        }
        // Skip internal Rust macro_rules! symbols (stored as Const with trailing '!')
        if sym.lang == Language::Rust && sym.name.ends_with('!') {
            continue;
        }
        if !opts.kinds.matches(sym.kind) {
            continue;
        }
        if opts.scope == DeadScope::Prod && sym.is_test {
            continue;
        }
        if let Some(ref langs) = opts.langs {
            if !langs.contains(&sym.lang) {
                continue;
            }
        }
        if let Some(ref pf) = opts.path_filter {
            if !pf.is_match(&sym.file) {
                continue;
            }
        }
        if live[idx] {
            continue;
        }

        // Count active incoming references (excluding self-edges, EnvRef, and Implements)
        let caller_count = graph
            .incoming
            .get(&sym.id)
            .map(|edges| {
                edges
                    .iter()
                    .filter(|&&eidx| {
                        let e = &graph.edges[eidx];
                        if e.from == sym.id
                            || e.kind == EdgeKind::EnvRef
                            || e.kind == EdgeKind::Implements
                        {
                            return false;
                        }
                        if opts.scope == DeadScope::Prod {
                            if let Some(src) = graph.symbols.get(&e.from) {
                                if src.is_test {
                                    return false;
                                }
                            }
                        }
                        true
                    })
                    .count()
            })
            .unwrap_or(0);

        let confidence = if sym.is_dynamic_candidate {
            Confidence::Heuristic
        } else {
            Confidence::Resolved
        };
        if confidence < opts.min_confidence {
            continue;
        }

        let reason = if caller_count == 0 {
            unreferenced += 1;
            "unreferenced".to_string()
        } else {
            if !opts.include_clusters {
                continue;
            }
            unreachable_clusters += 1;
            "unreachable_cluster".to_string()
        };

        dead_list.push(DeadSymbol {
            symbol: sym.qualified_name.clone(),
            kind: sym.kind,
            lang: sym.lang,
            file: sym.file.clone(),
            span: sym.span,
            reason,
            confidence,
        });
    }

    dead_list.sort_by(|a, b| {
        b.confidence
            .cmp(&a.confidence)
            .then_with(|| a.reason.cmp(&b.reason))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.span.start_line.cmp(&b.span.start_line))
    });

    DeadReport {
        total_symbols: graph.symbols.len(),
        total_dead: dead_list.len(),
        unreferenced,
        unreachable_clusters,
        symbols: dead_list,
    }
}

/// Run dead-code RTA analysis directly on a compact cached `GraphSnapshot` using dense `u32`
/// indices without hydrating `HashMap<String, _>` or cloning per-edge symbol ID strings.
pub fn analyze_dead_snapshot(snap: &GraphSnapshot, opts: &DeadOptions) -> DeadReport {
    let n = snap.symbols.len();
    let mut live: Vec<bool> = vec![false; n];
    let mut queue: VecDeque<usize> = VecDeque::new();
    let has_allowlist = !opts.allowlist.is_empty();

    for (idx, sym) in snap.symbols.iter().enumerate() {
        let is_allowlisted = has_allowlist
            && (opts.allowlist.contains(&sym.name)
                || opts.allowlist.contains(&sym.qualified_name)
                || opts.allowlist.contains(&format!("{}:{}", sym.file, sym.name)));
        let is_root = sym.is_entrypoint
            || is_allowlisted
            || is_virtual_kind(sym.kind)
            || (opts.scope == DeadScope::All && sym.is_test);

        if is_root && !live[idx] {
            live[idx] = true;
            queue.push_back(idx);
        }
    }

    let mut outgoing: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut caller_counts: Vec<usize> = vec![0; n];
    for ce in &snap.edges {
        let from = ce.from as usize;
        let to = ce.to as usize;
        if from >= n || to >= n {
            continue;
        }
        if ce.kind != EdgeKind::EnvRef {
            outgoing[from].push(to);
        }
        if from != to
            && ce.kind != EdgeKind::EnvRef
            && ce.kind != EdgeKind::Implements
            && !(opts.scope == DeadScope::Prod && snap.symbols[from].is_test)
        {
            caller_counts[to] += 1;
        }
    }

    loop {
        while let Some(curr_idx) = queue.pop_front() {
            if opts.scope == DeadScope::Prod && snap.symbols[curr_idx].is_test {
                continue;
            }
            for &to_idx in &outgoing[curr_idx] {
                if !live[to_idx] {
                    live[to_idx] = true;
                    queue.push_back(to_idx);
                }
            }
        }
        let mut added = false;
        for imp in &snap.interface_impls {
            let r_idx = imp.receiver_type_idx as usize;
            let i_idx = imp.interface_idx as usize;
            if r_idx < n && i_idx < n && live[r_idx] && live[i_idx] {
                for &m_u32 in &imp.method_indices {
                    let m_idx = m_u32 as usize;
                    if m_idx < n && !live[m_idx] {
                        live[m_idx] = true;
                        queue.push_back(m_idx);
                        added = true;
                    }
                }
            }
        }
        if !added {
            break;
        }
    }

    let mut dead_list = Vec::new();
    let mut unreferenced = 0usize;
    let mut unreachable_clusters = 0usize;

    for (idx, sym) in snap.symbols.iter().enumerate() {
        if is_virtual_kind(sym.kind) || sym.is_entrypoint || sym.name == "_" {
            continue;
        }
        if sym.lang == Language::Rust && sym.name.ends_with('!') {
            continue;
        }
        if !opts.kinds.matches(sym.kind) {
            continue;
        }
        if opts.scope == DeadScope::Prod && sym.is_test {
            continue;
        }
        if let Some(ref langs) = opts.langs
            && !langs.contains(&sym.lang)
        {
            continue;
        }
        if let Some(ref pf) = opts.path_filter
            && !pf.is_match(&sym.file)
        {
            continue;
        }
        if live[idx] {
            continue;
        }

        let confidence = if sym.is_dynamic_candidate {
            Confidence::Heuristic
        } else {
            Confidence::Resolved
        };
        if confidence < opts.min_confidence {
            continue;
        }

        let reason = if caller_counts[idx] == 0 {
            unreferenced += 1;
            "unreferenced".to_string()
        } else {
            if !opts.include_clusters {
                continue;
            }
            unreachable_clusters += 1;
            "unreachable_cluster".to_string()
        };

        dead_list.push(DeadSymbol {
            symbol: sym.qualified_name.clone(),
            kind: sym.kind,
            lang: sym.lang,
            file: sym.file.clone(),
            span: sym.span,
            reason,
            confidence,
        });
    }

    dead_list.sort_by(|a, b| {
        b.confidence
            .cmp(&a.confidence)
            .then_with(|| a.reason.cmp(&b.reason))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.span.start_line.cmp(&b.span.start_line))
    });

    DeadReport {
        total_symbols: n,
        total_dead: dead_list.len(),
        unreferenced,
        unreachable_clusters,
        symbols: dead_list,
    }
}
