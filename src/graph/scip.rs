use crate::graph::CodeGraph;
use crate::model::{Confidence, Edge, EdgeKind, SymbolKind};
use anyhow::{bail, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct ScipIndex {
    pub project_root: Option<String>,
    pub documents: Vec<ScipDocument>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ScipMetadata {
    #[serde(default)]
    pub project_root: String,
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
    pub single_line_range: Option<ScipSingleLineRange>,
    #[serde(default)]
    pub multiline_range: Option<ScipMultilineRange>,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub symbol_roles: i32,
}

#[derive(Debug, Default, Deserialize)]
pub struct ScipSingleLineRange {
    #[serde(default)]
    pub line: i32,
    #[serde(default)]
    pub start_character: i32,
    #[serde(default)]
    pub end_character: i32,
}

#[derive(Debug, Default, Deserialize)]
pub struct ScipMultilineRange {
    #[serde(default)]
    pub start_line: i32,
    #[serde(default)]
    pub start_character: i32,
    #[serde(default)]
    pub end_line: i32,
    #[serde(default)]
    pub end_character: i32,
}

impl ScipOccurrence {
    fn start_line_1based(&self) -> Option<usize> {
        if let Some(ref slr) = self.single_line_range {
            return usize::try_from(slr.line).ok().map(|l| l + 1);
        }
        if let Some(ref mlr) = self.multiline_range {
            return usize::try_from(mlr.start_line).ok().map(|l| l + 1);
        }
        self.range.first().and_then(|&l| usize::try_from(l).ok()).map(|l| l + 1)
    }
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
    metadata: Option<ScipMetadata>,
    #[serde(default)]
    documents: Vec<ScipDocument>,
}

/// Load a SCIP index from either binary protobuf (`.scip`) or JSON (`.scip.json`).
pub fn load_scip_file(path: &Path) -> Result<ScipIndex> {
    let bytes = fs::read(path)?;
    if bytes.first().map(|&b| b == b'{').unwrap_or(false) {
        let json_idx: ScipJsonIndex = serde_json::from_slice(&bytes)?;
        let project_root = json_idx
            .metadata
            .map(|m| m.project_root)
            .filter(|s| !s.is_empty());
        return Ok(ScipIndex {
            project_root,
            documents: json_idx.documents,
        });
    }
    decode_scip_protobuf(&bytes)
}

fn rebase_doc_path(doc_rel: &str, project_root: Option<&str>, analysis_root: &Path) -> String {
    let clean_rel = doc_rel.trim_start_matches("./").replace('\\', "/");
    let Some(pr) = project_root else {
        return clean_rel;
    };
    let pr_path_str = pr.strip_prefix("file://").unwrap_or(pr);
    let pr_path = PathBuf::from(pr_path_str);
    let abs_doc = if pr_path.is_absolute() {
        pr_path.join(&clean_rel)
    } else {
        analysis_root.join(&pr_path).join(&clean_rel)
    };
    let norm_root = analysis_root.canonicalize().unwrap_or_else(|_| analysis_root.to_path_buf());
    let norm_doc = abs_doc.canonicalize().unwrap_or(abs_doc);
    if let Ok(stripped) = norm_doc.strip_prefix(&norm_root) {
        return stripped.to_string_lossy().replace('\\', "/");
    }
    if let Ok(stripped) = Path::new(pr_path_str).strip_prefix(analysis_root) {
        let prefix = stripped.to_string_lossy().replace('\\', "/");
        if !prefix.is_empty() {
            return format!("{}/{}", prefix.trim_end_matches('/'), clean_rel);
        }
    }
    clean_rel
}

fn scip_symbol_matches_name(scip_sym: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let (is_macro, expected_name) = match name.strip_suffix('!') {
        Some(base) => (true, base),
        None => (false, name),
    };
    if is_macro != scip_sym.ends_with('!') {
        return false;
    }
    // Strip trailing SCIP descriptor punctuation and empty/numeric method disambiguators
    // (e.g. foo()., foo(+1)., bar(+2)().), while preserving parameter descriptors like (x).
    let mut s = scip_sym.trim_end_matches(|c: char| matches!(c, '.' | '#' | '/' | '!' | ':'));
    loop {
        if let Some(before_close) = s.strip_suffix(')') {
            if let Some(open_idx) = before_close.rfind('(') {
                let inner = &before_close[open_idx + 1..];
                let digits = inner.strip_prefix('+').unwrap_or(inner);
                let is_disambiguator =
                    inner.is_empty() || (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()));
                if is_disambiguator {
                    s = before_close[..open_idx]
                        .trim_end_matches(|c: char| matches!(c, '.' | '#' | '/' | '!' | ':'));
                    continue;
                }
            }
        }
        break;
    }
    let s = s.strip_suffix('\x60').unwrap_or(s);
    let terminal = s
        .rsplit(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or("");
    terminal == expected_name
}

/// Apply a SCIP index overlay onto an existing CodeGraph, adding or upgrading edges to Confidence::Exact.
pub fn apply_scip_overlay(graph: &mut CodeGraph, index: &ScipIndex, analysis_root: &Path) -> usize {
    let mut by_file_syms: HashMap<&str, Vec<(usize, usize, String, String)>> = HashMap::new();
    for sym in graph.symbols.values() {
        by_file_syms.entry(sym.file.as_str()).or_default().push((
            sym.span.start_line,
            sym.span.end_line,
            sym.id.clone(),
            sym.name.clone(),
        ));
    }

    // 1. Resolve non-local SCIP symbols -> CodeGraph symbol IDs via definition occurrences
    let mut scip_to_graph_id: HashMap<String, String> = HashMap::new();
    for doc in &index.documents {
        let rel = rebase_doc_path(&doc.relative_path, index.project_root.as_deref(), analysis_root);
        let Some(file_syms) = by_file_syms.get(rel.as_str()) else {
            continue;
        };
        for occ in &doc.occurrences {
            if (occ.symbol_roles & 1) == 0
                || occ.symbol.is_empty()
                || occ.symbol.starts_with("local ")
            {
                continue;
            }
            let Some(line) = occ.start_line_1based() else {
                continue;
            };
            // Require symbol name token match so untracked fields on the same line never map to enclosing structs
            let matched = file_syms
                .iter()
                .find(|(s, _, _, name)| *s == line && scip_symbol_matches_name(&occ.symbol, name))
                .or_else(|| {
                    file_syms
                        .iter()
                        .find(|(s, e, _, name)| line >= *s && line <= *e && scip_symbol_matches_name(&occ.symbol, name))
                });
            if let Some((_, _, id, _)) = matched {
                scip_to_graph_id.insert(occ.symbol.clone(), id.clone());
            }
        }
    }

    let mut existing: HashMap<(String, String), usize> = HashMap::new();
    for (idx, e) in graph.edges.iter().enumerate() {
        existing.insert((e.from.clone(), e.to.clone()), idx);
    }

    let mut added_or_upgraded = 0usize;
    let mut seen_new: HashSet<(String, String, EdgeKind)> = HashSet::new();
    let mut exact_sites: HashSet<(String, String, usize, String)> = HashSet::new();

    // 2. Process non-local reference occurrences
    for doc in &index.documents {
        let rel = rebase_doc_path(&doc.relative_path, index.project_root.as_deref(), analysis_root);
        let Some(file_syms) = by_file_syms.get(rel.as_str()) else {
            continue;
        };
        for occ in &doc.occurrences {
            if (occ.symbol_roles & 1) != 0
                || occ.symbol.is_empty()
                || occ.symbol.starts_with("local ")
            {
                continue;
            }
            let Some(target_id) = scip_to_graph_id.get(&occ.symbol) else {
                continue;
            };
            let Some(line) = occ.start_line_1based() else {
                continue;
            };
            let enclosing = file_syms
                .iter()
                .filter(|(s, e, _, _)| line >= *s && line <= *e)
                .min_by_key(|(s, e, _, _)| e.saturating_sub(*s));
            let Some((_, _, from_id, _)) = enclosing else {
                continue;
            };
            if from_id == target_id {
                continue;
            }
            if let Some(tname) = graph.symbols.get(target_id).map(|s| s.name.clone()) {
                exact_sites.insert((from_id.clone(), rel.clone(), line, tname));
            }
            if let Some(&eidx) = existing.get(&(from_id.clone(), target_id.clone())) {
                if graph.edges[eidx].confidence < Confidence::Exact {
                    graph.edges[eidx].confidence = Confidence::Exact;
                    added_or_upgraded += 1;
                }
            } else {
                let edge_kind = match graph.symbols.get(target_id).map(|s| s.kind) {
                    Some(SymbolKind::Function | SymbolKind::Method | SymbolKind::Recipe) => EdgeKind::Calls,
                    _ => EdgeKind::TypeRef,
                };
                if seen_new.insert((from_id.clone(), target_id.clone(), edge_kind)) {
                    let cross_lang = match (graph.symbols.get(from_id), graph.symbols.get(target_id)) {
                        (Some(a), Some(b)) => a.lang != b.lang,
                        _ => false,
                    };
                    let eidx = graph.edges.len();
                    graph.edges.push(Edge {
                        from: from_id.clone(),
                        to: target_id.clone(),
                        kind: edge_kind,
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
        }

        // 3. Process implementation relationships
        for sinfo in &doc.symbols {
            if sinfo.symbol.starts_with("local ") {
                continue;
            }
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

    // 4. Prune competing lower-confidence guessed edges at sites where SCIP established Exact edges
    if !exact_sites.is_empty() {
        let before = graph.edges.len();
        graph.edges.retain(|e| {
            if e.confidence == Confidence::Exact || e.via_bridge.is_some() {
                return true;
            }
            let Some(tname) = graph.symbols.get(&e.to).map(|s| s.name.as_str()) else {
                return true;
            };
            !exact_sites.contains(&(e.from.clone(), e.file.clone(), e.line, tname.to_string()))
        });
        if graph.edges.len() != before {
            graph.outgoing.clear();
            graph.incoming.clear();
            for (idx, e) in graph.edges.iter().enumerate() {
                graph.outgoing.entry(e.from.clone()).or_default().push(idx);
                graph.incoming.entry(e.to.clone()).or_default().push(idx);
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
    let len_u64 = read_varint(buf, pos)?;
    let len = usize::try_from(len_u64).map_err(|_| anyhow::anyhow!("SCIP length overflow"))?;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| anyhow::anyhow!("SCIP length overflow"))?;
    if end > buf.len() {
        bail!("unexpected EOF reading length-delimited field in SCIP protobuf");
    }
    let slice = &buf[*pos..end];
    *pos = end;
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
        match (field, wire) {
            (1, 2) => {
                let sub = read_len_delim(buf, &mut pos)?;
                let pr = decode_metadata_project_root(sub)?;
                if !pr.is_empty() {
                    idx.project_root = Some(pr);
                }
            }
            (2, 2) => {
                let sub = read_len_delim(buf, &mut pos)?;
                idx.documents.push(decode_document(sub)?);
            }
            _ => skip_field(buf, &mut pos, wire)?,
        }
    }
    Ok(idx)
}

fn decode_metadata_project_root(buf: &[u8]) -> Result<String> {
    let mut pos = 0;
    let mut project_root = String::new();
    while pos < buf.len() {
        let tag = read_varint(buf, &mut pos)?;
        let field = tag >> 3;
        let wire = tag & 7;
        // SCIP Metadata: field 1 = version, field 2 = tool_info, field 3 = project_root
        if field == 3 && wire == 2 {
            project_root = String::from_utf8_lossy(read_len_delim(buf, &mut pos)?).to_string();
        } else {
            skip_field(buf, &mut pos, wire)?;
        }
    }
    Ok(project_root)
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

fn decode_typed_single_line_range(buf: &[u8]) -> Result<Vec<i32>> {
    let mut pos = 0;
    let (mut line, mut start_col, mut end_col) = (0i32, 0i32, 0i32);
    while pos < buf.len() {
        let tag = read_varint(buf, &mut pos)?;
        let field = tag >> 3;
        let wire = tag & 7;
        match (field, wire) {
            (1, 0) => line = read_varint(buf, &mut pos)? as i32,
            (2, 0) => start_col = read_varint(buf, &mut pos)? as i32,
            (3, 0) => end_col = read_varint(buf, &mut pos)? as i32,
            _ => skip_field(buf, &mut pos, wire)?,
        }
    }
    Ok(vec![line, start_col, end_col])
}

fn decode_typed_multiline_range(buf: &[u8]) -> Result<Vec<i32>> {
    let mut pos = 0;
    let (mut start_line, mut start_col, mut end_line, mut end_col) = (0i32, 0i32, 0i32, 0i32);
    while pos < buf.len() {
        let tag = read_varint(buf, &mut pos)?;
        let field = tag >> 3;
        let wire = tag & 7;
        match (field, wire) {
            (1, 0) => start_line = read_varint(buf, &mut pos)? as i32,
            (2, 0) => start_col = read_varint(buf, &mut pos)? as i32,
            (3, 0) => end_line = read_varint(buf, &mut pos)? as i32,
            (4, 0) => end_col = read_varint(buf, &mut pos)? as i32,
            _ => skip_field(buf, &mut pos, wire)?,
        }
    }
    Ok(vec![start_line, start_col, end_line, end_col])
}

fn decode_occurrence(buf: &[u8]) -> Result<ScipOccurrence> {
    let mut occ = ScipOccurrence::default();
    let mut typed_range: Option<Vec<i32>> = None;
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
            (8, 2) => {
                let sub = read_len_delim(buf, &mut pos)?;
                typed_range = Some(decode_typed_single_line_range(sub)?);
            }
            (9, 2) => {
                let sub = read_len_delim(buf, &mut pos)?;
                typed_range = Some(decode_typed_multiline_range(sub)?);
            }
            _ => skip_field(buf, &mut pos, wire)?,
        }
    }
    if let Some(tr) = typed_range {
        occ.range = tr;
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
            (4, 2) => {
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
