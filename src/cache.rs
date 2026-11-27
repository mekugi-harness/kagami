use crate::model::FileExtract;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const PARSER_VERSION: u32 = 5;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CacheData {
    pub version: u32,
    pub files: HashMap<String, FileExtract>,
}

pub struct CacheStore {
    path: PathBuf,
}

impl CacheStore {
    pub fn new(root: &Path) -> Self {
        Self {
            path: root.join(".kagami").join("cache.json"),
        }
    }

    pub fn load(&self) -> CacheData {
        let Ok(bytes) = fs::read(&self.path) else {
            return CacheData {
                version: PARSER_VERSION,
                files: HashMap::new(),
            };
        };
        match serde_json::from_slice::<CacheData>(&bytes) {
            Ok(data) if data.version == PARSER_VERSION => data,
            _ => CacheData {
                version: PARSER_VERSION,
                files: HashMap::new(),
            },
        }
    }

    pub fn save(&self, data: &CacheData) {
        if let Some(parent) = self.path.parent() {
            if let Ok(meta) = fs::symlink_metadata(parent) {
                if meta.file_type().is_symlink() {
                    return;
                }
            }
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(meta) = fs::symlink_metadata(&self.path) {
            if meta.file_type().is_symlink() {
                return;
            }
        }
        if let Ok(bytes) = serde_json::to_vec(data) {
            let tmp = self.path.with_extension("json.tmp");
            if fs::symlink_metadata(&tmp).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                return;
            }
            if fs::write(&tmp, bytes).is_ok() {
                let _ = fs::rename(&tmp, &self.path);
            }
        }
    }
}
