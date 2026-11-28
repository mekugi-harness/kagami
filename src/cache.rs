use crate::graph::InterfaceImpl;
use crate::model::{Confidence, Edge, EdgeKind, FileExtract, Language, Symbol};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const PARSER_VERSION: u32 = 13;

const CACHE_MAGIC: &[u8; 8] = b"KAGAMI01";
const MANIFEST_MAGIC: &[u8; 8] = b"KAGAMIM1";
const GRAPH_MAGIC: &[u8; 8] = b"KAGAMIG1";
const HEADER_LEN: usize = 12; // 8-byte magic + 4-byte LE version

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FileStat {
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    pub size: u64,
}

impl FileStat {
    pub fn from_path(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        let size = meta.len();
        let modified = meta.modified().ok()?;
        let (mtime_secs, mtime_nanos) = match modified.duration_since(UNIX_EPOCH) {
            Ok(dur) => (dur.as_secs() as i64, dur.subsec_nanos()),
            Err(err) => {
                let dur = err.duration();
                (-(dur.as_secs() as i64), dur.subsec_nanos())
            }
        };
        Some(Self {
            mtime_secs,
            mtime_nanos,
            size,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFile {
    pub stat: FileStat,
    pub extract: FileExtract,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CacheData {
    pub version: u32,
    pub files: HashMap<String, CachedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub rel_path: String,
    pub lang: Language,
    pub stat: FileStat,
    pub blake3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoManifestEntry {
    pub rel_path: String,
    pub stat: FileStat,
    pub blake3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphManifest {
    pub version: u32,
    pub fingerprint: [u8; 32],
    pub files: Vec<ManifestEntry>,
    pub cargo_manifests: Vec<CargoManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactEdge {
    pub from: u32,
    pub to: u32,
    pub kind: EdgeKind,
    pub confidence: Confidence,
    pub file_idx: u32,
    pub line: u32,
    pub cross_lang: bool,
    pub via_bridge: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactInterfaceImpl {
    pub receiver_type_idx: u32,
    pub interface_idx: u32,
    pub method_indices: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSnapshot {
    pub version: u32,
    pub fingerprint: [u8; 32],
    pub symbols: Vec<Symbol>,
    pub files: Vec<String>,
    pub bridges: Vec<String>,
    pub edges: Vec<CompactEdge>,
    pub interface_impls: Vec<CompactInterfaceImpl>,
}

pub struct GraphParts {
    pub symbols: HashMap<String, Symbol>,
    pub edges: Vec<Edge>,
    pub outgoing: HashMap<String, Vec<usize>>,
    pub incoming: HashMap<String, Vec<usize>>,
    pub interface_impls: Vec<InterfaceImpl>,
}

impl GraphSnapshot {
    pub fn from_graph_parts(
        fingerprint: [u8; 32],
        symbols: &HashMap<String, Symbol>,
        edges: &[Edge],
        interface_impls: &[InterfaceImpl],
    ) -> Option<Self> {
        let mut sym_vec: Vec<Symbol> = symbols.values().cloned().collect();
        sym_vec.sort_by(|a, b| a.id.cmp(&b.id));
        let sym_idx: rustc_hash::FxHashMap<&str, u32> = sym_vec
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.as_str(), i as u32))
            .collect();

        let mut file_map: rustc_hash::FxHashMap<&str, u32> = rustc_hash::FxHashMap::default();
        let mut files: Vec<String> = Vec::new();
        let mut bridge_map: rustc_hash::FxHashMap<&str, u32> = rustc_hash::FxHashMap::default();
        let mut bridges: Vec<String> = Vec::new();

        let mut compact_edges = Vec::with_capacity(edges.len());
        for e in edges {
            let &from = sym_idx.get(e.from.as_str())?;
            let &to = sym_idx.get(e.to.as_str())?;
            let file_idx = match file_map.get(e.file.as_str()) {
                Some(&idx) => idx,
                None => {
                    let idx = files.len() as u32;
                    file_map.insert(e.file.as_str(), idx);
                    files.push(e.file.clone());
                    idx
                }
            };
            let via_bridge = e.via_bridge.as_deref().map(|b| match bridge_map.get(b) {
                Some(&idx) => idx,
                None => {
                    let idx = bridges.len() as u32;
                    bridge_map.insert(b, idx);
                    bridges.push(b.to_string());
                    idx
                }
            });
            compact_edges.push(CompactEdge {
                from,
                to,
                kind: e.kind,
                confidence: e.confidence,
                file_idx,
                line: e.line as u32,
                cross_lang: e.cross_lang,
                via_bridge,
            });
        }

        let mut compact_ifaces = Vec::with_capacity(interface_impls.len());
        for imp in interface_impls {
            let &receiver_type_idx = sym_idx.get(imp.receiver_type_id.as_str())?;
            let &interface_idx = sym_idx.get(imp.interface_id.as_str())?;
            let mut method_indices = Vec::with_capacity(imp.method_ids.len());
            for mid in &imp.method_ids {
                method_indices.push(*sym_idx.get(mid.as_str())?);
            }
            compact_ifaces.push(CompactInterfaceImpl {
                receiver_type_idx,
                interface_idx,
                method_indices,
            });
        }

        Some(Self {
            version: PARSER_VERSION,
            fingerprint,
            symbols: sym_vec,
            files,
            bridges,
            edges: compact_edges,
            interface_impls: compact_ifaces,
        })
    }

    pub fn into_graph_parts(self) -> Option<GraphParts> {
        let n_syms = self.symbols.len();
        let sym_ids: Vec<String> = self.symbols.iter().map(|s| s.id.clone()).collect();
        let mut out_vecs: Vec<Vec<usize>> = vec![Vec::new(); n_syms];
        let mut in_vecs: Vec<Vec<usize>> = vec![Vec::new(); n_syms];
        let mut edges = Vec::with_capacity(self.edges.len());
        for (idx, ce) in self.edges.into_iter().enumerate() {
            let from_idx = ce.from as usize;
            let to_idx = ce.to as usize;
            let from = sym_ids.get(from_idx)?.clone();
            let to = sym_ids.get(to_idx)?.clone();
            let file = self.files.get(ce.file_idx as usize)?.clone();
            let via_bridge = match ce.via_bridge {
                Some(idx) => Some(self.bridges.get(idx as usize)?.clone()),
                None => None,
            };
            out_vecs[from_idx].push(idx);
            in_vecs[to_idx].push(idx);
            edges.push(Edge {
                from,
                to,
                kind: ce.kind,
                confidence: ce.confidence,
                file,
                line: ce.line as usize,
                cross_lang: ce.cross_lang,
                via_bridge,
            });
        }

        let mut interface_impls = Vec::with_capacity(self.interface_impls.len());
        for ci in self.interface_impls {
            let receiver_type_id = sym_ids.get(ci.receiver_type_idx as usize)?.clone();
            let interface_id = sym_ids.get(ci.interface_idx as usize)?.clone();
            let mut method_ids = Vec::with_capacity(ci.method_indices.len());
            for idx in ci.method_indices {
                method_ids.push(sym_ids.get(idx as usize)?.clone());
            }
            interface_impls.push(InterfaceImpl {
                receiver_type_id,
                interface_id,
                method_ids,
            });
        }

        let mut symbols = HashMap::with_capacity(self.symbols.len());
        for sym in self.symbols {
            symbols.insert(sym.id.clone(), sym);
        }

        let mut outgoing = HashMap::with_capacity(n_syms);
        let mut incoming = HashMap::with_capacity(n_syms);
        for (i, id) in sym_ids.into_iter().enumerate() {
            if !out_vecs[i].is_empty() {
                outgoing.insert(id.clone(), std::mem::take(&mut out_vecs[i]));
            }
            if !in_vecs[i].is_empty() {
                incoming.insert(id, std::mem::take(&mut in_vecs[i]));
            }
        }

        Some(GraphParts {
            symbols,
            edges,
            outgoing,
            incoming,
            interface_impls,
        })
    }
}

pub struct CacheStore {
    dir: PathBuf,
    path: PathBuf,
    manifest_path: PathBuf,
    graph_path: PathBuf,
    legacy_json_path: PathBuf,
}

impl CacheStore {
    pub fn new(root: &Path) -> Self {
        let dir = root.join(".kagami");
        Self {
            path: dir.join("cache.bin"),
            manifest_path: dir.join("manifest.bin"),
            graph_path: dir.join("graph.bin"),
            legacy_json_path: dir.join("cache.json"),
            dir,
        }
    }

    pub fn cache_path(&self) -> &Path {
        &self.path
    }

    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    pub fn graph_path(&self) -> &Path {
        &self.graph_path
    }

    fn decode_framed<T: for<'de> Deserialize<'de>>(bytes: &[u8], magic: &[u8; 8]) -> Option<T> {
        if bytes.len() < HEADER_LEN || &bytes[..8] != magic {
            return None;
        }
        let ver = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
        if ver != PARSER_VERSION {
            return None;
        }
        postcard::from_bytes(&bytes[HEADER_LEN..]).ok()
    }

    fn encode_framed<T: Serialize>(value: &T, magic: &[u8; 8]) -> Option<Vec<u8>> {
        let payload = postcard::to_stdvec(value).ok()?;
        let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
        buf.extend_from_slice(magic);
        buf.extend_from_slice(&PARSER_VERSION.to_le_bytes());
        buf.extend_from_slice(&payload);
        Some(buf)
    }

    fn write_atomic(&self, target: &Path, bytes: &[u8]) {
        if let Ok(meta) = fs::symlink_metadata(&self.dir)
            && meta.file_type().is_symlink()
        {
            return;
        }
        if fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        if let Ok(meta) = fs::symlink_metadata(target)
            && meta.file_type().is_symlink()
        {
            return;
        }
        let tmp = target.with_extension(format!("bin.{}.tmp", std::process::id()));
        if fs::symlink_metadata(&tmp)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return;
        }
        if fs::write(&tmp, bytes).is_ok() && fs::rename(&tmp, target).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }

    pub fn cleanup_legacy_json(&self) {
        if let Ok(meta) = fs::symlink_metadata(&self.legacy_json_path)
            && !meta.file_type().is_symlink()
            && meta.file_type().is_file()
        {
            let _ = fs::remove_file(&self.legacy_json_path);
        }
    }

    pub fn load(&self) -> CacheData {
        let Ok(bytes) = fs::read(&self.path) else {
            return CacheData {
                version: PARSER_VERSION,
                files: HashMap::new(),
            };
        };
        match Self::decode_framed::<CacheData>(&bytes, CACHE_MAGIC) {
            Some(data) if data.version == PARSER_VERSION => data,
            _ => CacheData {
                version: PARSER_VERSION,
                files: HashMap::new(),
            },
        }
    }

    pub fn save(&self, data: &CacheData) {
        if let Some(bytes) = Self::encode_framed(data, CACHE_MAGIC) {
            self.write_atomic(&self.path, &bytes);
            self.cleanup_legacy_json();
        }
    }

    pub fn load_manifest(&self) -> Option<GraphManifest> {
        let bytes = fs::read(&self.manifest_path).ok()?;
        let manifest: GraphManifest = Self::decode_framed(&bytes, MANIFEST_MAGIC)?;
        (manifest.version == PARSER_VERSION).then_some(manifest)
    }

    pub fn save_manifest(&self, manifest: &GraphManifest) {
        if let Some(bytes) = Self::encode_framed(manifest, MANIFEST_MAGIC) {
            self.write_atomic(&self.manifest_path, &bytes);
        }
    }

    pub fn load_graph(&self) -> Option<GraphSnapshot> {
        let bytes = fs::read(&self.graph_path).ok()?;
        let snap: GraphSnapshot = Self::decode_framed(&bytes, GRAPH_MAGIC)?;
        (snap.version == PARSER_VERSION).then_some(snap)
    }

    pub fn save_graph(&self, snapshot: &GraphSnapshot) {
        if let Some(bytes) = Self::encode_framed(snapshot, GRAPH_MAGIC) {
            self.write_atomic(&self.graph_path, &bytes);
            self.cleanup_legacy_json();
        }
    }
}
