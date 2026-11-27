use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
pub struct KagamiConfig {
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub test_paths: Vec<String>,
    #[serde(default)]
    pub dead: DeadConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct DeadConfig {
    pub allowlist: Option<String>,
}

impl KagamiConfig {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(".kagami.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)?;
        let cfg: Self = toml::from_str(&text)?;
        Ok(cfg)
    }

    pub fn build_ignore_globset(&self) -> Result<GlobSet> {
        let mut builder = GlobSetBuilder::new();
        for pat in &self.ignore {
            builder.add(Glob::new(pat)?);
        }
        Ok(builder.build()?)
    }

    pub fn build_test_paths_globset(&self) -> Result<Option<GlobSet>> {
        build_path_filter(&self.test_paths)
    }
}

pub fn load_allowlist(root: &Path, explicit: Option<&Path>) -> HashSet<String> {
    let mut set = HashSet::new();
    let candidate: Option<PathBuf> = explicit
        .map(|p| if p.is_absolute() { p.to_path_buf() } else { root.join(p) })
        .or_else(|| {
            let default = root.join(".deadcode-allowlist");
            if default.exists() { Some(default) } else { None }
        });

    let Some(path) = candidate else {
        return set;
    };
    let Ok(content) = fs::read_to_string(path) else {
        return set;
    };
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        set.insert(trimmed.to_string());
    }
    set
}

pub fn build_path_filter(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let norm = if pat.ends_with('/') {
            format!("{}**", pat)
        } else if !pat.contains('*') && !pat.contains('?') {
            builder.add(Glob::new(pat)?);
            format!("{}/**", pat.trim_end_matches('/'))
        } else {
            pat.clone()
        };
        builder.add(Glob::new(&norm)?);
    }
    Ok(Some(builder.build()?))
}
