use crate::model::FileExtract;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const PARSER_VERSION: u32 = 1;

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
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec(data) {
            let _ = fs::write(&self.path, bytes);
        }
    }
}
