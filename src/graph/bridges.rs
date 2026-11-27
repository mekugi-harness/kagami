use crate::model::{
    BridgeHint, BridgeKind, Confidence, Edge, EdgeKind, FileExtract, Language, Symbol, SymbolKind,
};
use std::collections::{HashMap, HashSet};

pub fn materialize_bridges(
    extracts: &[FileExtract],
    symbols: &HashMap<String, Symbol>,
    edges: &mut Vec<Edge>,
) {
    // Group bridge hints by (BridgeKind, normalized key)
    let mut groups: HashMap<(BridgeKind, String), Vec<(BridgeHint, Language, String)>> =
        HashMap::new();

    for ext in extracts {
        for hint in &ext.bridges {
            if !symbols.contains_key(&hint.from_symbol_id) {
                continue;
            }
            let norm_key = normalize_bridge_key(hint.bridge_kind, &hint.key);
            if norm_key.is_empty() {
                continue;
            }
            groups
                .entry((hint.bridge_kind, norm_key))
                .or_default()
                .push((hint.clone(), ext.lang, ext.file.clone()));
        }
    }

    // Also auto-register Go/Rust binary entrypoints (main functions) as ExecTarget producers
    for sym in symbols.values() {
        if sym.is_entrypoint && sym.name == "main" {
            if let Some(dir) = sym.file.rsplit_once('/').map(|(d, _)| d) {
                groups
                    .entry((BridgeKind::ExecTarget, dir.to_string()))
                    .or_default()
                    .push((
                        BridgeHint {
                            from_symbol_id: sym.id.clone(),
                            bridge_kind: BridgeKind::ExecTarget,
                            key: dir.to_string(),
                            role: crate::model::BridgeRole::Producer,
                            line: sym.span.start_line,
                            confidence: Confidence::Resolved,
                        },
                        sym.lang,
                        sym.file.clone(),
                    ));
                if let Some(leaf) = dir.rsplit('/').next() {
                    if !matches!(leaf, "src" | "bin" | "cmd") {
                        groups
                            .entry((BridgeKind::ExecTarget, leaf.to_string()))
                            .or_default()
                            .push((
                                BridgeHint {
                                    from_symbol_id: sym.id.clone(),
                                    bridge_kind: BridgeKind::ExecTarget,
                                    key: leaf.to_string(),
                                    role: crate::model::BridgeRole::Producer,
                                    line: sym.span.start_line,
                                    confidence: Confidence::Heuristic,
                                },
                                sym.lang,
                                sym.file.clone(),
                            ));
                    }
                }
            }
        }
    }

    let mut seen: HashSet<(String, String, EdgeKind)> = HashSet::new();

    for ((kind, key), members) in groups {
        if members.len() < 2 || members.len() > 64 {
            continue;
        }
        // Only link across distinct files (and primarily cross-language or exec/wire contracts)
        let edge_kind = match kind {
            BridgeKind::WireContract => EdgeKind::WireRpc,
            BridgeKind::ExecTarget => EdgeKind::CliExec,
            BridgeKind::EnvKey => EdgeKind::EnvRef,
        };
        let bridge_label = format!("{:?}:{}", kind, key);

        for i in 0..members.len() {
            for j in 0..members.len() {
                if i == j {
                    continue;
                }
                let (ha, la, fa) = &members[i];
                let (hb, lb, _fb) = &members[j];
                if ha.from_symbol_id == hb.from_symbol_id {
                    continue;
                }
                // For WireContract/EnvKey, connect cross-language pairs (or schema <-> code)
                let cross_lang = la != lb;
                if !cross_lang && kind != BridgeKind::ExecTarget {
                    continue;
                }
                // Respect producer -> consumer direction when distinct
                use crate::model::BridgeRole;
                let dir_ok = match (ha.role, hb.role) {
                    (BridgeRole::Consumer, BridgeRole::Producer)
                    | (BridgeRole::Consumer, BridgeRole::Both)
                    | (BridgeRole::Both, BridgeRole::Producer)
                    | (BridgeRole::Both, BridgeRole::Both) => true,
                    _ => false,
                };
                if !dir_ok {
                    continue;
                }
                if seen.insert((ha.from_symbol_id.clone(), hb.from_symbol_id.clone(), edge_kind)) {
                    edges.push(Edge {
                        from: ha.from_symbol_id.clone(),
                        to: hb.from_symbol_id.clone(),
                        kind: edge_kind,
                        confidence: ha.confidence.min(hb.confidence),
                        file: fa.clone(),
                        line: ha.line,
                        cross_lang,
                        via_bridge: Some(bridge_label.clone()),
                    });
                }
            }
        }
    }
}

fn normalize_bridge_key(kind: BridgeKind, raw: &str) -> String {
    let t = raw.trim();
    match kind {
        BridgeKind::WireContract => t.to_string(),
        BridgeKind::ExecTarget => t.trim_start_matches("./").to_string(),
        BridgeKind::EnvKey => t.to_string(),
    }
}

pub fn is_virtual_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::WireContract | SymbolKind::ExecTarget | SymbolKind::EnvKey
    )
}
