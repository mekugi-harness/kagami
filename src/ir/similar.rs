use crate::ir::normalize::{normalize_func, NormalizeOptions};
use crate::ir::{IrFunc, IrNode, LoopKind};
use crate::model::{Language, Span};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize)]
pub struct SimilarPair {
    pub score: f64,
    pub match_kind: String,
    pub symbol_a: String,
    pub file_a: String,
    pub lang_a: Language,
    pub span_a: Span,
    pub symbol_b: String,
    pub file_b: String,
    pub lang_b: Language,
    pub span_b: Span,
    pub ir_nodes_a: usize,
    pub ir_nodes_b: usize,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct SimilarOptions {
    pub min_similarity: f64,
    pub min_nodes: usize,
    pub cross_lang: bool,
    pub strict_literals: bool,
    pub limit: usize,
}

struct PreparedFunc<'a> {
    func: &'a IrFunc,
    nodes: usize,
    exact_hash: [u8; 32],
    features: HashMap<u64, u32>,
    minhash: [u64; NUM_HASHES],
}

const NUM_HASHES: usize = 64;
const BANDS: usize = 16;
const ROWS_PER_BAND: usize = NUM_HASHES / BANDS;

pub fn find_similar(funcs: &[IrFunc], opts: &SimilarOptions) -> Vec<SimilarPair> {
    let norm_opts = NormalizeOptions {
        strict_literals: opts.strict_literals,
    };

    let prepared: Vec<PreparedFunc> = funcs
        .iter()
        .filter_map(|f| {
            let norm = normalize_func(f, norm_opts);
            let nodes = norm.node_count();
            if nodes < opts.min_nodes {
                return None;
            }
            let exact_bytes = serde_json::to_vec(&norm).unwrap_or_default();
            let exact_hash = *blake3::hash(&exact_bytes).as_bytes();
            let mut features = HashMap::new();
            extract_features(&norm, &mut features);
            let minhash = compute_minhash(&features);
            Some(PreparedFunc {
                func: f,
                nodes,
                exact_hash,
                features,
                minhash,
            })
        })
        .collect();

    let mut candidate_pairs: HashSet<(usize, usize)> = HashSet::new();

    let mut exact_buckets: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
    for (idx, pf) in prepared.iter().enumerate() {
        exact_buckets.entry(pf.exact_hash).or_default().push(idx);
    }
    for indices in exact_buckets.values() {
        let cap = indices.len().min(32);
        for i in 0..cap {
            for j in (i + 1)..cap {
                candidate_pairs.insert((indices[i], indices[j]));
            }
        }
    }

    for band in 0..BANDS {
        let mut buckets: HashMap<[u64; ROWS_PER_BAND], Vec<usize>> = HashMap::new();
        let offset = band * ROWS_PER_BAND;
        for (idx, pf) in prepared.iter().enumerate() {
            let mut key = [0u64; ROWS_PER_BAND];
            key.copy_from_slice(&pf.minhash[offset..offset + ROWS_PER_BAND]);
            buckets.entry(key).or_default().push(idx);
        }
        for indices in buckets.values() {
            if indices.len() < 2 || indices.len() > 64 {
                continue;
            }
            for i in 0..indices.len() {
                for j in (i + 1)..indices.len() {
                    let (a, b) = if indices[i] < indices[j] {
                        (indices[i], indices[j])
                    } else {
                        (indices[j], indices[i])
                    };
                    candidate_pairs.insert((a, b));
                }
            }
        }
    }

    let mut results = Vec::new();
    for (i, j) in candidate_pairs {
        let a = &prepared[i];
        let b = &prepared[j];
        if !opts.cross_lang && a.func.lang != b.func.lang {
            continue;
        }
        if a.func.file == b.func.file && a.func.span.start_line == b.func.span.start_line {
            continue;
        }
        let (score, match_kind, reason) = if a.exact_hash == b.exact_hash {
            (
                1.0,
                "exact-ir".to_string(),
                "identical canonical IR".to_string(),
            )
        } else {
            let s = weighted_dice(&a.features, &b.features);
            if s < opts.min_similarity {
                continue;
            }
            let diff = (a.nodes as isize - b.nodes as isize).abs();
            (
                (s * 1000.0).round() / 1000.0,
                "near-ir".to_string(),
                format!("structural IR match (node delta: {})", diff),
            )
        };

        if score >= opts.min_similarity {
            results.push(SimilarPair {
                score,
                match_kind,
                symbol_a: a.func.name.clone(),
                file_a: a.func.file.clone(),
                lang_a: a.func.lang,
                span_a: a.func.span,
                symbol_b: b.func.name.clone(),
                file_b: b.func.file.clone(),
                lang_b: b.func.lang,
                span_b: b.func.span,
                ir_nodes_a: a.nodes,
                ir_nodes_b: b.nodes,
                reason,
            });
        }
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (b.ir_nodes_a + b.ir_nodes_b).cmp(&(a.ir_nodes_a + a.ir_nodes_b)))
            .then_with(|| a.file_a.cmp(&b.file_a))
    });
    if opts.limit > 0 && results.len() > opts.limit {
        results.truncate(opts.limit);
    }
    results
}

pub fn pair_similarity(a: &IrFunc, b: &IrFunc, strict_literals: bool) -> f64 {
    let norm_opts = NormalizeOptions { strict_literals };
    let na = normalize_func(a, norm_opts);
    let nb = normalize_func(b, norm_opts);
    let ba = serde_json::to_vec(&na).unwrap_or_default();
    let bb = serde_json::to_vec(&nb).unwrap_or_default();
    if blake3::hash(&ba) == blake3::hash(&bb) {
        return 1.0;
    }
    let mut fa = HashMap::new();
    let mut fb = HashMap::new();
    extract_features(&na, &mut fa);
    extract_features(&nb, &mut fb);
    weighted_dice(&fa, &fb)
}

