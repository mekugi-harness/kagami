use crate::graph::CodeGraph;
use crate::model::{Confidence, Edge, EdgeKind};
use anyhow::{bail, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Default)]
pub struct ScipIndex {
    pub documents: Vec<ScipDocument>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ScipDocument {
    pub relative_path: String,
    #[serde(default)]
    pub occurrences: Vec<ScipOccurrence>,
    #[serde(default)]
    pub symbols: Vec<ScipSymbolInfo>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ScipOccurrence {
    #[serde(default)]
    pub range: Vec<i32>,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub symbol_roles: i32,
}

#[derive(Debug, Default, Deserialize)]
pub struct ScipSymbolInfo {
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub relationships: Vec<ScipRelationship>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ScipRelationship {
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub is_reference: bool,
    #[serde(default)]
    pub is_implementation: bool,
    #[serde(default)]
    pub is_type_definition: bool,
}

#[derive(Debug, Deserialize)]
struct ScipJsonIndex {
    #[serde(default)]
    documents: Vec<ScipDocument>,
}

/// Load a SCIP index from either binary protobuf (`.scip`) or JSON (`.scip.json`).
pub fn load_scip_file(path: &Path) -> Result<ScipIndex> {
    let bytes = fs::read(path)?;
    if bytes.first().map(|&b| b == b'{').unwrap_or(false) {
        let json_idx: ScipJsonIndex = serde_json::from_slice(&bytes)?;
        return Ok(ScipIndex {
            documents: json_idx.documents,
        });
    }
    decode_scip_protobuf(&bytes)
}

/// Apply a SCIP index overlay onto an existing CodeGraph, adding or upgrading edges to Confidence::Exact.
pub fn apply_scip_overlay(graph: &mut CodeGraph, index: &ScipIndex) -> usize {
    // Map (file, line) -> list of symbol IDs defined at or spanning that line
    let mut by_file_syms: HashMap<&str, Vec<(usize, usize, String)>> = HashMap::new();
    for sym in graph.symbols.values() {
        by_file_syms
            .entry(sym.file.as_str())
            .or_default()
            .push((sym.span.start_line, sym.span.end_line, sym.id.clone()));
    }

    // 1. Resolve SCIP symbol strings -> CodeGraph symbol IDs via definition occurrences (role bit 0 = 1)
    let mut scip_to_graph_id: HashMap<String, String> = HashMap::new();
    for doc in &index.documents {
        let rel = doc.relative_path.trim_start_matches("./").replace('\\', "/");
        let Some(file_syms) = by_file_syms.get(rel.as_str()) else {
            continue;
        };
        for occ in &doc.occurrences {
            if (occ.symbol_roles & 1) == 0 || occ.symbol.is_empty() || occ.range.is_empty() {
                continue;
            }
            let line = (occ.range[0] + 1) as usize;
            // Find exact start_line match first, else nearest containing span
            if let Some((_, _, id)) = file_syms.iter().find(|(s, _, _)| *s == line) {
                scip_to_graph_id.insert(occ.symbol.clone(), id.clone());
            } else if let Some((_, _, id)) = file_syms.iter().find(|(s, e, _)| line >= *s && line <= *e) {
                scip_to_graph_id.insert(occ.symbol.clone(), id.clone());
            }
        }
    }

    // Existing edge index for fast upgrade to Confidence::Exact
    let mut existing: HashMap<(String, String), usize> = HashMap::new();
    for (idx, e) in graph.edges.iter().enumerate() {
        existing.insert((e.from.clone(), e.to.clone()), idx);
    }

    let mut added_or_upgraded = 0usize;
    let mut seen_new: HashSet<(String, String, EdgeKind)> = HashSet::new();

    // 2. Process reference occurrences (non-definition occurrences inside a symbol span)
    for doc in &index.documents {
        let rel = doc.relative_path.trim_start_matches("./").replace('\\', "/");
        let Some(file_syms) = by_file_syms.get(rel.as_str()) else {
            continue;
        };
        for occ in &doc.occurrences {
            if (occ.symbol_roles & 1) != 0 || occ.symbol.is_empty() || occ.range.is_empty() {
                continue;
            }
            let Some(target_id) = scip_to_graph_id.get(&occ.symbol) else {
                continue;
            };
            let line = (occ.range[0] + 1) as usize;
            // Find innermost enclosing symbol in this file
            let enclosing = file_syms
                .iter()
                .filter(|(s, e, _)| line >= *s && line <= *e)
                .min_by_key(|(s, e, _)| e.saturating_sub(*s));
            let Some((_, _, from_id)) = enclosing else {
                continue;
            };
            if from_id == target_id {
                continue;
            }
            if let Some(&eidx) = existing.get(&(from_id.clone(), target_id.clone())) {
                if graph.edges[eidx].confidence < Confidence::Exact {
                    graph.edges[eidx].confidence = Confidence::Exact;
                    added_or_upgraded += 1;
                }
            } else if seen_new.insert((from_id.clone(), target_id.clone(), EdgeKind::Calls)) {
                let cross_lang = match (graph.symbols.get(from_id), graph.symbols.get(target_id)) {
                    (Some(a), Some(b)) => a.lang != b.lang,
                    _ => false,
                };
                let eidx = graph.edges.len();
                graph.edges.push(Edge {
                    from: from_id.clone(),
                    to: target_id.clone(),
                    kind: EdgeKind::Calls,
                    confidence: Confidence::Exact,
                    file: rel.clone(),
                    line,
                    cross_lang,
                    via_bridge: Some("SCIP".to_string()),
                });
                graph.outgoing.entry(from_id.clone()).or_default().push(eidx);
                graph.incoming.entry(target_id.clone()).or_default().push(eidx);
                existing.insert((from_id.clone(), target_id.clone()), eidx);
                added_or_upgraded += 1;
            }
        }

        // 3. Process implementation relationships
        for sinfo in &doc.symbols {
            let Some(from_id) = scip_to_graph_id.get(&sinfo.symbol) else {
                continue;
            };
            for rel_item in &sinfo.relationships {
                if !rel_item.is_implementation {
                    continue;
                }
                let Some(to_id) = scip_to_graph_id.get(&rel_item.symbol) else {
                    continue;
                };
                if from_id == to_id {
                    continue;
                }
                if seen_new.insert((from_id.clone(), to_id.clone(), EdgeKind::Implements)) {
                    let eidx = graph.edges.len();
                    let file = graph.symbols.get(from_id).map(|s| s.file.clone()).unwrap_or_default();
                    let line = graph.symbols.get(from_id).map(|s| s.span.start_line).unwrap_or(1);
                    graph.edges.push(Edge {
                        from: from_id.clone(),
                        to: to_id.clone(),
                        kind: EdgeKind::Implements,
                        confidence: Confidence::Exact,
                        file,
                        line,
                        cross_lang: false,
                        via_bridge: Some("SCIP".to_string()),
                    });
                    graph.outgoing.entry(from_id.clone()).or_default().push(eidx);
                    graph.incoming.entry(to_id.clone()).or_default().push(eidx);
                    // Also add reverse trait->impl edge for blast-radius/reachability
                    let rev_idx = graph.edges.len();
                    graph.edges.push(Edge {
                        from: to_id.clone(),
                        to: from_id.clone(),
                        kind: EdgeKind::Implements,
                        confidence: Confidence::Exact,
                        file: graph.symbols.get(to_id).map(|s| s.file.clone()).unwrap_or_default(),
                        line: graph.symbols.get(to_id).map(|s| s.span.start_line).unwrap_or(1),
                        cross_lang: false,
                        via_bridge: Some("SCIP".to_string()),
                    });
                    graph.outgoing.entry(to_id.clone()).or_default().push(rev_idx);
                    graph.incoming.entry(from_id.clone()).or_default().push(rev_idx);
                    added_or_upgraded += 1;
                }
            }
        }
    }

    added_or_upgraded
}

// --- Zero-dependency SCIP protobuf wire-format decoder ---

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut res = 0u64;
    let mut shift = 0u32;
    while *pos < buf.len() {
        let b = buf[*pos];
        *pos += 1;
        res |= ((b & 0x7f) as u64) << shift;
        if (b & 0x80) == 0 {
            return Ok(res);
        }
        shift += 7;
        if shift >= 64 {
            bail!("varint overflow in SCIP protobuf");
        }
    }
    bail!("unexpected EOF reading varint in SCIP protobuf")
}

fn read_len_delim<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    let len = read_varint(buf, pos)? as usize;
    if *pos + len > buf.len() {
        bail!("unexpected EOF reading length-delimited field in SCIP protobuf");
    }
    let slice = &buf[*pos..*pos + len];
    *pos += len;
    Ok(slice)
}

fn skip_field(buf: &[u8], pos: &mut usize, wire_type: u64) -> Result<()> {
    match wire_type {
        0 => {
            read_varint(buf, pos)?;
        }
        1 => {
            if *pos + 8 > buf.len() {
                bail!("unexpected EOF skipping 64-bit field");
            }
            *pos += 8;
        }
        2 => {
            read_len_delim(buf, pos)?;
        }
        5 => {
            if *pos + 4 > buf.len() {
                bail!("unexpected EOF skipping 32-bit field");
            }
            *pos += 4;
        }
        _ => bail!("unsupported protobuf wire type {}", wire_type),
    }
    Ok(())
}

fn decode_scip_protobuf(buf: &[u8]) -> Result<ScipIndex> {
    let mut idx = ScipIndex::default();
    let mut pos = 0;
    while pos < buf.len() {
        let tag = read_varint(buf, &mut pos)?;
        let field = tag >> 3;
        let wire = tag & 7;
        if field == 2 && wire == 2 {
            let sub = read_len_delim(buf, &mut pos)?;
            idx.documents.push(decode_document(sub)?);
        } else {
            skip_field(buf, &mut pos, wire)?;
        }
    }
    Ok(idx)
}

fn decode_document(buf: &[u8]) -> Result<ScipDocument> {
    let mut doc = ScipDocument::default();
    let mut pos = 0;
    while pos < buf.len() {
        let tag = read_varint(buf, &mut pos)?;
        let field = tag >> 3;
        let wire = tag & 7;
        match (field, wire) {
            (1, 2) => {
                doc.relative_path = String::from_utf8_lossy(read_len_delim(buf, &mut pos)?).to_string();
            }
            (2, 2) => {
                let sub = read_len_delim(buf, &mut pos)?;
                doc.occurrences.push(decode_occurrence(sub)?);
            }
            (3, 2) => {
                let sub = read_len_delim(buf, &mut pos)?;
                doc.symbols.push(decode_symbol_info(sub)?);
            }
            _ => skip_field(buf, &mut pos, wire)?,
        }
    }
    Ok(doc)
}

fn decode_occurrence(buf: &[u8]) -> Result<ScipOccurrence> {
    let mut occ = ScipOccurrence::default();
    let mut pos = 0;
    while pos < buf.len() {
        let tag = read_varint(buf, &mut pos)?;
        let field = tag >> 3;
        let wire = tag & 7;
        match (field, wire) {
            (1, 0) => occ.range.push(read_varint(buf, &mut pos)? as i32),
            (1, 2) => {
                let packed = read_len_delim(buf, &mut pos)?;
                let mut p = 0;
                while p < packed.len() {
                    occ.range.push(read_varint(packed, &mut p)? as i32);
                }
            }
            (2, 2) => {
                occ.symbol = String::from_utf8_lossy(read_len_delim(buf, &mut pos)?).to_string();
            }
            (3, 0) => occ.symbol_roles = read_varint(buf, &mut pos)? as i32,
            _ => skip_field(buf, &mut pos, wire)?,
        }
    }
    Ok(occ)
}

fn decode_symbol_info(buf: &[u8]) -> Result<ScipSymbolInfo> {
    let mut info = ScipSymbolInfo::default();
    let mut pos = 0;
    while pos < buf.len() {
        let tag = read_varint(buf, &mut pos)?;
        let field = tag >> 3;
        let wire = tag & 7;
        match (field, wire) {
            (1, 2) => {
                info.symbol = String::from_utf8_lossy(read_len_delim(buf, &mut pos)?).to_string();
            }
            (5, 2) => {
                let sub = read_len_delim(buf, &mut pos)?;
                info.relationships.push(decode_relationship(sub)?);
            }
            _ => skip_field(buf, &mut pos, wire)?,
        }
    }
    Ok(info)
}

fn decode_relationship(buf: &[u8]) -> Result<ScipRelationship> {
    let mut rel = ScipRelationship::default();
    let mut pos = 0;
    while pos < buf.len() {
        let tag = read_varint(buf, &mut pos)?;
        let field = tag >> 3;
        let wire = tag & 7;
        match (field, wire) {
            (1, 2) => {
                rel.symbol = String::from_utf8_lossy(read_len_delim(buf, &mut pos)?).to_string();
            }
            (2, 0) => rel.is_reference = read_varint(buf, &mut pos)? != 0,
            (3, 0) => rel.is_implementation = read_varint(buf, &mut pos)? != 0,
            (4, 0) => rel.is_type_definition = read_varint(buf, &mut pos)? != 0,
            _ => skip_field(buf, &mut pos, wire)?,
        }
    }
    Ok(rel)
}
