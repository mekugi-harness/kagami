use crate::model::Language;
use anyhow::Result;
use globset::GlobSet;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

const META_DIRS: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    ".bzr",
    ".kagami",
    ".worktrees",
    "node_modules",
    "target",
    ".svelte-kit",
    "dist",
    "build",
    "__pycache__",
];

#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub abs_path: PathBuf,
    pub rel_path: String,
    pub lang: Language,
}

pub fn detect_language(path: &Path) -> Option<Language> {
    let file_name = path.file_name()?.to_str()?;
    let lower_name = file_name.to_ascii_lowercase();
    if lower_name == "justfile" || lower_name == ".justfile" || lower_name.ends_with(".just") {
        return Some(Language::Just);
    }
    let ext = path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("go") => Some(Language::Go),
        Some("rs") => Some(Language::Rust),
        Some("ts") | Some("tsx") | Some("mts") | Some("cts") => Some(Language::TypeScript),
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => Some(Language::JavaScript),
        Some("svelte") => Some(Language::Svelte),
        Some("sh") | Some("bash") => Some(Language::Bash),
        Some("json") => {
            let s = path.to_string_lossy();
            if s.contains("schema") && s.contains("json") {
                Some(Language::JsonSchema)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn discover_files(
    root: &Path,
    extra_ignores: &GlobSet,
    path_filter: Option<&GlobSet>,
) -> Result<Vec<DiscoveredFile>> {
    let mut out = Vec::new();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    if META_DIRS.contains(&name) {
                        return false;
                    }
                }
            }
            true
        });

    for dent in builder.build() {
        let Ok(dent) = dent else { continue };
        if !dent.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let abs_path = dent.path().to_path_buf();
        let Ok(rel) = abs_path.strip_prefix(root) else { continue };
        let rel_path = rel.to_string_lossy().replace('\\', "/");

        if extra_ignores.is_match(&rel_path) {
            continue;
        }
        if path_filter.is_none() && (rel_path.contains("/vendor/") || rel_path.starts_with("vendor/") || rel_path.starts_with("third_party/")) {
            continue;
        }
        if let Some(filter) = path_filter {
            if !filter.is_match(&rel_path) {
                continue;
            }
        }
        if let Some(lang) = detect_language(&abs_path) {
            out.push(DiscoveredFile {
                abs_path,
                rel_path,
                lang,
            });
        }
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}