fn hash_tag(tag: &str, children: &[u64]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(tag.as_bytes());
    for c in children {
        hasher.update(&c.to_le_bytes());
    }
    let out = hasher.finalize();
    u64::from_le_bytes(out.as_bytes()[0..8].try_into().unwrap())
}

fn extract_features(node: &IrNode, out: &mut HashMap<u64, u32>) -> u64 {
    let h = match node {
        IrNode::Seq(items) => {
            let mut ch = Vec::with_capacity(items.len());
            for item in items {
                ch.push(extract_features(item, out));
            }
            for w in ch.windows(2) {
                let bg = hash_tag("SEQ_BIGRAM", w);
                *out.entry(bg).or_insert(0) += 2;
            }
            hash_tag("Seq", &ch)
        }
        IrNode::Loop { kind, body, .. } => {
            let kh = match kind {
                LoopKind::IterRange { start, end, step } => {
                    let s = extract_features(start, out);
                    let e = extract_features(end, out);
                    let st = extract_features(step, out);
                    hash_tag("LoopRange", &[s, e, st])
                }
                LoopKind::IterSeq { seq } => {
                    let s = extract_features(seq, out);
                    hash_tag("LoopSeq", &[s])
                }
                LoopKind::While { cond } => {
                    let c = extract_features(cond, out);
                    hash_tag("LoopWhile", &[c])
                }
                LoopKind::Infinite => hash_tag("LoopInf", &[]),
            };
            let bh = extract_features(body, out);
            *out.entry(kh).or_insert(0) += 3;
            hash_tag("Loop", &[kh, bh])
        }
        IrNode::MultiBranch {
            scrutinee,
            arms,
            default,
        } => {
            let mut ch = Vec::new();
            if let Some(s) = scrutinee {
                ch.push(extract_features(s, out));
            }
            for (k, v) in arms {
                let kh = extract_features(k, out);
                let vh = extract_features(v, out);
                let arm_h = hash_tag("Arm", &[kh, vh]);
                *out.entry(arm_h).or_insert(0) += 2;
                ch.push(arm_h);
            }
            if let Some(d) = default {
                ch.push(extract_features(d, out));
            }
            hash_tag("MultiBranch", &ch)
        }
        IrNode::Branch {
            cond,
            then_br,
            else_br,
        } => {
            let c = extract_features(cond, out);
            let t = extract_features(then_br, out);
            let mut ch = vec![c, t];
            if let Some(e) = else_br {
                ch.push(extract_features(e, out));
            }
            hash_tag("Branch", &ch)
        }
        IrNode::Return(v) => {
            let ch: Vec<u64> = v.iter().map(|x| extract_features(x, out)).collect();
            hash_tag("Return", &ch)
        }
        IrNode::Break => hash_tag("Break", &[]),
        IrNode::Continue => hash_tag("Continue", &[]),
        IrNode::PropagateErr(e) => {
            let eh = extract_features(e, out);
            hash_tag("PropagateErr", &[eh])
        }
        IrNode::Assign { dst, val } => {
            let vh = extract_features(val, out);
            let dh = hash_tag(dst, &[]);
            hash_tag("Assign", &[dh, vh])
        }
        IrNode::Call { callee, args } => {
            let mut ch = vec![hash_tag(callee, &[])];
            for a in args {
                ch.push(extract_features(a, out));
            }
            hash_tag("Call", &ch)
        }
        IrNode::BinOp { op, lhs, rhs } => {
            let l = extract_features(lhs, out);
            let r = extract_features(rhs, out);
            hash_tag(&format!("BinOp:{:?}", op), &[l, r])
        }
        IrNode::UnOp { op, expr } => {
            let e = extract_features(expr, out);
            hash_tag(&format!("UnOp:{:?}", op), &[e])
        }
        IrNode::Member { obj, field } => {
            let o = extract_features(obj, out);
            hash_tag(&format!("Member:{}", field), &[o])
        }
        IrNode::Index { obj, idx } => {
            let o = extract_features(obj, out);
            let i = extract_features(idx, out);
            hash_tag("Index", &[o, i])
        }
        IrNode::Var(v) => hash_tag(&format!("Var:{}", v), &[]),
        IrNode::Lit(l) => hash_tag(&format!("Lit:{:?}", l), &[]),
        IrNode::Opaque(s) => hash_tag(&format!("Opaque:{}", s), &[]),
    };
    *out.entry(h).or_insert(0) += 1;
    h
}

fn compute_minhash(features: &HashMap<u64, u32>) -> [u64; NUM_HASHES] {
    let mut sig = [u64::MAX; NUM_HASHES];
    for &feat in features.keys() {
        for (i, slot) in sig.iter_mut().enumerate() {
            let seed = (i as u64).wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(1);
            let h = splitmix64(feat ^ seed);
            if h < *slot {
                *slot = h;
            }
        }
    }
    sig
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn weighted_dice(a: &HashMap<u64, u32>, b: &HashMap<u64, u32>) -> f64 {
    let sum_a: u64 = a.values().map(|&v| v as u64).sum();
    let sum_b: u64 = b.values().map(|&v| v as u64).sum();
    if sum_a + sum_b == 0 {
        return 0.0;
    }
    let mut inter: u64 = 0;
    for (k, &va) in a {
        if let Some(&vb) = b.get(k) {
            inter += va.min(vb) as u64;
        }
    }
    (2.0 * inter as f64) / (sum_a + sum_b) as f64
}
