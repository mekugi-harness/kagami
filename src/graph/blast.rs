use crate::graph::CodeGraph;
use crate::model::{Confidence, EdgeKind, Language, Span, Symbol};
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Callers,
    Callees,
    Both,
}

#[derive(Debug, Clone)]
pub struct BlastOptions {
    pub depth: usize,
    pub direction: Direction,
    pub cross_lang: bool,
    pub include_env: bool,
    pub include_tests: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactItem {
    pub hop: usize,
    pub direction: String,
    pub symbol: String,
    pub file: String,
    pub lang: Language,
    pub span: Span,
    pub edge_kind: EdgeKind,
    pub confidence: Confidence,
    pub cross_lang: bool,
    pub via_bridge: Option<String>,
    pub is_test: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlastRadiusReport {
    pub target: Symbol,
    pub total_impacted: usize,
    pub by_language: BTreeMap<String, usize>,
    pub impacts: Vec<ImpactItem>,
}

pub fn analyze_blast_radius(
    graph: &CodeGraph,
    file_path: &str,
    symbol_query: &str,
    opts: &BlastOptions,
) -> Result<BlastRadiusReport> {
    let norm_file = file_path.trim_start_matches("./").replace('\\', "/");

    let mut matches: Vec<&Symbol> = graph
        .symbols
        .values()
        .filter(|s| {
            (s.file == norm_file || s.file.ends_with(&format!("/{}", norm_file)))
                && (s.name == symbol_query || s.qualified_name == symbol_query)
        })
        .collect();

    if matches.is_empty() {
        // Fallback: allow suffix match like Type.Method vs Method
        matches = graph
            .symbols
            .values()
            .filter(|s| {
                (s.file == norm_file || s.file.ends_with(&format!("/{}", norm_file)))
                    && (s.qualified_name.ends_with(symbol_query) || s.name.contains(symbol_query))
            })
            .collect();
    }

    if matches.is_empty() {
        bail!(
            "symbol '{}' not found in '{}'",
            symbol_query,
            norm_file
        );
    }
    matches.sort_by_key(|s| s.span.start_line);
    let target = matches[0].clone();

    let mut visited: HashSet<(String, &'static str)> = HashSet::new();
    visited.insert((target.id.clone(), "caller"));
    visited.insert((target.id.clone(), "callee"));

    let mut queue: VecDeque<(String, usize, &'static str)> = VecDeque::new();
    if matches!(opts.direction, Direction::Callers | Direction::Both) {
        queue.push_back((target.id.clone(), 0, "caller"));
    }
    if matches!(opts.direction, Direction::Callees | Direction::Both) {
        queue.push_back((target.id.clone(), 0, "callee"));
    }

    let mut impacts = Vec::new();

    while let Some((curr_id, dist, dir)) = queue.pop_front() {
        if opts.depth > 0 && dist >= opts.depth {
            continue;
        }
        let edge_indices = if dir == "caller" {
            graph.incoming.get(&curr_id)
        } else {
            graph.outgoing.get(&curr_id)
        };
        let Some(edge_indices) = edge_indices else { continue };

        for &eidx in edge_indices {
            let edge = &graph.edges[eidx];
            if !opts.cross_lang && edge.cross_lang {
                continue;
            }
            if !opts.include_env && edge.kind == EdgeKind::EnvRef {
                continue;
            }
            let next_id = if dir == "caller" { &edge.from } else { &edge.to };
            let Some(next_sym) = graph.symbols.get(next_id) else { continue };
            if !opts.include_tests && next_sym.is_test {
                continue;
            }
            if visited.insert((next_id.clone(), dir)) {
                impacts.push(ImpactItem {
                    hop: dist + 1,
                    direction: dir.to_string(),
                    symbol: next_sym.qualified_name.clone(),
                    file: next_sym.file.clone(),
                    lang: next_sym.lang,
                    span: next_sym.span,
                    edge_kind: edge.kind,
                    confidence: edge.confidence,
                    cross_lang: edge.cross_lang || next_sym.lang != target.lang,
                    via_bridge: edge.via_bridge.clone(),
                    is_test: next_sym.is_test,
                });
                queue.push_back((next_id.clone(), dist + 1, dir));
            }
        }
    }

    impacts.sort_by(|a, b| {
        a.hop
            .cmp(&b.hop)
            .then_with(|| a.direction.cmp(&b.direction))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.span.start_line.cmp(&b.span.start_line))
    });

    let mut by_language = BTreeMap::new();
    for item in &impacts {
        *by_language
            .entry(item.lang.as_str().to_string())
            .or_insert(0) += 1;
    }

    Ok(BlastRadiusReport {
        target,
        total_impacted: impacts.len(),
        by_language,
        impacts,
    })
}
