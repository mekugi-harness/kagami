use crate::graph::bridges::is_virtual_kind;
use crate::graph::CodeGraph;
use crate::model::{Confidence, EdgeKind, Language, Span, SymbolKind};
use globset::GlobSet;
use serde::Serialize;
use std::collections::{HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadScope {
    Prod,
    All,
}

#[derive(Debug, Clone)]
pub struct DeadOptions {
    pub scope: DeadScope,
    pub langs: Option<HashSet<Language>>,
    pub allowlist: HashSet<String>,
    pub include_clusters: bool,
    pub path_filter: Option<GlobSet>,
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
    // 1. Seed live roots
    let mut live: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    for sym in graph.symbols.values() {
        let is_allowlisted = opts.allowlist.contains(&sym.name)
            || opts.allowlist.contains(&sym.qualified_name)
            || opts.allowlist.contains(&format!("{}:{}", sym.file, sym.name));
        let is_root = sym.is_entrypoint
            || is_allowlisted
            || is_virtual_kind(sym.kind)
            || (opts.scope == DeadScope::All && sym.is_test);

        if is_root && live.insert(sym.id.clone()) {
            queue.push_back(sym.id.clone());
        }
    }

    // 2. Mark-and-sweep reachability over outgoing edges (including cross-language bridges!)
    while let Some(curr) = queue.pop_front() {
        if let Some(out_edges) = graph.outgoing.get(&curr) {
            for &eidx in out_edges {
                let edge = &graph.edges[eidx];
                if edge.kind == EdgeKind::EnvRef {
                    continue;
                }
                if opts.scope == DeadScope::Prod {
                    if let Some(src_sym) = graph.symbols.get(&edge.from) {
                        if src_sym.is_test {
                            continue;
                        }
                    }
                }
                if live.insert(edge.to.clone()) {
                    queue.push_back(edge.to.clone());
                }
            }
        }
    }

    // 3. Collect dead callable symbols
    let mut dead_list = Vec::new();
    let mut unreferenced = 0usize;
    let mut unreachable_clusters = 0usize;

    for sym in graph.symbols.values() {
        if is_virtual_kind(sym.kind) || sym.is_entrypoint {
            continue;
        }
        // Only report callable/actionable symbols (functions, methods, recipes, components)
        if !matches!(
            sym.kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Recipe | SymbolKind::Component
        ) {
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
        if live.contains(&sym.id) {
            continue;
        }

        // Count active incoming call sites
        let caller_count = graph
            .incoming
            .get(&sym.id)
            .map(|edges| {
                edges
                    .iter()
                    .filter(|&&eidx| {
                        let e = &graph.edges[eidx];
                        if e.kind == EdgeKind::EnvRef {
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

        let confidence = if sym.is_dynamic_candidate {
            Confidence::Heuristic
        } else {
            Confidence::Resolved
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
