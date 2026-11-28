use crate::graph::InterfaceImpl;
use crate::model::{
    Confidence, Edge, EdgeKind, FileExtract, Language, ReceiverStep, RustUseItem, Symbol,
    SymbolKind,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RustSymFilter {
    TypesOnly,
    ValuesOnly,
    TypesAndConsts,
}

impl RustSymFilter {
    fn matches(self, kind: SymbolKind) -> bool {
        let is_type = matches!(
            kind,
            SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Type | SymbolKind::Trait
        );
        let is_const = matches!(kind, SymbolKind::Const | SymbolKind::Var);
        let is_val = matches!(kind, SymbolKind::Function | SymbolKind::Const | SymbolKind::Var);
        match self {
            RustSymFilter::TypesOnly => is_type,
            RustSymFilter::ValuesOnly => is_val,
            RustSymFilter::TypesAndConsts => is_type || is_const,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CargoCrateInfo {
    /// Directory containing the crate's root module (e.g. "codex-rs/app-server-daemon/src").
    pub src_dir: String,
    /// Normalized library entry file path relative to repo root (e.g. "codex-rs/app-server-daemon/src/lib.rs").
    pub lib_file: String,
    /// Explicit Cargo target root files declared in Cargo.toml ([lib], [[bin]], [[example]], [[test]], [[bench]]).
    pub target_roots: Vec<String>,
    /// Manifest-relative auto-discovered Cargo target directories (e.g. "src/bin", "examples", "tests", "benches").
    pub target_root_dirs: Vec<String>,
}

/// Discover Rust crate names from Cargo.toml files ([lib].name and [package].name)
/// and map each Rust crate identifier (with '-' replaced by '_') to its crate root info.
/// Return the sorted list of relative `Cargo.toml` paths discovered in ancestor directories
/// of all Rust files. Used to detect Cargo manifest changes for graph cache invalidation.
pub fn collect_cargo_manifest_paths(root: &Path, rust_rel_paths: &[&str]) -> Vec<String> {
    let mut checked_dirs: FxHashSet<String> = FxHashSet::default();
    let mut found: Vec<String> = Vec::new();

    for rel in rust_rel_paths {
        let mut dir = rel
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();
        loop {
            if !checked_dirs.insert(dir.clone()) {
                break;
            }
            let manifest_rel = if dir.is_empty() {
                "Cargo.toml".to_string()
            } else {
                format!("{}/Cargo.toml", dir)
            };
            if root.join(&manifest_rel).is_file() {
                found.push(manifest_rel);
            }
            if dir.is_empty() {
                break;
            }
            dir = dir
                .rsplit_once('/')
                .map(|(parent, _)| parent.to_string())
                .unwrap_or_default();
        }
    }

    found.sort();
    found
}

pub fn discover_cargo_crates(
    root: &Path,
    extracts: &[FileExtract],
) -> HashMap<String, CargoCrateInfo> {
    let mut crate_roots: HashMap<String, CargoCrateInfo> = HashMap::new();
    let mut checked_dirs: HashSet<String> = HashSet::new();
    let mut parsed_manifests: HashSet<String> = HashSet::new();

    for ext in extracts {
        if ext.lang != Language::Rust {
            continue;
        }
        let mut dir = ext
            .file
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();
        loop {
            if !checked_dirs.insert(dir.clone()) {
                break;
            }
            let manifest_path = if dir.is_empty() {
                root.join("Cargo.toml")
            } else {
                root.join(&dir).join("Cargo.toml")
            };
            if manifest_path.is_file()
                && parsed_manifests.insert(dir.clone())
                && let Ok(content) = fs::read_to_string(&manifest_path)
                && let Ok(val) = toml::from_str::<toml::Value>(&content)
            {
                let lib_name = val
                    .get("lib")
                    .and_then(|t| t.get("name"))
                    .and_then(|v| v.as_str());
                let pkg_name = val
                    .get("package")
                    .and_then(|t| t.get("name"))
                    .and_then(|v| v.as_str());
                if lib_name.is_some() || pkg_name.is_some() {
                    let lib_path = val
                        .get("lib")
                        .and_then(|t| t.get("path"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("src/lib.rs");
                    let lib_file = normalize_rel_path_join(&dir, lib_path);
                    let mut target_roots = Vec::new();
                    if val.get("lib").and_then(|t| t.get("path")).is_some() {
                        push_unique(&mut target_roots, lib_file.clone());
                    }
                    for (section, default_dir) in [
                        ("bin", "src/bin"),
                        ("example", "examples"),
                        ("test", "tests"),
                        ("bench", "benches"),
                    ] {
                        if let Some(items) = val.get(section).and_then(|v| v.as_array()) {
                            for item in items {
                                if let Some(p) = item.get("path").and_then(|v| v.as_str()) {
                                    let full = normalize_rel_path_join(&dir, p);
                                    if !full.is_empty() {
                                        push_unique(&mut target_roots, full);
                                    }
                                } else if let Some(name) = item.get("name").and_then(|v| v.as_str())
                                {
                                    for suffix in [".rs", "/main.rs"] {
                                        let rel = format!("{}/{}{}", default_dir, name, suffix);
                                        push_unique(
                                            &mut target_roots,
                                            normalize_rel_path_join(&dir, &rel),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    let pkg_tbl = val.get("package");
                    let mut target_root_dirs = Vec::new();
                    for (flag, rel_dir) in [
                        ("autobins", "src/bin"),
                        ("autoexamples", "examples"),
                        ("autotests", "tests"),
                        ("autobenches", "benches"),
                    ] {
                        let enabled = pkg_tbl
                            .and_then(|t| t.get(flag))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                        if enabled {
                            push_unique(
                                &mut target_root_dirs,
                                normalize_rel_path_join(&dir, rel_dir),
                            );
                        }
                    }
                    let src_root = lib_file
                        .rsplit_once('/')
                        .map(|(d, _)| d.to_string())
                        .unwrap_or_default();
                    let info = CargoCrateInfo {
                        src_dir: src_root,
                        lib_file,
                        target_roots,
                        target_root_dirs,
                    };
                    if let Some(ln) = lib_name {
                        crate_roots.insert(ln.replace('-', "_"), info.clone());
                    }
                    if let Some(pn) = pkg_name {
                        crate_roots.entry(pn.replace('-', "_")).or_insert(info);
                    }
                    // Stop at the nearest package manifest.
                    break;
                }
            }
            if dir.is_empty() {
                break;
            }
            dir = dir
                .rsplit_once('/')
                .map(|(parent, _)| parent.to_string())
                .unwrap_or_default();
        }
    }

    crate_roots
}

/// Returns true when file is a Cargo crate root (lib.rs, main.rs,
/// explicit Cargo.toml target path, or direct child of a manifest-relative target dir).
fn is_rust_crate_root_file(
    file: &str,
    target_roots: &FxHashSet<String>,
    target_root_dirs: &FxHashSet<String>,
) -> bool {
    let file_dir = file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let file_name = file.rsplit('/').next().unwrap_or(file);
    matches!(file_name, "lib.rs" | "main.rs")
        || target_roots.contains(file)
        || target_root_dirs.contains(file_dir)
}

/// Computes candidate base directories for an external mod declaration inside file
/// within inline module scope mod_scope.
fn rust_mod_base_dirs(
    file: &str,
    mod_scope: &str,
    has_explicit_path: bool,
    target_roots: &FxHashSet<String>,
    target_root_dirs: &FxHashSet<String>,
    path_loaded_files: &FxHashSet<String>,
) -> Vec<String> {
    let file_dir = file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let file_name = file.rsplit('/').next().unwrap_or(file);
    let is_dir_mod = file_name == "mod.rs"
        || is_rust_crate_root_file(file, target_roots, target_root_dirs)
        || path_loaded_files.contains(file);
    let mod_root_dir = if is_dir_mod {
        file_dir.to_string()
    } else {
        let stem = file_name.strip_suffix(".rs").unwrap_or(file_name);
        if file_dir.is_empty() {
            stem.to_string()
        } else {
            format!("{}/{}", file_dir, stem)
        }
    };
    let scoped_dir = if mod_scope.is_empty() {
        mod_root_dir.clone()
    } else {
        let scope_rel = mod_scope.replace("::", "/");
        if mod_root_dir.is_empty() {
            scope_rel
        } else {
            format!("{}/{}", mod_root_dir, scope_rel)
        }
    };
    if has_explicit_path && mod_scope.is_empty() {
        // Top-level `#[path = "..."] mod foo;` resolves relative to the declaring file's directory.
        return vec![file_dir.to_string()];
    }
    vec![scoped_dir]
}

/// Discover Rust files loaded via explicit `#[path = "..."]` (fixed-point up to depth 4)
/// since Rust resolves children of path-overridden modules relative to their containing directory.
fn discover_path_loaded_files(
    extracts: &[FileExtract],
    known_rust_files: &FxHashSet<&str>,
    target_roots: &FxHashSet<String>,
    target_root_dirs: &FxHashSet<String>,
    include_test: bool,
) -> FxHashSet<String> {
    let mut path_loaded_files: FxHashSet<String> = FxHashSet::default();
    for _ in 0..4 {
        let before = path_loaded_files.len();
        for ext in extracts.iter() {
            if ext.lang != Language::Rust || ext.rust_mod_paths.is_empty() {
                continue;
            }
            if !include_test && ext.is_test_file {
                continue;
            }
            for mp in &ext.rust_mod_paths {
                if !include_test && mp.is_test {
                    continue;
                }
                let Some(ref raw_path) = mp.path else {
                    continue;
                };
                for bd in rust_mod_base_dirs(
                    &ext.file,
                    &mp.mod_scope,
                    true,
                    target_roots,
                    target_root_dirs,
                    &path_loaded_files,
                ) {
                    let cand = normalize_rel_path_join(&bd, raw_path);
                    if known_rust_files.contains(cand.as_str()) {
                        path_loaded_files.insert(cand);
                    }
                }
            }
        }
        if path_loaded_files.len() == before {
            break;
        }
    }
    path_loaded_files
}

/// Propagate `#[cfg(test)]` transitively from parent Rust files to external child `mod` files
/// (both `mod foo;` and `#[path = "..."] mod foo;`). Any Rust module file that has incoming `mod`
/// declarations and is not reachable from any non-test crate/file root via non-test `mod` edges
/// is marked test-only.
pub fn propagate_rust_cfg_test_modules(
    extracts: &mut [FileExtract],
    crate_roots: &HashMap<String, CargoCrateInfo>,
) {
    let target_roots: FxHashSet<String> = crate_roots
        .values()
        .flat_map(|info| info.target_roots.iter().cloned())
        .collect();
    let target_root_dirs: FxHashSet<String> = crate_roots
        .values()
        .flat_map(|info| info.target_root_dirs.iter().cloned())
        .collect();
    let known_rust_files: FxHashSet<String> = extracts
        .iter()
        .filter(|e| e.lang == Language::Rust)
        .map(|e| e.file.clone())
        .collect();
    if known_rust_files.is_empty() {
        return;
    }
    let known_refs: FxHashSet<&str> = known_rust_files.iter().map(String::as_str).collect();
    let path_loaded_files = discover_path_loaded_files(
        extracts,
        &known_refs,
        &target_roots,
        &target_root_dirs,
        true,
    );

    let mut has_incoming_mod: FxHashSet<String> = FxHashSet::default();
    let mut prod_children: FxHashMap<String, Vec<String>> = FxHashMap::default();

    for ext in extracts.iter() {
        if ext.lang != Language::Rust || ext.rust_mod_paths.is_empty() {
            continue;
        }
        for mp in &ext.rust_mod_paths {
            let mut targets: Vec<String> = Vec::new();
            let base_dirs = rust_mod_base_dirs(
                &ext.file,
                &mp.mod_scope,
                mp.path.is_some(),
                &target_roots,
                &target_root_dirs,
                &path_loaded_files,
            );
            if let Some(ref raw_path) = mp.path {
                for bd in base_dirs {
                    let cand = normalize_rel_path_join(&bd, raw_path);
                    if known_rust_files.contains(&cand) {
                        push_unique(&mut targets, cand);
                    }
                }
            } else {
                for bd in base_dirs {
                    for suffix in [".rs", "/mod.rs"] {
                        let rel = format!("{}{}", mp.mod_name, suffix);
                        let cand = normalize_rel_path_join(&bd, &rel);
                        if known_rust_files.contains(&cand) {
                            push_unique(&mut targets, cand);
                        }
                    }
                }
            }
            for child_file in targets {
                if child_file == ext.file {
                    continue;
                }
                has_incoming_mod.insert(child_file.clone());
                if !mp.is_test && !ext.is_test_file {
                    push_unique(
                        prod_children.entry(ext.file.clone()).or_default(),
                        child_file,
                    );
                }
            }
        }
    }

    let lib_roots: FxHashSet<&str> = crate_roots
        .values()
        .map(|info| info.lib_file.as_str())
        .collect();
    let mut prod_reachable: FxHashSet<String> = FxHashSet::default();
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for ext in extracts.iter() {
        if ext.lang != Language::Rust {
            continue;
        }
        let has_prod_entry = ext.symbols.iter().any(|s| s.is_entrypoint && !s.is_test);
        let is_prod_lib_root = !ext.is_test_file && lib_roots.contains(ext.file.as_str());
        if ((!ext.is_test_file && !has_incoming_mod.contains(&ext.file))
            || has_prod_entry
            || is_prod_lib_root)
            && prod_reachable.insert(ext.file.clone())
        {
            queue.push_back(ext.file.clone());
        }
    }

    while let Some(curr) = queue.pop_front() {
        if let Some(children) = prod_children.get(&curr) {
            for ch in children {
                if prod_reachable.insert(ch.clone()) {
                    queue.push_back(ch.clone());
                }
            }
        }
    }

    for ext in extracts.iter_mut() {
        if ext.lang == Language::Rust
            && has_incoming_mod.contains(&ext.file)
            && !prod_reachable.contains(&ext.file)
        {
            ext.is_test_file = true;
            for sym in &mut ext.symbols {
                sym.is_test = true;
            }
            for u in &mut ext.rust_uses {
                u.is_test = true;
            }
            for imp in &mut ext.trait_impls {
                imp.is_test = true;
            }
            ext.cfg_gated_symbols.clear();
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum RustChainState {
    Type(String, usize),
    Tuple {
        owner_file: String,
        owner_pkg: String,
        elems: Vec<String>,
        depth: usize,
    },
}

pub struct RustTypeIndex<'a> {
    file_imports: FxHashMap<&'a str, &'a [String]>,
    file_rust_uses: FxHashMap<&'a str, &'a [RustUseItem]>,
    known_files: FxHashSet<&'a str>,
    crate_roots: HashMap<String, CargoCrateInfo>,
    file_to_crate_root: HashMap<&'a str, String>,
    crate_root_to_idents: HashMap<String, Vec<String>>,
    type_aliases: HashMap<String, String>,
    symbol_mod_scopes: FxHashMap<String, String>,
    file_inline_scopes: FxHashMap<&'a str, FxHashSet<&'a str>>,
    cfg_gated_ids: FxHashSet<String>,
    file_non_method_syms: FxHashMap<(&'a str, &'a str), Vec<String>>,
    file_pub_uses_by_local: FxHashMap<(&'a str, &'a str), Vec<&'a RustUseItem>>,
    file_uses_by_local: FxHashMap<(&'a str, &'a str), Vec<&'a RustUseItem>>,
    file_mod_paths: FxHashMap<(&'a str, &'a str), Vec<String>>,
    mod_path_names: FxHashSet<&'a str>,
    types_by_file_name: HashMap<(&'a str, &'a str), Vec<String>>,
    types_by_pkg_name: HashMap<(String, &'a str), Vec<String>>,
    types_by_name: HashMap<&'a str, Vec<String>>,
    traits_by_file_name: HashMap<(&'a str, &'a str), String>,
    traits_by_pkg_name: HashMap<(String, &'a str), Vec<String>>,
    traits_by_name: HashMap<&'a str, Vec<String>>,
    inherent_methods_by_type: HashMap<String, HashMap<String, Vec<String>>>,
    trait_methods_by_type: HashMap<String, HashMap<String, Vec<String>>>,
    test_methods_by_type: HashMap<String, HashMap<String, Vec<String>>>,
    raw_struct_fields: HashMap<String, HashMap<String, String>>,
    /// Wrapper type ID -> already-resolved Deref target type IDs (resolved in impl Deref scope).
    deref_target_ids: HashMap<String, Vec<String>>,
    func_return_types: HashMap<String, Vec<String>>,
    func_callback_params: HashMap<String, Vec<(usize, usize, String)>>,
    pub trait_methods: HashMap<String, HashSet<String>>,
    traits_by_method_name: FxHashMap<&'a str, Vec<String>>,
    cfg_sibling_groups: HashMap<String, Vec<String>>,
    target_roots: FxHashSet<String>,
    target_root_dirs: FxHashSet<String>,
}

impl<'a> RustTypeIndex<'a> {
    pub fn build(
        extracts: &'a [FileExtract],
        symbols: &HashMap<String, Symbol>,
        crate_roots: HashMap<String, CargoCrateInfo>,
    ) -> Self {
        let mut file_imports: FxHashMap<&'a str, &'a [String]> = FxHashMap::default();
        let mut file_rust_uses: FxHashMap<&'a str, &'a [RustUseItem]> = FxHashMap::default();
        let mut known_files: FxHashSet<&'a str> = FxHashSet::default();
        for ext in extracts {
            if ext.lang == Language::Rust {
                known_files.insert(ext.file.as_str());
            }
        }

        let mut crate_root_to_idents: HashMap<String, Vec<String>> = HashMap::new();
        for (ident, info) in &crate_roots {
            crate_root_to_idents
                .entry(info.src_dir.clone())
                .or_default()
                .push(ident.clone());
        }
        let target_roots: FxHashSet<String> = crate_roots
            .values()
            .flat_map(|info| info.target_roots.iter().cloned())
            .collect();
        let target_root_dirs: FxHashSet<String> = crate_roots
            .values()
            .flat_map(|info| info.target_root_dirs.iter().cloned())
            .collect();
        let explicit_target_dirs: FxHashSet<&str> = target_roots
            .iter()
            .filter(|tr| known_files.contains(tr.as_str()))
            .filter_map(|tr| tr.rsplit_once('/').map(|(d, _)| d))
            .filter(|d| !d.is_empty())
            .filter(|d| !target_root_dirs.contains(*d))
            .filter(|d| !crate_roots.values().any(|info| info.src_dir == *d))
            .collect();
        let mut auto_bin_dirs: FxHashSet<String> = FxHashSet::default();
        for &file in &known_files {
            if let Some(bin_dir) = target_root_dirs
                .iter()
                .filter(|d| *d == "src/bin" || d.ends_with("/src/bin"))
                .find_map(|d| {
                    let prefix = format!("{}/", d);
                    file.strip_prefix(&prefix).and_then(|rest| {
                        rest.split_once('/').and_then(|(sub, _)| {
                            let cand_dir = format!("{}/{}", d, sub);
                            let cand_main = format!("{}/main.rs", cand_dir);
                            known_files.contains(cand_main.as_str()).then_some(cand_dir)
                        })
                    })
                })
            {
                auto_bin_dirs.insert(bin_dir);
            }
        }
        let mut file_to_crate_root: HashMap<&'a str, String> = HashMap::new();
        for &file in &known_files {
            // Pick the most specific target crate root directory across auto-discovered binaries
            // (src/bin/<name>/main.rs) and explicit non-library target directories.
            let best_target_dir = auto_bin_dirs
                .iter()
                .map(String::as_str)
                .chain(explicit_target_dirs.iter().copied())
                .filter(|d| file.starts_with(&format!("{}/", d)))
                .max_by_key(|d| d.len());
            if let Some(best_dir) = best_target_dir {
                file_to_crate_root.insert(file, best_dir.to_string());
                continue;
            }
            if target_roots.contains(file)
                && let Some((dir, _)) = file.rsplit_once('/')
            {
                file_to_crate_root.insert(file, dir.to_string());
                continue;
            }
            let mut best: Option<&str> = None;
            for info in crate_roots.values() {
                let root_dir = &info.src_dir;
                let matches = if root_dir.is_empty() {
                    true
                } else {
                    file.starts_with(&format!("{}/", root_dir))
                };
                if matches && best.is_none_or(|b| root_dir.len() > b.len()) {
                    best = Some(root_dir.as_str());
                }
            }
            if let Some(b) = best {
                file_to_crate_root.insert(file, b.to_string());
            } else if let Some(pos) = file.find("/src/") {
                file_to_crate_root.insert(file, format!("{}/src", &file[..pos]));
            } else if file.starts_with("src/") {
                file_to_crate_root.insert(file, "src".to_string());
            }
        }

        let mut type_aliases: HashMap<String, String> = HashMap::new();
        let mut symbol_mod_scopes: FxHashMap<String, String> = FxHashMap::default();
        let mut file_inline_scopes: FxHashMap<&'a str, FxHashSet<&'a str>> = FxHashMap::default();
        let mut cfg_gated_ids: FxHashSet<String> = FxHashSet::default();
        let mut file_non_method_syms: FxHashMap<(&'a str, &'a str), Vec<String>> =
            FxHashMap::default();
        let mut file_pub_uses_by_local: FxHashMap<(&'a str, &'a str), Vec<&'a RustUseItem>> =
            FxHashMap::default();
        let mut file_uses_by_local: FxHashMap<(&'a str, &'a str), Vec<&'a RustUseItem>> =
            FxHashMap::default();
        let mut macro_defs_by_file: FxHashMap<(&'a str, &'a str), Vec<&'a str>> =
            FxHashMap::default();
        let mut macro_defs_by_pkg: FxHashMap<(&'a str, &'a str), Vec<&'a str>> =
            FxHashMap::default();
        let mut macro_defs_by_name: FxHashMap<&'a str, Vec<&'a str>> = FxHashMap::default();
        let mut types_by_file_name: HashMap<(&'a str, &'a str), Vec<String>> = HashMap::new();
        let mut types_by_pkg_name: HashMap<(String, &'a str), Vec<String>> = HashMap::new();
        let mut types_by_name: HashMap<&'a str, Vec<String>> = HashMap::new();
        let mut traits_by_file_name: HashMap<(&'a str, &'a str), String> = HashMap::new();
        let mut traits_by_pkg_name: HashMap<(String, &'a str), Vec<String>> = HashMap::new();
        let mut traits_by_name: HashMap<&'a str, Vec<String>> = HashMap::new();
        let mut raw_struct_fields: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut raw_deref_by_name: HashMap<(&'a str, &'a str, String), Vec<String>> =
            HashMap::new();
        let mut func_return_types: HashMap<String, Vec<String>> = HashMap::new();
        let mut func_callback_params: HashMap<String, Vec<(usize, usize, String)>> = HashMap::new();
        let mut trait_methods: HashMap<String, HashSet<String>> = HashMap::new();
        let mut traits_by_method_name: FxHashMap<&'a str, Vec<String>> = FxHashMap::default();
        let mut trait_method_ids: HashSet<&str> = HashSet::new();
        let mut cfg_gated_scopes: HashMap<&str, &str> = HashMap::new();

        for ext in extracts {
            if ext.lang != Language::Rust {
                continue;
            }
            file_imports.insert(ext.file.as_str(), ext.imports.as_slice());
            file_rust_uses.insert(ext.file.as_str(), ext.rust_uses.as_slice());
            let crate_key = file_to_crate_root
                .get(ext.file.as_str())
                .map(String::as_str)
                .unwrap_or(ext.package_or_module.as_str());
            for (k, v) in &ext.type_aliases {
                type_aliases.insert(k.clone(), v.clone());
            }
            for (k, v) in &ext.symbol_mod_scopes {
                symbol_mod_scopes.insert(k.clone(), v.clone());
                if !v.is_empty() {
                    file_inline_scopes
                        .entry(ext.file.as_str())
                        .or_default()
                        .insert(v.as_str());
                }
            }
            for u in &ext.rust_uses {
                if !u.mod_scope.is_empty() {
                    file_inline_scopes
                        .entry(ext.file.as_str())
                        .or_default()
                        .insert(u.mod_scope.as_str());
                }
                file_uses_by_local
                    .entry((ext.file.as_str(), u.local_name.as_str()))
                    .or_default()
                    .push(u);
                if u.is_pub && !u.is_test && u.mod_scope.is_empty() {
                    file_pub_uses_by_local
                        .entry((ext.file.as_str(), u.local_name.as_str()))
                        .or_default()
                        .push(u);
                }
            }
            for (mname, gen_items) in &ext.rust_macro_defs {
                let f_ent = macro_defs_by_file
                    .entry((ext.file.as_str(), mname.as_str()))
                    .or_default();
                let p_ent = macro_defs_by_pkg
                    .entry((ext.package_or_module.as_str(), mname.as_str()))
                    .or_default();
                let g_ent = macro_defs_by_name.entry(mname.as_str()).or_default();
                for item in gen_items {
                    if !f_ent.contains(&item.as_str()) {
                        f_ent.push(item.as_str());
                    }
                    if !p_ent.contains(&item.as_str()) {
                        p_ent.push(item.as_str());
                    }
                    if !g_ent.contains(&item.as_str()) {
                        g_ent.push(item.as_str());
                    }
                }
            }
            for (k, v) in &ext.struct_fields {
                raw_struct_fields.insert(k.clone(), v.clone());
            }
            for (k, v) in &ext.embedded_types {
                raw_deref_by_name
                    .entry((ext.file.as_str(), ext.package_or_module.as_str(), k.clone()))
                    .or_default()
                    .extend(v.iter().cloned());
            }
            for (k, v) in &ext.func_return_types {
                func_return_types.insert(k.clone(), v.clone());
            }
            for (k, v) in &ext.func_callback_params {
                func_callback_params.insert(k.clone(), v.clone());
            }
            for (k, v) in &ext.interface_methods {
                for m in v {
                    push_unique(
                        traits_by_method_name.entry(m.as_str()).or_default(),
                        k.clone(),
                    );
                }
                trait_methods.insert(k.clone(), v.iter().cloned().collect());
            }
            for imp in &ext.trait_impls {
                for mid in &imp.method_ids {
                    trait_method_ids.insert(mid.as_str());
                }
            }
            for (id, scope) in &ext.cfg_gated_symbols {
                cfg_gated_scopes.insert(id.as_str(), scope.as_str());
                cfg_gated_ids.insert(id.clone());
            }
            for sym in &ext.symbols {
                let is_assoc_type =
                    sym.kind == SymbolKind::Type && sym.qualified_name.contains("::");
                if sym.kind != SymbolKind::Method && !is_assoc_type {
                    file_non_method_syms
                        .entry((ext.file.as_str(), sym.name.as_str()))
                        .or_default()
                        .push(sym.id.clone());
                }
                if !is_assoc_type
                    && matches!(
                        sym.kind,
                        SymbolKind::Struct
                            | SymbolKind::Enum
                            | SymbolKind::Type
                            | SymbolKind::Trait
                    )
                {
                    let entry = types_by_file_name
                        .entry((ext.file.as_str(), sym.name.as_str()))
                        .or_default();
                    let has_non_test = entry
                        .iter()
                        .any(|id| symbols.get(id.as_str()).is_some_and(|s| !s.is_test));
                    if !sym.is_test {
                        if !has_non_test {
                            entry.clear();
                        }
                        push_unique(entry, sym.id.clone());
                    } else if !has_non_test {
                        push_unique(entry, sym.id.clone());
                    }
                    if !crate_key.is_empty() {
                        types_by_pkg_name
                            .entry((crate_key.to_string(), sym.name.as_str()))
                            .or_default()
                            .push(sym.id.clone());
                    }
                    types_by_name
                        .entry(sym.name.as_str())
                        .or_default()
                        .push(sym.id.clone());
                }
                if sym.kind == SymbolKind::Trait {
                    traits_by_file_name
                        .entry((ext.file.as_str(), sym.name.as_str()))
                        .and_modify(|existing| {
                            if symbols.get(existing.as_str()).is_some_and(|s| s.is_test)
                                && !sym.is_test
                            {
                                *existing = sym.id.clone();
                            }
                        })
                        .or_insert_with(|| sym.id.clone());
                    if !crate_key.is_empty() {
                        traits_by_pkg_name
                            .entry((crate_key.to_string(), sym.name.as_str()))
                            .or_default()
                            .push(sym.id.clone());
                    }
                    traits_by_name
                        .entry(sym.name.as_str())
                        .or_default()
                        .push(sym.id.clone());
                }
            }
        }

        // Resolve #[path = "..."] mod declarations against known_files and index macro-generated items.
        let mut file_mod_paths: FxHashMap<(&'a str, &'a str), Vec<String>> = FxHashMap::default();
        let mut mod_path_names: FxHashSet<&'a str> = FxHashSet::default();
        let prod_path_loaded_files = discover_path_loaded_files(
            extracts,
            &known_files,
            &target_roots,
            &target_root_dirs,
            false,
        );
        let all_path_loaded_files = discover_path_loaded_files(
            extracts,
            &known_files,
            &target_roots,
            &target_root_dirs,
            true,
        );
        for ext in extracts {
            if ext.lang != Language::Rust {
                continue;
            }
            if !ext.rust_mod_paths.is_empty() {
                let active_path_loaded = if ext.is_test_file {
                    &all_path_loaded_files
                } else {
                    &prod_path_loaded_files
                };
                for mp in &ext.rust_mod_paths {
                    let Some(ref raw_path) = mp.path else {
                        continue;
                    };
                    for bd in rust_mod_base_dirs(
                        &ext.file,
                        &mp.mod_scope,
                        true,
                        &target_roots,
                        &target_root_dirs,
                        active_path_loaded,
                    ) {
                        let cand = normalize_rel_path_join(&bd, raw_path);
                        if known_files.contains(cand.as_str()) {
                            mod_path_names.insert(mp.mod_name.as_str());
                            push_unique(
                                file_mod_paths
                                    .entry((ext.file.as_str(), mp.mod_name.as_str()))
                                    .or_default(),
                                cand,
                            );
                        }
                    }
                }
            }
            for (inv_id, macro_target) in &ext.rust_top_level_macros {
                if let Some(inv_sym) = symbols.get(inv_id)
                    && let Some(items) =
                        macro_defs_by_file.get(&(ext.file.as_str(), inv_sym.name.as_str()))
                {
                    for &gen_name in items {
                        push_unique(
                            file_non_method_syms
                                .entry((ext.file.as_str(), gen_name))
                                .or_default(),
                            inv_id.clone(),
                        );
                    }
                }
                let gen_items = macro_defs_by_file
                    .get(&(ext.file.as_str(), macro_target.as_str()))
                    .or_else(|| {
                        macro_defs_by_pkg
                            .get(&(ext.package_or_module.as_str(), macro_target.as_str()))
                    })
                    .or_else(|| macro_defs_by_name.get(macro_target.as_str()));
                if let Some(items) = gen_items {
                    for &gen_name in items {
                        push_unique(
                            file_non_method_syms
                                .entry((ext.file.as_str(), gen_name))
                                .or_default(),
                            inv_id.clone(),
                        );
                    }
                }
            }
        }

        let mut idx = Self {
            file_imports,
            file_rust_uses,
            known_files,
            crate_roots,
            file_to_crate_root,
            crate_root_to_idents,
            type_aliases,
            symbol_mod_scopes,
            file_inline_scopes,
            cfg_gated_ids,
            file_non_method_syms,
            file_pub_uses_by_local,
            file_uses_by_local,
            file_mod_paths,
            mod_path_names,
            types_by_file_name,
            types_by_pkg_name,
            types_by_name,
            traits_by_file_name,
            traits_by_pkg_name,
            traits_by_name,
            inherent_methods_by_type: HashMap::new(),
            trait_methods_by_type: HashMap::new(),
            test_methods_by_type: HashMap::new(),
            raw_struct_fields,
            deref_target_ids: HashMap::new(),
            func_return_types,
            func_callback_params,
            trait_methods,
            traits_by_method_name,
            cfg_sibling_groups: HashMap::new(),
            target_roots,
            target_root_dirs,
        };

        // Resolve Deref targets in the impl Deref block's own module scope.
        for ((file, pkg, type_name), targets) in raw_deref_by_name {
            let imps = idx.file_imports.get(file).copied().unwrap_or(&[]);
            if let Some(tid) = idx.resolve_type(file, pkg, imps, &type_name, symbols) {
                for tname in targets {
                    if let Some(target_id) = idx.resolve_type(file, pkg, imps, &tname, symbols) {
                        let entry = idx.deref_target_ids.entry(tid.clone()).or_default();
                        if !entry.contains(&target_id) {
                            entry.push(target_id);
                        }
                    }
                }
            }
        }

        // Index Rust methods onto their owning concrete types, separating inherent vs trait methods.
        for sym in symbols.values() {
            if sym.lang != Language::Rust || sym.kind != SymbolKind::Method {
                continue;
            }
            if let Some((recv_type, _)) = sym.qualified_name.split_once("::") {
                let imps = idx
                    .file_imports
                    .get(sym.file.as_str())
                    .copied()
                    .unwrap_or(&[]);
                if let Some(type_id) =
                    idx.resolve_type(&sym.file, &sym.package_or_module, imps, recv_type, symbols)
                {
                    let type_is_test = symbols.get(&type_id).map(|s| s.is_test).unwrap_or(false);
                    if sym.is_test && !type_is_test {
                        idx.test_methods_by_type
                            .entry(type_id)
                            .or_default()
                            .entry(sym.name.clone())
                            .or_default()
                            .push(sym.id.clone());
                    } else if trait_method_ids.contains(sym.id.as_str()) {
                        idx.trait_methods_by_type
                            .entry(type_id)
                            .or_default()
                            .entry(sym.name.clone())
                            .or_default()
                            .push(sym.id.clone());
                    } else {
                        idx.inherent_methods_by_type
                            .entry(type_id)
                            .or_default()
                            .entry(sym.name.clone())
                            .or_default()
                            .push(sym.id.clone());
                    }
                }
            }
        }

        // Group #[cfg(...)] sibling definitions sharing (file, inline_mod_scope, qualified_name, kind, is_test)
        // and same-crate #[cfg] methods sharing (package_or_module, qualified_name, is_test).
        let mut same_file_groups: HashMap<(&str, &str, &str, SymbolKind, bool), Vec<String>> =
            HashMap::new();
        let mut same_pkg_cfg_methods: HashMap<(&str, &str, bool), Vec<String>> = HashMap::new();
        for sym in symbols.values() {
            if sym.lang != Language::Rust
                || !matches!(sym.kind, SymbolKind::Function | SymbolKind::Method)
            {
                continue;
            }
            let Some(&mod_scope) = cfg_gated_scopes.get(sym.id.as_str()) else {
                continue;
            };
            same_file_groups
                .entry((
                    sym.file.as_str(),
                    mod_scope,
                    sym.qualified_name.as_str(),
                    sym.kind,
                    sym.is_test,
                ))
                .or_default()
                .push(sym.id.clone());
            if sym.kind == SymbolKind::Method {
                same_pkg_cfg_methods
                    .entry((
                        sym.package_or_module.as_str(),
                        sym.qualified_name.as_str(),
                        sym.is_test,
                    ))
                    .or_default()
                    .push(sym.id.clone());
            }
        }
        for group in same_file_groups
            .values()
            .chain(same_pkg_cfg_methods.values())
        {
            if group.len() > 1 {
                for id in group {
                    let entry = idx.cfg_sibling_groups.entry(id.clone()).or_default();
                    for other in group {
                        if !entry.contains(other) {
                            entry.push(other.clone());
                        }
                    }
                }
            }
        }

        idx
    }

    pub fn expand_cfg_siblings(
        &self,
        results: Vec<(String, Confidence)>,
    ) -> Vec<(String, Confidence)> {
        if self.cfg_sibling_groups.is_empty() || results.is_empty() {
            return results;
        }
        let mut out = Vec::with_capacity(results.len());
        let mut seen: HashSet<String> = HashSet::new();
        for (id, conf) in results {
            if let Some(group) = self.cfg_sibling_groups.get(&id) {
                for sib in group {
                    if seen.insert(sib.clone()) {
                        out.push((sib.clone(), conf));
                    }
                }
            } else if seen.insert(id.clone()) {
                out.push((id, conf));
            }
        }
        out
    }

    pub fn resolve_type(
        &self,
        owner_file: &str,
        owner_pkg: &str,
        owner_imports: &[String],
        type_name: &str,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<String> {
        let tid =
            self.resolve_type_raw(owner_file, owner_pkg, owner_imports, type_name, symbols)?;
        let (unwrapped, extra_depth) = self.unwrap_type_alias(tid.clone(), symbols, true);
        if extra_depth == 0 {
            Some(unwrapped)
        } else {
            Some(tid)
        }
    }

    /// Resolve one or more type IDs for associated function calls (e.g. `Alias::new()`,
    /// `imp::SleepInhibitor::new()`), expanding `#[cfg]` module/type aliases without flattening
    /// smart-pointer wrappers like `Arc<T>`.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_direct_types(
        &self,
        owner_file: &str,
        owner_pkg: &str,
        owner_imports: &[String],
        caller_id: &str,
        type_name: &str,
        caller_is_test: bool,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<String> {
        let caller_scope = self
            .symbol_mod_scopes
            .get(caller_id)
            .map(String::as_str)
            .unwrap_or("");
        let raw_ids = self.resolve_types_raw_in_scope(
            owner_file,
            owner_pkg,
            owner_imports,
            type_name,
            caller_scope,
            caller_is_test,
            symbols,
        );
        let mut out = Vec::new();
        for tid in raw_ids {
            let (unwrapped, extra_depth) = self.unwrap_type_alias(tid.clone(), symbols, false);
            let chosen = if extra_depth == 0 { unwrapped } else { tid };
            push_unique(&mut out, chosen);
        }
        out
    }

    fn resolve_type_raw(
        &self,
        owner_file: &str,
        owner_pkg: &str,
        owner_imports: &[String],
        type_name: &str,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<String> {
        self.resolve_type_raw_in_scope(owner_file, owner_pkg, owner_imports, type_name, "", symbols)
    }

    fn resolve_type_raw_in_scope(
        &self,
        owner_file: &str,
        owner_pkg: &str,
        owner_imports: &[String],
        type_name: &str,
        caller_scope: &str,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<String> {
        let mut ids = self.resolve_types_raw_in_scope(
            owner_file,
            owner_pkg,
            owner_imports,
            type_name,
            caller_scope,
            false,
            symbols,
        );
        if ids.is_empty() {
            None
        } else {
            Some(ids.remove(0))
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_types_raw_in_scope(
        &self,
        owner_file: &str,
        owner_pkg: &str,
        owner_imports: &[String],
        type_name: &str,
        caller_scope: &str,
        caller_is_test: bool,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<String> {
        // Handle module-qualified type paths (e.g. `remote::Worker`, `imp::SleepInhibitor`).
        if let Some((mod_qual, short_name)) = type_name.rsplit_once("::") {
            let mod_segs: Vec<&str> = mod_qual.split("::").filter(|s| !s.is_empty()).collect();
            let mut out = Vec::new();
            if !mod_segs.is_empty() {
                // First expand local `use ... as mod_alias;` (including multi-#[cfg] aliases).
                if let Some(uses) = self.file_uses_by_local.get(&(owner_file, mod_segs[0])) {
                    for u in uses {
                        let scope_ok = u.mod_scope == caller_scope
                            || (!caller_scope.is_empty() && u.mod_scope.is_empty());
                        if scope_ok && (!u.is_test || caller_is_test) {
                            let expanded = if mod_segs.len() == 1 {
                                u.target_path.clone()
                            } else {
                                format!("{}::{}", u.target_path, mod_segs[1..].join("::"))
                            };
                            let mut visited = HashSet::new();
                            self.resolve_mod_path_into(
                                owner_file,
                                &expanded,
                                short_name,
                                RustSymFilter::TypesOnly,
                                symbols,
                                0,
                                &mut visited,
                                &mut out,
                            );
                        }
                    }
                }
                if !out.is_empty() {
                    return out;
                }
                let mut visited = HashSet::new();
                self.resolve_mod_path_into(
                    owner_file,
                    mod_qual,
                    short_name,
                    RustSymFilter::TypesOnly,
                    symbols,
                    0,
                    &mut visited,
                    &mut out,
                );
                if !out.is_empty() {
                    return out;
                }
            }
            let Some(cands) = self.types_by_name.get(short_name) else {
                return Vec::new();
            };
            let matched: Vec<&String> = cands
                .iter()
                .filter(|id| {
                    symbols
                        .get(*id)
                        .map(|s| symbol_matches_rust_mod_qual(s, mod_qual))
                        .unwrap_or(false)
                })
                .collect();
            if matched.len() == 1 {
                return vec![matched[0].clone()];
            }
            let same_pkg: Vec<&String> = matched
                .into_iter()
                .filter(|id| {
                    symbols
                        .get(*id)
                        .map(|s| s.package_or_module == owner_pkg)
                        .unwrap_or(false)
                })
                .collect();
            if same_pkg.len() == 1 {
                return vec![same_pkg[0].clone()];
            }
            // Support reexported types (e.g. api::Worker reexporting impl_mod::Worker)
            // only when there is a single unique definition in the entire repository.
            if cands.len() == 1 {
                return vec![cands[0].clone()];
            }
            return Vec::new();
        }
        if let Some(ids) = self.types_by_file_name.get(&(owner_file, type_name))
            && !ids.is_empty()
        {
            return ids.clone();
        }
        // Explicit `use` (and glob `use super::*;`) imports MUST take precedence over crate-wide fallback.
        let imported = self.resolve_imported_types_in_scope_depth(
            owner_file,
            type_name,
            caller_scope,
            true,
            symbols,
            0,
        );
        if !imported.is_empty() {
            return imported;
        }
        let crate_key = self
            .file_to_crate_root
            .get(owner_file)
            .map(String::as_str)
            .unwrap_or(owner_pkg);
        if !crate_key.is_empty()
            && let Some(cands) = self
                .types_by_pkg_name
                .get(&(crate_key.to_string(), type_name))
            && let Some(id) = self.pick_unique_type_cand(cands, owner_imports, symbols)
        {
            return vec![id];
        }
        if let Some(cands) = self.types_by_name.get(type_name)
            && let Some(id) = self.pick_unique_type_cand(cands, owner_imports, symbols)
        {
            return vec![id];
        }
        Vec::new()
    }

    fn pick_unique_type_cand(
        &self,
        cands: &[String],
        owner_imports: &[String],
        symbols: &HashMap<String, Symbol>,
    ) -> Option<String> {
        let non_test: Vec<&String> = cands
            .iter()
            .filter(|id| symbols.get(*id).is_some_and(|s| !s.is_test))
            .collect();
        let pool: Vec<&String> = if !non_test.is_empty() {
            non_test
        } else {
            cands.iter().collect()
        };
        if pool.len() == 1 {
            return Some(pool[0].clone());
        }
        let matched: Vec<&String> = pool
            .into_iter()
            .filter(|id| {
                symbols
                    .get(*id)
                    .map(|s| self.imports_match_symbol(owner_imports, s))
                    .unwrap_or(false)
            })
            .collect();
        if matched.len() == 1 {
            return Some(matched[0].clone());
        }
        None
    }

    fn resolve_imported_type_in_scope(
        &self,
        owner_file: &str,
        type_name: &str,
        caller_scope: &str,
        include_root_uses: bool,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<String> {
        let mut ids = self.resolve_imported_types_in_scope_depth(
            owner_file,
            type_name,
            caller_scope,
            include_root_uses,
            symbols,
            0,
        );
        if ids.len() == 1 { ids.pop() } else { None }
    }

    fn resolve_imported_types_in_scope_depth(
        &self,
        owner_file: &str,
        type_name: &str,
        caller_scope: &str,
        include_root_uses: bool,
        symbols: &HashMap<String, Symbol>,
        depth: usize,
    ) -> Vec<String> {
        if depth > 4 {
            return Vec::new();
        }
        let mut scoped_resolved: Vec<String> = Vec::new();
        let mut root_resolved: Vec<String> = Vec::new();
        if let Some(uses) = self.file_uses_by_local.get(&(owner_file, type_name)) {
            for u in uses {
                let is_exact = u.mod_scope == caller_scope;
                let is_root_fallback =
                    include_root_uses && !caller_scope.is_empty() && u.mod_scope.is_empty();
                if !u.is_test
                    && (is_exact || is_root_fallback)
                    && let Some((mod_path, target_sym)) = u.target_path.rsplit_once("::")
                {
                    let bucket = if is_exact {
                        &mut scoped_resolved
                    } else {
                        &mut root_resolved
                    };
                    if mod_path == "super" && !u.mod_scope.is_empty() {
                        let parent_scope =
                            u.mod_scope.rsplit_once("::").map(|(p, _)| p).unwrap_or("");
                        if let Some(ids) = self.file_non_method_syms.get(&(owner_file, target_sym))
                        {
                            let hits: Vec<&String> = ids
                                .iter()
                                .filter(|id| {
                                    self.symbol_mod_scopes
                                        .get(id.as_str())
                                        .map(String::as_str)
                                        .unwrap_or("")
                                        == parent_scope
                                        && symbols.get(*id).is_some_and(|s| {
                                            !s.is_test
                                                && matches!(
                                                    s.kind,
                                                    SymbolKind::Struct
                                                        | SymbolKind::Enum
                                                        | SymbolKind::Type
                                                        | SymbolKind::Trait
                                                )
                                        })
                                })
                                .collect();
                            if !hits.is_empty() && hits.len() <= 4 {
                                for h in hits {
                                    push_unique(bucket, h.clone());
                                }
                                continue;
                            }
                        }
                    }
                    let mut hits = Vec::new();
                    let mut visited = HashSet::new();
                    self.resolve_mod_path_into(
                        owner_file,
                        mod_path,
                        target_sym,
                        RustSymFilter::TypesOnly,
                        symbols,
                        0,
                        &mut visited,
                        &mut hits,
                    );
                    if !hits.is_empty() && hits.len() <= 4 {
                        for h in hits {
                            push_unique(bucket, h);
                        }
                    }
                }
            }
        }
        if scoped_resolved.is_empty()
            && root_resolved.is_empty()
            && let Some(glob_uses) = self.file_uses_by_local.get(&(owner_file, "*"))
        {
            for u in glob_uses {
                let is_exact = u.mod_scope == caller_scope;
                let is_root_fallback =
                    include_root_uses && !caller_scope.is_empty() && u.mod_scope.is_empty();
                if !u.is_test && (is_exact || is_root_fallback) {
                    let bucket = if is_exact {
                        &mut scoped_resolved
                    } else {
                        &mut root_resolved
                    };
                    let mut hits = Vec::new();
                    let mut visited = HashSet::new();
                    self.resolve_mod_path_into(
                        owner_file,
                        &u.target_path,
                        type_name,
                        RustSymFilter::TypesOnly,
                        symbols,
                        0,
                        &mut visited,
                        &mut hits,
                    );
                    // Also follow private `use ... as TypeAlias;` inherited from a parent module via `use super::*;`
                    if hits.is_empty() && u.target_path == "super" {
                        for mf in self.resolve_module_files(owner_file, &["super"]) {
                            if mf != owner_file {
                                for id in self.resolve_imported_types_in_scope_depth(
                                    &mf,
                                    type_name,
                                    "",
                                    false,
                                    symbols,
                                    depth + 1,
                                ) {
                                    push_unique(&mut hits, id);
                                }
                            }
                        }
                    }
                    if !hits.is_empty() && hits.len() <= 4 {
                        for h in hits {
                            push_unique(bucket, h);
                        }
                    }
                }
            }
        }
        if !scoped_resolved.is_empty() {
            if scoped_resolved.len() <= 4 {
                scoped_resolved
            } else {
                Vec::new()
            }
        } else if root_resolved.len() <= 4 {
            root_resolved
        } else {
            Vec::new()
        }
    }

    fn unwrap_type_alias(
        &self,
        start_id: String,
        symbols: &HashMap<String, Symbol>,
        follow_wrappers: bool,
    ) -> (String, usize) {
        let mut curr_id = start_id;
        let mut extra_depth = 0usize;
        let mut visited: HashSet<String> = HashSet::from([curr_id.clone()]);
        for _ in 0..4 {
            let Some(raw_target) = self.type_aliases.get(&curr_id) else {
                break;
            };
            let (is_wrapper, target_clean) = match raw_target.strip_prefix('*') {
                Some(rest) => (true, rest),
                None => (false, raw_target.as_str()),
            };
            if is_wrapper && !follow_wrappers {
                break;
            }
            let Some(sym) = symbols.get(&curr_id) else {
                break;
            };
            let imps = self
                .file_imports
                .get(sym.file.as_str())
                .copied()
                .unwrap_or(&[]);
            let mut target_ty = target_clean;
            let mut d = 0usize;
            while let Some(rest) = target_ty.strip_prefix("[]") {
                d += 1;
                target_ty = rest;
            }
            if d > 0 && !follow_wrappers {
                break;
            }
            let Some(next_id) =
                self.resolve_type_raw(&sym.file, &sym.package_or_module, imps, target_ty, symbols)
            else {
                break;
            };
            if !visited.insert(next_id.clone()) {
                break;
            }
            curr_id = next_id;
            extra_depth += d;
        }
        (curr_id, extra_depth)
    }

    pub fn resolve_trait(
        &self,
        owner_file: &str,
        owner_pkg: &str,
        owner_imports: &[String],
        trait_path: &str,
        trait_name: &str,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<String> {
        let clean_path = trait_path.trim().trim_start_matches("::");
        if is_external_rust_crate_path(clean_path) {
            return None;
        }
        if let Some((raw_mod_qual, short_name)) = clean_path.rsplit_once("::") {
            let mod_qual = raw_mod_qual
                .strip_prefix("crate::")
                .or_else(|| raw_mod_qual.strip_prefix("super::"))
                .or_else(|| raw_mod_qual.strip_prefix("self::"))
                .unwrap_or(raw_mod_qual);
            if !matches!(mod_qual, "crate" | "super" | "self" | "") {
                let cands = self.traits_by_name.get(short_name)?;
                let matched: Vec<&String> = cands
                    .iter()
                    .filter(|id| {
                        symbols
                            .get(*id)
                            .map(|s| symbol_matches_rust_mod_qual(s, mod_qual))
                            .unwrap_or(false)
                    })
                    .collect();
                return if matched.len() == 1 {
                    Some(matched[0].clone())
                } else {
                    // Inline module trait (e.g. mod tasks { pub trait Task }) in the same file
                    self.traits_by_file_name
                        .get(&(owner_file, short_name))
                        .cloned()
                };
            }
        }
        if let Some(id) = self.traits_by_file_name.get(&(owner_file, trait_name)) {
            return Some(id.clone());
        }
        if let Some(id) =
            self.resolve_imported_type_in_scope(owner_file, trait_name, "", true, symbols)
            && symbols
                .get(&id)
                .is_some_and(|s| s.kind == SymbolKind::Trait)
        {
            return Some(id);
        }
        let crate_key = self
            .file_to_crate_root
            .get(owner_file)
            .map(String::as_str)
            .unwrap_or(owner_pkg);
        if !crate_key.is_empty()
            && let Some(cands) = self
                .traits_by_pkg_name
                .get(&(crate_key.to_string(), trait_name))
            && let Some(id) = self.pick_unique_type_cand(cands, owner_imports, symbols)
        {
            return Some(id);
        }
        let cands = self.traits_by_name.get(trait_name)?;
        self.pick_unique_type_cand(cands, owner_imports, symbols)
    }

    fn resolve_type_states_in_scope(
        &self,
        owner_file: &str,
        owner_pkg: &str,
        owner_imports: &[String],
        raw_type: &str,
        caller_scope: &str,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<RustChainState> {
        let mut out = Vec::new();
        for branch in raw_type.split('|') {
            let mut ty = branch.trim();
            if ty.is_empty() {
                continue;
            }
            let mut depth = 0usize;
            while let Some(rest) = ty.strip_prefix("[]") {
                depth += 1;
                ty = rest;
            }
            if ty.starts_with('(') && ty.ends_with(')') && ty.len() >= 2 {
                let elems = split_tuple_norm_elems(&ty[1..ty.len() - 1]);
                if elems.len() >= 2 {
                    let st = RustChainState::Tuple {
                        owner_file: owner_file.to_string(),
                        owner_pkg: owner_pkg.to_string(),
                        elems,
                        depth,
                    };
                    if !out.contains(&st) {
                        out.push(st);
                    }
                    continue;
                }
            }
            for tid in self.resolve_types_raw_in_scope(
                owner_file,
                owner_pkg,
                owner_imports,
                ty,
                caller_scope,
                false,
                symbols,
            ) {
                if let Some(raw_alias) = self.type_aliases.get(&tid) {
                    let clean = raw_alias.strip_prefix('*').unwrap_or(raw_alias.as_str());
                    let mut adepth = 0usize;
                    let mut aty = clean;
                    while let Some(rest) = aty.strip_prefix("[]") {
                        adepth += 1;
                        aty = rest;
                    }
                    if aty.starts_with('(')
                        && aty.ends_with(')')
                        && aty.len() >= 2
                        && let Some(sym) = symbols.get(&tid)
                    {
                        let elems = split_tuple_norm_elems(&aty[1..aty.len() - 1]);
                        if elems.len() >= 2 {
                            let st = RustChainState::Tuple {
                                owner_file: sym.file.clone(),
                                owner_pkg: sym.package_or_module.clone(),
                                elems,
                                depth: depth + adepth,
                            };
                            if !out.contains(&st) {
                                out.push(st);
                            }
                            continue;
                        }
                    }
                }
                let (unwrapped, extra_depth) = self.unwrap_type_alias(tid, symbols, true);
                let st = RustChainState::Type(unwrapped, depth + extra_depth);
                if !out.contains(&st) {
                    out.push(st);
                }
            }
        }
        out
    }

    pub fn imports_match_symbol(&self, imports: &[String], sym: &Symbol) -> bool {
        if rust_imports_match_symbol(imports, sym) {
            return true;
        }
        if let Some(root_dir) = self.file_to_crate_root.get(sym.file.as_str())
            && let Some(idents) = self.crate_root_to_idents.get(root_dir)
        {
            return idents
                .iter()
                .any(|ident| imports.iter().any(|imp| imp.contains(ident.as_str())));
        }
        false
    }

    fn push_module_candidates(&self, base_dir: &str, rest: &[&str], out: &mut Vec<String>) {
        let join_dir = |a: &str, b: &str| -> String {
            match (a.is_empty(), b.is_empty()) {
                (true, _) => b.to_string(),
                (_, true) => a.to_string(),
                (false, false) => format!("{}/{}", a, b),
            }
        };
        let cands = if rest.is_empty() {
            let mut v = vec![
                join_dir(base_dir, "lib.rs"),
                join_dir(base_dir, "mod.rs"),
                join_dir(base_dir, "main.rs"),
            ];
            if !base_dir.is_empty() {
                v.push(format!("{}.rs", base_dir));
            }
            v
        } else {
            let sub = rest.join("/");
            let full = join_dir(base_dir, &sub);
            vec![
                format!("{}.rs", full),
                format!("{}/mod.rs", full),
                format!("{}/lib.rs", full),
            ]
        };
        for cand in cands {
            if self.known_files.contains(cand.as_str()) && !out.contains(&cand) {
                out.push(cand);
            }
        }
    }

    fn resolve_module_files(&self, base_file: &str, segs: &[&str]) -> Vec<String> {
        self.resolve_module_files_depth(base_file, segs, 0)
    }

    fn resolve_module_files_depth(
        &self,
        base_file: &str,
        segs: &[&str],
        depth: usize,
    ) -> Vec<String> {
        let mut out = Vec::new();
        if depth > 4
            || segs.len() > 8
            || segs
                .iter()
                .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        {
            return out;
        }
        let Some(&first) = segs.first() else {
            return out;
        };
        let file_dir = base_file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let file_name = base_file.rsplit('/').next().unwrap_or(base_file);
        let is_dir_mod = matches!(file_name, "lib.rs" | "mod.rs" | "main.rs");

        if first == "crate" {
            if let Some(root) = self.file_to_crate_root.get(base_file) {
                self.push_module_candidates(root, &segs[1..], &mut out);
            }
        } else if first == "super" {
            let mut super_count = 0usize;
            while segs.get(super_count) == Some(&"super") {
                super_count += 1;
            }
            let pops = if is_dir_mod {
                super_count
            } else {
                super_count.saturating_sub(1)
            };
            let mut dir = file_dir;
            for _ in 0..pops {
                dir = dir.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            }
            self.push_module_candidates(dir, &segs[super_count..], &mut out);
        } else if first == "self" {
            if segs.len() >= 2
                && let Some(mapped) = self.file_mod_paths.get(&(base_file, segs[1]))
            {
                for mf in mapped {
                    if segs.len() == 2 {
                        push_unique(&mut out, mf.clone());
                    } else {
                        for sub in self.resolve_module_files_depth(mf, &segs[2..], depth + 1) {
                            push_unique(&mut out, sub);
                        }
                    }
                }
            }
            let self_dir = if is_dir_mod {
                file_dir.to_string()
            } else {
                let stem = file_name.strip_suffix(".rs").unwrap_or(file_name);
                if file_dir.is_empty() {
                    stem.to_string()
                } else {
                    format!("{}/{}", file_dir, stem)
                }
            };
            self.push_module_candidates(&self_dir, &segs[1..], &mut out);
            if !is_dir_mod {
                self.push_module_candidates(file_dir, &segs[1..], &mut out);
            }
        } else if let Some(crate_info) = self.crate_roots.get(first) {
            if segs.len() == 1 {
                if self.known_files.contains(crate_info.lib_file.as_str())
                    && !out.contains(&crate_info.lib_file)
                {
                    out.push(crate_info.lib_file.clone());
                }
            } else {
                self.push_module_candidates(&crate_info.src_dir, &segs[1..], &mut out);
            }
        } else {
            if let Some(mapped) = self.file_mod_paths.get(&(base_file, first)) {
                for mf in mapped {
                    if segs.len() == 1 {
                        push_unique(&mut out, mf.clone());
                    } else {
                        for sub in self.resolve_module_files_depth(mf, &segs[1..], depth + 1) {
                            push_unique(&mut out, sub);
                        }
                    }
                }
            }
            // Relative to current module directory, submodule directory (if foo.rs), or crate root.
            self.push_module_candidates(file_dir, segs, &mut out);
            if !is_dir_mod {
                let stem = file_name.strip_suffix(".rs").unwrap_or(file_name);
                let sub_dir = if file_dir.is_empty() {
                    stem.to_string()
                } else {
                    format!("{}/{}", file_dir, stem)
                };
                self.push_module_candidates(&sub_dir, segs, &mut out);
            }
            if let Some(root) = self.file_to_crate_root.get(base_file)
                && root != file_dir
            {
                self.push_module_candidates(root, segs, &mut out);
            }
        }

        // Fall back to stepping through intermediate #[path = "..."] module declarations
        // (e.g. `crate::context_sources::framing` where `context_sources.rs` has `#[path = "..."] mod framing;`).
        if out.is_empty() && segs.len() >= 2 && !self.mod_path_names.is_empty() {
            for split in (1..segs.len()).rev() {
                if !self.mod_path_names.contains(segs[split]) {
                    continue;
                }
                let parents = self.resolve_module_files_depth(base_file, &segs[..split], depth + 1);
                for pf in parents {
                    if let Some(mapped) = self.file_mod_paths.get(&(pf.as_str(), segs[split])) {
                        for mf in mapped {
                            if split + 1 == segs.len() {
                                push_unique(&mut out, mf.clone());
                            } else {
                                for sub in self.resolve_module_files_depth(
                                    mf,
                                    &segs[split + 1..],
                                    depth + 1,
                                ) {
                                    push_unique(&mut out, sub);
                                }
                            }
                        }
                    }
                }
                if !out.is_empty() {
                    break;
                }
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_symbol_in_inline_mod(
        &self,
        mod_file: &str,
        inline_scope: &str,
        sym_name: &str,
        filter: RustSymFilter,
        symbols: &HashMap<String, Symbol>,
        depth: usize,
        visited: &mut HashSet<(String, String)>,
        out: &mut Vec<String>,
    ) {
        if depth > 4 || inline_scope.is_empty() {
            return;
        }
        let visit_key = (
            format!("{}#{}", mod_file, inline_scope),
            sym_name.to_string(),
        );
        if !visited.insert(visit_key) {
            return;
        }
        let mut direct_hits = Vec::new();
        if let Some(ids) = self.file_non_method_syms.get(&(mod_file, sym_name)) {
            for id in ids {
                if self
                    .symbol_mod_scopes
                    .get(id.as_str())
                    .map(String::as_str)
                    .unwrap_or("")
                    == inline_scope
                    && symbols
                        .get(id)
                        .is_some_and(|s| !s.is_test && filter.matches(s.kind))
                {
                    push_unique(&mut direct_hits, id.clone());
                }
            }
        }
        let had_direct_hits = !direct_hits.is_empty();
        let any_cfg = direct_hits
            .iter()
            .any(|id| self.cfg_gated_ids.contains(id.as_str()));
        for h in direct_hits {
            push_unique(out, h);
        }
        if had_direct_hits && !any_cfg {
            return;
        }
        let resolve_inline_use_target =
            |target_path: &str, out: &mut Vec<String>, visited: &mut HashSet<(String, String)>| {
                // Inside inline `mod foo { pub use super::bar::Baz; }`, one leading `super::`
                // climbs out of `foo` to `mod_file`'s enclosing scope rather than the parent file.
                let adjusted = if let Some(rest) = target_path.strip_prefix("super::") {
                    if let Some((parent_scope, _)) = inline_scope.rsplit_once("::") {
                        format!("{}::{}", parent_scope, rest)
                    } else {
                        rest.to_string()
                    }
                } else {
                    target_path.to_string()
                };
                if let Some((mp, tsym)) = adjusted.rsplit_once("::") {
                    self.resolve_mod_path_into(
                        mod_file,
                        mp,
                        tsym,
                        filter,
                        symbols,
                        depth + 1,
                        visited,
                        out,
                    );
                } else {
                    for hit in self.resolve_symbol_in_module(
                        mod_file,
                        &adjusted,
                        filter,
                        symbols,
                        depth + 1,
                        visited,
                    ) {
                        push_unique(out, hit);
                    }
                }
            };
        if let Some(uses) = self.file_uses_by_local.get(&(mod_file, sym_name)) {
            for u in uses {
                if u.is_pub && !u.is_test && u.mod_scope == inline_scope {
                    resolve_inline_use_target(&u.target_path, out, visited);
                }
            }
        }
        if let Some(glob_uses) = self.file_uses_by_local.get(&(mod_file, "*")) {
            for u in glob_uses {
                if u.is_pub && !u.is_test && u.mod_scope == inline_scope {
                    let target = format!("{}::{}", u.target_path, sym_name);
                    resolve_inline_use_target(&target, out, visited);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_mod_path_into(
        &self,
        base_file: &str,
        mod_path: &str,
        sym_name: &str,
        filter: RustSymFilter,
        symbols: &HashMap<String, Symbol>,
        depth: usize,
        visited: &mut HashSet<(String, String)>,
        out: &mut Vec<String>,
    ) {
        let mod_segs: Vec<&str> = mod_path.split("::").filter(|s| !s.is_empty()).collect();
        let before = out.len();
        // When a crate root (e.g. src/bin/tool/main.rs or app/entry.rs) calls `crate::api::...`
        // for its own inline module, resolve that inline module before searching library directories.
        if is_rust_crate_root_file(base_file, &self.target_roots, &self.target_root_dirs)
            && let Some(crate_inline) = mod_path.strip_prefix("crate::")
            && !crate_inline.is_empty()
            && self
                .file_inline_scopes
                .get(base_file)
                .is_some_and(|scopes| scopes.contains(crate_inline))
        {
            self.resolve_symbol_in_inline_mod(
                base_file,
                crate_inline,
                sym_name,
                filter,
                symbols,
                depth,
                visited,
                out,
            );
            let any_cfg = out[before..]
                .iter()
                .any(|id| self.cfg_gated_ids.contains(id.as_str()));
            if out.len() > before && !any_cfg {
                return;
            }
        }
        for mf in self.resolve_module_files(base_file, &mod_segs) {
            for hit in
                self.resolve_symbol_in_module(&mf, sym_name, filter, symbols, depth, visited)
            {
                push_unique(out, hit);
            }
        }
        // Check same-file inline module (e.g. `mod option_duration_secs { ... }` or `self::imp`).
        let inline_rel = mod_path.strip_prefix("self::").unwrap_or(mod_path);
        if !inline_rel.is_empty()
            && self
                .file_inline_scopes
                .get(base_file)
                .is_some_and(|scopes| scopes.contains(inline_rel))
        {
            self.resolve_symbol_in_inline_mod(
                base_file, inline_rel, sym_name, filter, symbols, depth, visited, out,
            );
        }
        // Check cross-file inline module (e.g. `crate::foo::inline_mod::helper`).
        if out.len() == before && mod_segs.len() >= 2 {
            let last_mod = mod_segs[mod_segs.len() - 1];
            if !matches!(last_mod, "crate" | "super" | "self") {
                for pf in self.resolve_module_files(base_file, &mod_segs[..mod_segs.len() - 1]) {
                    if self
                        .file_inline_scopes
                        .get(pf.as_str())
                        .is_some_and(|scopes| scopes.contains(last_mod))
                    {
                        self.resolve_symbol_in_inline_mod(
                            &pf, last_mod, sym_name, filter, symbols, depth, visited, out,
                        );
                    }
                }
            }
        }
    }

    fn resolve_symbol_in_module(
        &self,
        mod_file: &str,
        sym_name: &str,
        filter: RustSymFilter,
        symbols: &HashMap<String, Symbol>,
        depth: usize,
        visited: &mut HashSet<(String, String)>,
    ) -> Vec<String> {
        if depth > 4 || !visited.insert((mod_file.to_string(), sym_name.to_string())) {
            return Vec::new();
        }
        let kind_matches = |s: &Symbol| -> bool { filter.matches(s.kind) };
        let mut out = Vec::new();
        if let Some(ids) = self.file_non_method_syms.get(&(mod_file, sym_name)) {
            let hits: Vec<String> = ids
                .iter()
                .filter(|id| {
                    symbols.get(*id).is_some_and(|s| {
                        !s.is_test
                            && kind_matches(s)
                            && self
                                .symbol_mod_scopes
                                .get(id.as_str())
                                .is_none_or(|m| m.is_empty())
                    })
                })
                .cloned()
                .collect();
            if !hits.is_empty() {
                let any_cfg = hits
                    .iter()
                    .any(|id| self.cfg_gated_ids.contains(id.as_str()));
                if !any_cfg {
                    return hits;
                }
                for h in hits {
                    push_unique(&mut out, h);
                }
            }
        }
        let mut reexport_hits = Vec::new();
        if let Some(pub_uses) = self.file_pub_uses_by_local.get(&(mod_file, sym_name)) {
            for u in pub_uses {
                if let Some((mod_path, target_sym)) = u.target_path.rsplit_once("::") {
                    self.resolve_mod_path_into(
                        mod_file,
                        mod_path,
                        target_sym,
                        filter,
                        symbols,
                        depth + 1,
                        visited,
                        &mut reexport_hits,
                    );
                } else if u.target_path != sym_name {
                    for hit in self.resolve_symbol_in_module(
                        mod_file,
                        &u.target_path,
                        filter,
                        symbols,
                        depth + 1,
                        visited,
                    ) {
                        push_unique(&mut reexport_hits, hit);
                    }
                }
            }
        }
        for h in reexport_hits {
            push_unique(&mut out, h);
        }
        if !out.is_empty() {
            return out;
        }
        if let Some(glob_uses) = self.file_pub_uses_by_local.get(&(mod_file, "*")) {
            for u in glob_uses {
                self.resolve_mod_path_into(
                    mod_file,
                    &u.target_path,
                    sym_name,
                    filter,
                    symbols,
                    depth + 1,
                    visited,
                    &mut out,
                );
            }
        }
        out
    }

    /// Resolve a bare Rust function/const call (no receiver/qualifier):
    /// prefers same-file non-method definitions over methods and follows local `use` aliases
    /// (including `#[cfg(...)] use foo as bar;`) and imported `pub use` re-exports.
    pub fn resolve_bare_call(
        &self,
        caller_file: &str,
        caller_id: &str,
        target_name: &str,
        caller_is_test: bool,
        include_types: bool,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<(String, Confidence)> {
        let filter = if include_types {
            RustSymFilter::TypesAndConsts
        } else {
            RustSymFilter::ValuesOnly
        };
        let mut out: Vec<String> = Vec::new();
        let caller_scope = self
            .symbol_mod_scopes
            .get(caller_id)
            .map(String::as_str)
            .unwrap_or("");
        let sym_in_scope = |id: &str| -> bool {
            let sym_scope = self
                .symbol_mod_scopes
                .get(id)
                .map(String::as_str)
                .unwrap_or("");
            sym_scope == caller_scope
        };

        if let Some(ids) = self.file_non_method_syms.get(&(caller_file, target_name)) {
            for id in ids {
                if sym_in_scope(id)
                    && symbols.get(id).is_some_and(|s| {
                        filter.matches(s.kind) && (!s.is_test || caller_is_test)
                    })
                {
                    push_unique(&mut out, id.clone());
                }
            }
        }

        let (use_lookup_name, macro_suffix) = match target_name.strip_suffix('!') {
            Some(base) => (base, "!"),
            None => (target_name, ""),
        };
        if let Some(uses) = self.file_uses_by_local.get(&(caller_file, use_lookup_name)) {
            for u in uses {
                if u.mod_scope != caller_scope || (u.is_test && !caller_is_test) {
                    continue;
                }
                if let Some((mod_path, target_sym)) = u.target_path.rsplit_once("::") {
                    let target_sym_full = if macro_suffix.is_empty() {
                        target_sym.to_string()
                    } else {
                        format!("{}{}", target_sym, macro_suffix)
                    };
                    if mod_path == "self" && !caller_scope.is_empty() {
                        let before = out.len();
                        if let Some(ids) = self
                            .file_non_method_syms
                            .get(&(caller_file, target_sym_full.as_str()))
                        {
                            for id in ids {
                                if sym_in_scope(id)
                                    && symbols.get(id).is_some_and(|s| {
                                        filter.matches(s.kind) && (!s.is_test || caller_is_test)
                                    })
                                {
                                    push_unique(&mut out, id.clone());
                                }
                            }
                        }
                        // Also follow same-scope reimports (e.g. use super::helper; use self::helper as alias;).
                        if out.len() == before
                            && let Some(reimports) =
                                self.file_uses_by_local.get(&(caller_file, target_sym))
                        {
                            for u2 in reimports {
                                if u2.mod_scope == caller_scope
                                    && (!u2.is_test || caller_is_test)
                                    && u2.target_path != u.target_path
                                    && let Some((mp2, ts2)) = u2.target_path.rsplit_once("::")
                                {
                                    if mp2 == "super" {
                                        let parent_scope = caller_scope
                                            .rsplit_once("::")
                                            .map(|(p, _)| p)
                                            .unwrap_or("");
                                        let ts2_full = if macro_suffix.is_empty() {
                                            ts2.to_string()
                                        } else {
                                            format!("{}{}", ts2, macro_suffix)
                                        };
                                        if let Some(ids) = self
                                            .file_non_method_syms
                                            .get(&(caller_file, ts2_full.as_str()))
                                        {
                                            for id in ids {
                                                let sc = self
                                                    .symbol_mod_scopes
                                                    .get(id.as_str())
                                                    .map(String::as_str)
                                                    .unwrap_or("");
                                                if sc == parent_scope
                                                    && symbols.get(id).is_some_and(|s| {
                                                        filter.matches(s.kind)
                                                            && (!s.is_test || caller_is_test)
                                                    })
                                                {
                                                    push_unique(&mut out, id.clone());
                                                }
                                            }
                                        }
                                    } else if mp2 != "self" {
                                        let ts2_full = if macro_suffix.is_empty() {
                                            ts2.to_string()
                                        } else {
                                            format!("{}{}", ts2, macro_suffix)
                                        };
                                        let mut visited = HashSet::new();
                                        self.resolve_mod_path_into(
                                            caller_file,
                                            mp2,
                                            &ts2_full,
                                            filter,
                                            symbols,
                                            0,
                                            &mut visited,
                                            &mut out,
                                        );
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    let mut visited = HashSet::new();
                    self.resolve_mod_path_into(
                        caller_file,
                        mod_path,
                        &target_sym_full,
                        filter,
                        symbols,
                        0,
                        &mut visited,
                        &mut out,
                    );
                } else if u.target_path != use_lookup_name {
                    let target_sym_full = if macro_suffix.is_empty() {
                        u.target_path.clone()
                    } else {
                        format!("{}{}", u.target_path, macro_suffix)
                    };
                    if let Some(ids) = self
                        .file_non_method_syms
                        .get(&(caller_file, target_sym_full.as_str()))
                    {
                        for id in ids {
                            if sym_in_scope(id)
                                && symbols.get(id).is_some_and(|s| {
                                    filter.matches(s.kind) && (!s.is_test || caller_is_test)
                                })
                            {
                                push_unique(&mut out, id.clone());
                            }
                        }
                    }
                }
            }
        }
        if out.is_empty()
            && let Some(glob_uses) = self.file_uses_by_local.get(&(caller_file, "*"))
        {
            for u in glob_uses {
                if u.mod_scope != caller_scope || (u.is_test && !caller_is_test) {
                    continue;
                }
                let mut visited = HashSet::new();
                self.resolve_mod_path_into(
                    caller_file,
                    &u.target_path,
                    target_name,
                    filter,
                    symbols,
                    0,
                    &mut visited,
                    &mut out,
                );
            }
        }

        out.into_iter()
            .map(|id| (id, Confidence::Resolved))
            .collect()
    }

    /// Resolve a module- or crate-qualified Rust call (e.g. `codex_app_server_daemon::run`,
    /// `codex_app_server_daemon::start_with_features`, `auth::connect`) using Cargo.toml
    /// crate roots, local `use` module aliases, and `pub use` re-exports.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_qualified_path(
        &self,
        caller_file: &str,
        caller_id: &str,
        qual: &str,
        target_name: &str,
        caller_is_test: bool,
        include_types: bool,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<(String, Confidence)> {
        let filter = if include_types {
            RustSymFilter::TypesAndConsts
        } else {
            RustSymFilter::ValuesOnly
        };
        let raw_segs: Vec<&str> = qual.split("::").filter(|s| !s.is_empty()).collect();
        if raw_segs.is_empty() {
            return Vec::new();
        }
        let caller_scope = self
            .symbol_mod_scopes
            .get(caller_id)
            .map(String::as_str)
            .unwrap_or("");

        let mut alias_paths: Vec<Vec<&str>> = Vec::new();
        if let Some(uses) = self.file_uses_by_local.get(&(caller_file, raw_segs[0])) {
            for u in uses {
                if u.mod_scope == caller_scope
                    && (!u.is_test || caller_is_test)
                {
                    let mut expanded: Vec<&str> = u
                        .target_path
                        .split("::")
                        .filter(|s| !s.is_empty())
                        .collect();
                    expanded.extend_from_slice(&raw_segs[1..]);
                    if !expanded.is_empty() {
                        alias_paths.push(expanded);
                    }
                }
            }
        }

        let mut out: Vec<String> = Vec::new();
        let mut fallback_segs: &[&str] = &raw_segs;
        // When a caller inside an inline module uses `super::...`, consume leading `super`
        // segments against `caller_scope` first so `super::Config` resolves to the enclosing
        // same-file scope before climbing physical file modules.
        if !caller_scope.is_empty() && raw_segs.first() == Some(&"super") {
            let mut scope_parts: Vec<&str> = caller_scope.split("::").collect();
            let mut idx = 0;
            while idx < raw_segs.len() && raw_segs[idx] == "super" && !scope_parts.is_empty() {
                scope_parts.pop();
                idx += 1;
            }
            fallback_segs = &raw_segs[idx..];
            if idx == raw_segs.len() {
                let target_scope = scope_parts.join("::");
                if let Some(ids) = self.file_non_method_syms.get(&(caller_file, target_name)) {
                    for id in ids {
                        let sc = self
                            .symbol_mod_scopes
                            .get(id.as_str())
                            .map(String::as_str)
                            .unwrap_or("");
                        if sc == target_scope
                            && symbols.get(id).is_some_and(|s| {
                                filter.matches(s.kind) && (!s.is_test || caller_is_test)
                            })
                        {
                            push_unique(&mut out, id.clone());
                        }
                    }
                }
                if out.is_empty() && target_scope.is_empty() {
                    let mut visited = HashSet::new();
                    for hit in self.resolve_symbol_in_module(
                        caller_file,
                        target_name,
                        filter,
                        symbols,
                        0,
                        &mut visited,
                    ) {
                        push_unique(&mut out, hit);
                    }
                }
                if !out.is_empty() {
                    return out
                        .into_iter()
                        .map(|id| (id, Confidence::Resolved))
                        .collect();
                }
            }
        }
        // Collect matches across ALL matching alias expansions in the caller's scope
        // (e.g. mutually exclusive `#[cfg(unix)]` / `#[cfg(not(unix))]` module aliases).
        for segs in alias_paths {
            let expanded = segs.join("::");
            let mut visited = HashSet::new();
            self.resolve_mod_path_into(
                caller_file,
                &expanded,
                target_name,
                filter,
                symbols,
                0,
                &mut visited,
                &mut out,
            );
        }
        // Fall back to the raw qualifier path only if no alias expansion resolved.
        if out.is_empty() && !fallback_segs.is_empty() {
            let fallback_path = fallback_segs.join("::");
            let mut visited = HashSet::new();
            self.resolve_mod_path_into(
                caller_file,
                &fallback_path,
                target_name,
                filter,
                symbols,
                0,
                &mut visited,
                &mut out,
            );
        }

        out.into_iter()
            .map(|id| (id, Confidence::Resolved))
            .collect()
    }

    /// Resolve a method call against traits in scope in `caller_file` (same-file traits,
    /// explicitly imported traits, or glob-imported traits) whose method set contains `method_name`.
    pub fn resolve_extension_trait_methods(
        &self,
        caller_file: &str,
        caller_id: &str,
        method_name: &str,
        caller_is_test: bool,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<(String, Confidence)> {
        let Some(matching_traits) = self.traits_by_method_name.get(method_name) else {
            return Vec::new();
        };
        let caller_scope = self
            .symbol_mod_scopes
            .get(caller_id)
            .map(String::as_str)
            .unwrap_or("");
        let mut candidate_traits: Vec<String> = Vec::new();

        for tid in matching_traits {
            let Some(tsym) = symbols.get(tid) else {
                continue;
            };
            if tsym.is_test && !caller_is_test {
                continue;
            }
            let trait_scope = self
                .symbol_mod_scopes
                .get(tid.as_str())
                .map(String::as_str)
                .unwrap_or("");
            if tsym.file == caller_file && trait_scope == caller_scope {
                push_unique(&mut candidate_traits, tid.clone());
                continue;
            }
            let same_file_super_scope = if tsym.file == caller_file && !caller_scope.is_empty() {
                Some(caller_scope.rsplit_once("::").map(|(p, _)| p).unwrap_or(""))
            } else {
                None
            };
            let mut in_scope = false;
            if let Some(uses) = self
                .file_uses_by_local
                .get(&(caller_file, tsym.name.as_str()))
            {
                for u in uses {
                    let scope_ok = u.mod_scope == caller_scope
                        || (!caller_scope.is_empty() && u.mod_scope.is_empty());
                    if !scope_ok || (u.is_test && !caller_is_test) {
                        continue;
                    }
                    if let Some(parent_scope) = same_file_super_scope
                        && u.mod_scope == caller_scope
                        && trait_scope == parent_scope
                        && u.target_path == format!("super::{}", tsym.name)
                    {
                        in_scope = true;
                        break;
                    }
                    if let Some((mod_path, target_sym)) = u.target_path.rsplit_once("::") {
                        let mut hits = Vec::new();
                        let mut visited = HashSet::new();
                        self.resolve_mod_path_into(
                            caller_file,
                            mod_path,
                            target_sym,
                            RustSymFilter::TypesOnly,
                            symbols,
                            0,
                            &mut visited,
                            &mut hits,
                        );
                        if hits.iter().any(|h| h == tid) {
                            in_scope = true;
                            break;
                        }
                    }
                }
            }
            if !in_scope && let Some(glob_uses) = self.file_uses_by_local.get(&(caller_file, "*")) {
                for u in glob_uses {
                    let scope_ok = u.mod_scope == caller_scope
                        || (!caller_scope.is_empty() && u.mod_scope.is_empty());
                    if !scope_ok || (u.is_test && !caller_is_test) {
                        continue;
                    }
                    if let Some(parent_scope) = same_file_super_scope
                        && u.mod_scope == caller_scope
                        && trait_scope == parent_scope
                        && u.target_path == "super"
                    {
                        in_scope = true;
                        break;
                    }
                    let mut hits = Vec::new();
                    let mut visited = HashSet::new();
                    self.resolve_mod_path_into(
                        caller_file,
                        &u.target_path,
                        &tsym.name,
                        RustSymFilter::TypesOnly,
                        symbols,
                        0,
                        &mut visited,
                        &mut hits,
                    );
                    if hits.iter().any(|h| h == tid) {
                        in_scope = true;
                        break;
                    }
                }
            }
            if !in_scope && let Some(uses) = self.file_rust_uses.get(caller_file) {
                // Handle aliased trait imports (`use foo::MyExt as MyAlias;`).
                for u in *uses {
                    let scope_ok = u.mod_scope == caller_scope
                        || (!caller_scope.is_empty() && u.mod_scope.is_empty());
                    if !scope_ok || (u.is_test && !caller_is_test) || u.local_name == tsym.name {
                        continue;
                    }
                    if let Some(parent_scope) = same_file_super_scope
                        && u.mod_scope == caller_scope
                        && trait_scope == parent_scope
                        && u.target_path == format!("super::{}", tsym.name)
                    {
                        in_scope = true;
                        break;
                    }
                    if let Some((mod_path, target_sym)) = u.target_path.rsplit_once("::")
                        && target_sym == tsym.name
                    {
                        let mut hits = Vec::new();
                        let mut visited = HashSet::new();
                        self.resolve_mod_path_into(
                            caller_file,
                            mod_path,
                            target_sym,
                            RustSymFilter::TypesOnly,
                            symbols,
                            0,
                            &mut visited,
                            &mut hits,
                        );
                        if hits.iter().any(|h| h == tid) {
                            in_scope = true;
                            break;
                        }
                    }
                }
            }
            if in_scope {
                push_unique(&mut candidate_traits, tid.clone());
                if candidate_traits.len() > 4 {
                    return Vec::new();
                }
            }
        }

        candidate_traits
            .into_iter()
            .map(|id| (id, Confidence::Resolved))
            .collect()
    }

    fn resolve_field_states_on_type(
        &self,
        struct_id: &str,
        field_name: &str,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<RustChainState> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: std::collections::VecDeque<(String, usize)> =
            std::collections::VecDeque::from([(struct_id.to_string(), 0usize)]);
        while let Some((curr_id, depth)) = queue.pop_front() {
            if depth > 4 || !visited.insert(curr_id.clone()) {
                continue;
            }
            let Some(owner) = symbols.get(&curr_id) else {
                continue;
            };
            let imps = self
                .file_imports
                .get(owner.file.as_str())
                .copied()
                .unwrap_or(&[]);
            if let Some(raw_fty) = self.raw_struct_fields.get(&curr_id).and_then(|f| {
                f.get(field_name).or_else(|| {
                    if owner.kind == SymbolKind::Struct {
                        field_name
                            .rsplit_once("::")
                            .and_then(|(_, fname)| f.get(fname))
                    } else {
                        None
                    }
                })
            }) && !raw_fty.is_empty()
            {
                let scope = self
                    .symbol_mod_scopes
                    .get(&curr_id)
                    .map(String::as_str)
                    .unwrap_or("");
                return self.resolve_type_states_in_scope(
                    &owner.file,
                    &owner.package_or_module,
                    imps,
                    raw_fty,
                    scope,
                    symbols,
                );
            }
            if let Some(targets) = self.deref_target_ids.get(&curr_id) {
                for tid in targets {
                    queue.push_back((tid.clone(), depth + 1));
                }
            }
        }
        Vec::new()
    }

    /// Rust receiver method lookup: inherent methods win over trait methods at each Deref depth.
    pub fn lookup_methods_on_type(
        &self,
        type_id: &str,
        method_name: &str,
        caller_is_test: bool,
    ) -> Vec<String> {
        self.lookup_methods_on_type_inner(type_id, method_name, caller_is_test, true)
    }

    /// Direct associated-function lookup on a concrete type (no Deref traversal).
    pub fn lookup_direct_methods_on_type(
        &self,
        type_id: &str,
        method_name: &str,
        caller_is_test: bool,
    ) -> Vec<String> {
        self.lookup_methods_on_type_inner(type_id, method_name, caller_is_test, false)
    }

    fn lookup_methods_on_type_inner(
        &self,
        type_id: &str,
        method_name: &str,
        caller_is_test: bool,
        follow_deref: bool,
    ) -> Vec<String> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: std::collections::VecDeque<(String, usize)> =
            std::collections::VecDeque::from([(type_id.to_string(), 0usize)]);
        while let Some((curr_id, depth)) = queue.pop_front() {
            if depth > 4 || !visited.insert(curr_id.clone()) {
                continue;
            }
            if let Some(ids) = self
                .inherent_methods_by_type
                .get(&curr_id)
                .and_then(|m| m.get(method_name))
                && !ids.is_empty()
            {
                return ids.clone();
            }
            if caller_is_test
                && let Some(ids) = self
                    .test_methods_by_type
                    .get(&curr_id)
                    .and_then(|m| m.get(method_name))
                && !ids.is_empty()
            {
                return ids.clone();
            }
            if let Some(ids) = self
                .trait_methods_by_type
                .get(&curr_id)
                .and_then(|m| m.get(method_name))
                && !ids.is_empty()
            {
                return ids.clone();
            }
            if follow_deref && let Some(targets) = self.deref_target_ids.get(&curr_id) {
                for tid in targets {
                    queue.push_back((tid.clone(), depth + 1));
                }
            }
        }
        Vec::new()
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_func_call_states(
        &self,
        ext: &FileExtract,
        caller_id: &str,
        qualifier: Option<&str>,
        name: &str,
        result_index: usize,
        by_file_name: &HashMap<(&str, &str), String>,
        by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<RustChainState> {
        let fn_ids = self.resolve_callee_func_ids(
            ext,
            caller_id,
            qualifier,
            name,
            by_file_name,
            by_pkg_name,
            symbols,
        );
        let mut merged_states: Vec<RustChainState> = Vec::new();
        for fn_id in fn_ids {
            let Some(fn_sym) = symbols.get(&fn_id) else {
                continue;
            };
            let Some(ret_ty) = self
                .func_return_types
                .get(&fn_id)
                .and_then(|v| v.get(result_index))
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let imps = self
                .file_imports
                .get(fn_sym.file.as_str())
                .copied()
                .unwrap_or(&[]);
            let fn_scope = self
                .symbol_mod_scopes
                .get(&fn_id)
                .map(String::as_str)
                .unwrap_or("");
            let states = self.resolve_type_states_in_scope(
                &fn_sym.file,
                &fn_sym.package_or_module,
                imps,
                ret_ty,
                fn_scope,
                symbols,
            );
            for st in states {
                if merged_states.len() < 8 && !merged_states.contains(&st) {
                    merged_states.push(st);
                }
            }
        }
        if !merged_states.is_empty() {
            return merged_states;
        }
        if result_index == 0
            && let Some(q) = qualifier
        {
            let caller_is_test = symbols.get(caller_id).is_some_and(|s| s.is_test);
            let tids = self.resolve_direct_types(
                &ext.file,
                &ext.package_or_module,
                &ext.imports,
                caller_id,
                q,
                caller_is_test,
                symbols,
            );
            if !tids.is_empty()
                && (name == "new"
                    || name == "default"
                    || name == "from"
                    || name.starts_with("new_")
                    || name.starts_with("with_")
                    || name.starts_with("from_")
                    || matches!(name, "open" | "connect" | "build" | "init"))
            {
                let mut out = Vec::new();
                for tid in tids {
                    let methods = self.lookup_direct_methods_on_type(&tid, name, caller_is_test);
                    if methods.is_empty() {
                        let st = RustChainState::Type(tid, 0);
                        if out.len() < 8 && !out.contains(&st) {
                            out.push(st);
                        }
                    }
                }
                if !out.is_empty() {
                    return out;
                }
            }
        }
        // Tuple struct constructor call: e.g. SlotPair(a, b) or mod_a::SlotPair(a, b).
        if result_index == 0 && name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            let caller_scope = self
                .symbol_mod_scopes
                .get(caller_id)
                .map(String::as_str)
                .unwrap_or("");
            if qualifier.is_none()
                && let Some(ids) = self.file_non_method_syms.get(&(ext.file.as_str(), name))
            {
                let scoped: Vec<&String> = ids
                    .iter()
                    .filter(|id| {
                        self.symbol_mod_scopes
                            .get(id.as_str())
                            .map(String::as_str)
                            .unwrap_or("")
                            == caller_scope
                            && symbols
                                .get(*id)
                                .is_some_and(|s| !s.is_test && s.kind == SymbolKind::Struct)
                    })
                    .collect();
                if scoped.len() == 1 {
                    return vec![RustChainState::Type(scoped[0].clone(), 0)];
                }
            }
            if qualifier.is_none()
                && !caller_scope.is_empty()
                && let Some(tid) = self.resolve_imported_type_in_scope(
                    &ext.file,
                    name,
                    caller_scope,
                    false,
                    symbols,
                )
            {
                return vec![RustChainState::Type(tid, 0)];
            }
            if qualifier.is_some() || caller_scope.is_empty() {
                let type_name = match qualifier {
                    Some(q) => format!("{q}::{name}"),
                    None => name.to_string(),
                };
                let states = self.resolve_type_states_in_scope(
                    &ext.file,
                    &ext.package_or_module,
                    &ext.imports,
                    &type_name,
                    caller_scope,
                    symbols,
                );
                if !states.is_empty() {
                    return states;
                }
            }
        }
        Vec::new()
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_callee_func_ids(
        &self,
        ext: &FileExtract,
        caller_id: &str,
        qualifier: Option<&str>,
        name: &str,
        by_file_name: &HashMap<(&str, &str), String>,
        by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<String> {
        let caller_is_test = symbols.get(caller_id).is_some_and(|s| s.is_test);
        let is_callable_or_typed_val = |id: &String| -> bool {
            symbols
                .get(id)
                .map(|s| {
                    s.kind == SymbolKind::Function
                        || (matches!(s.kind, SymbolKind::Const | SymbolKind::Var)
                            && self.func_return_types.contains_key(id))
                })
                .unwrap_or(false)
        };
        if let Some(q) = qualifier {
            let tids = self.resolve_direct_types(
                &ext.file,
                &ext.package_or_module,
                &ext.imports,
                caller_id,
                q,
                caller_is_test,
                symbols,
            );
            if !tids.is_empty() {
                let mut methods = Vec::new();
                for tid in tids {
                    for mid in self.lookup_direct_methods_on_type(&tid, name, caller_is_test) {
                        push_unique(&mut methods, mid);
                    }
                }
                if !methods.is_empty() {
                    return methods;
                }
            }
            if q.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
                let hits = self.resolve_qualified_path(
                    &ext.file,
                    caller_id,
                    q,
                    name,
                    caller_is_test,
                    false,
                    symbols,
                );
                let mut fn_ids = Vec::new();
                for (fn_id, _) in hits {
                    push_unique(&mut fn_ids, fn_id);
                }
                if !fn_ids.is_empty() {
                    return fn_ids;
                }
                // Resolve inline module function calls (e.g. api::with_aliased_worker)
                // from file root scope without intercepting dot-calls or crate::-prefixed paths.
                let caller_scope = self
                    .symbol_mod_scopes
                    .get(caller_id)
                    .map(String::as_str)
                    .unwrap_or("");
                if caller_scope.is_empty()
                    && let Some(ids) = self.file_non_method_syms.get(&(ext.file.as_str(), name))
                {
                    let inline_hits: Vec<String> = ids
                        .iter()
                        .filter(|id| {
                            self.symbol_mod_scopes
                                .get(id.as_str())
                                .map(String::as_str)
                                .unwrap_or("")
                                == q
                                && symbols
                                    .get(*id)
                                    .is_some_and(|s| !s.is_test || caller_is_test)
                                && is_callable_or_typed_val(id)
                        })
                        .cloned()
                        .collect();
                    if !inline_hits.is_empty() {
                        return inline_hits;
                    }
                }
            }
            return Vec::new();
        }

        let mut bare_ids = Vec::new();
        for (id, _) in
            self.resolve_bare_call(&ext.file, caller_id, name, caller_is_test, false, symbols)
        {
            if is_callable_or_typed_val(&id) {
                push_unique(&mut bare_ids, id);
            }
        }
        if !bare_ids.is_empty() {
            return bare_ids;
        }
        if let Some(id) = by_file_name
            .get(&(ext.file.as_str(), name))
            .filter(|id| is_callable_or_typed_val(id))
        {
            return vec![id.clone()];
        }
        if let Some(id) = by_pkg_name
            .get(&(Language::Rust, ext.package_or_module.as_str(), name))
            .and_then(|ids| ids.iter().find(|id| is_callable_or_typed_val(id)))
        {
            return vec![id.clone()];
        }
        Vec::new()
    }

    fn resolve_callback_param_states_on_func(
        &self,
        fn_id: &str,
        arg_index: usize,
        param_index: usize,
        allow_ufcs_receiver_shift: bool,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<RustChainState> {
        let Some(fn_sym) = symbols.get(fn_id) else {
            return Vec::new();
        };
        let Some(entries) = self.func_callback_params.get(fn_id) else {
            return Vec::new();
        };
        let Some(param_ty) = entries
            .iter()
            .find(|(ai, pi, _)| *ai == arg_index && *pi == param_index)
            .or_else(|| {
                if allow_ufcs_receiver_shift
                    && fn_sym.kind == SymbolKind::Method
                    && let Some(shifted) = arg_index.checked_sub(1)
                {
                    entries
                        .iter()
                        .find(|(ai, pi, _)| *ai == shifted && *pi == param_index)
                } else {
                    None
                }
            })
            .map(|(_, _, ty)| ty.as_str())
            .filter(|s| !s.is_empty())
        else {
            return Vec::new();
        };
        let imps = self
            .file_imports
            .get(fn_sym.file.as_str())
            .copied()
            .unwrap_or(&[]);
        let fn_scope = self
            .symbol_mod_scopes
            .get(fn_id)
            .map(String::as_str)
            .unwrap_or("");
        self.resolve_type_states_in_scope(
            &fn_sym.file,
            &fn_sym.package_or_module,
            imps,
            param_ty,
            fn_scope,
            symbols,
        )
    }

    pub fn resolve_receiver_chains(
        &self,
        ext: &FileExtract,
        caller_id: &str,
        chain: &[ReceiverStep],
        by_file_name: &HashMap<(&str, &str), String>,
        by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<(String, usize)> {
        let mut iter = chain.iter();
        let Some(first) = iter.next() else {
            return Vec::new();
        };
        let caller_scope = self
            .symbol_mod_scopes
            .get(caller_id)
            .map(String::as_str)
            .unwrap_or("");
        let mut states: Vec<RustChainState> = match first {
            ReceiverStep::Type(ty) => self.resolve_type_states_in_scope(
                &ext.file,
                &ext.package_or_module,
                &ext.imports,
                ty,
                caller_scope,
                symbols,
            ),
            ReceiverStep::Call {
                qualifier,
                name,
                result_index,
            } => self.resolve_func_call_states(
                ext,
                caller_id,
                qualifier.as_deref(),
                name,
                *result_index,
                by_file_name,
                by_pkg_name,
                symbols,
            ),
            ReceiverStep::ClosureParam {
                qualifier,
                name,
                arg_index,
                param_index,
            } => {
                let fn_ids = self.resolve_callee_func_ids(
                    ext,
                    caller_id,
                    qualifier.as_deref(),
                    name,
                    by_file_name,
                    by_pkg_name,
                    symbols,
                );
                let mut merged = Vec::new();
                for fn_id in fn_ids {
                    for st in self.resolve_callback_param_states_on_func(
                        &fn_id,
                        *arg_index,
                        *param_index,
                        qualifier.is_some(),
                        symbols,
                    ) {
                        if merged.len() < 8 && !merged.contains(&st) {
                            merged.push(st);
                        }
                    }
                }
                merged
            }
            _ => return Vec::new(),
        };
        if states.is_empty() {
            return Vec::new();
        }

        let push_state = |next: &mut Vec<RustChainState>, st: RustChainState| {
            if next.len() < 8 && !next.contains(&st) {
                next.push(st);
            }
        };

        for step in iter {
            let mut next_states: Vec<RustChainState> = Vec::new();
            for st in &states {
                match step {
                    ReceiverStep::Field(fname) => {
                        if let RustChainState::Type(tid, 0) = st {
                            for nst in self.resolve_field_states_on_type(tid, fname, symbols) {
                                push_state(&mut next_states, nst);
                            }
                        }
                    }
                    ReceiverStep::Elem => match st {
                        RustChainState::Type(tid, d) if *d > 0 => {
                            push_state(&mut next_states, RustChainState::Type(tid.clone(), d - 1));
                        }
                        RustChainState::Tuple {
                            owner_file,
                            owner_pkg,
                            elems,
                            depth,
                        } if *depth > 0 => {
                            push_state(
                                &mut next_states,
                                RustChainState::Tuple {
                                    owner_file: owner_file.clone(),
                                    owner_pkg: owner_pkg.clone(),
                                    elems: elems.clone(),
                                    depth: depth - 1,
                                },
                            );
                        }
                        _ => {}
                    },
                    ReceiverStep::TupleElem(idx) => match st {
                        RustChainState::Tuple {
                            owner_file,
                            owner_pkg,
                            elems,
                            depth: 0,
                        } => {
                            if let Some(elem_ty) = elems.get(*idx)
                                && !elem_ty.is_empty()
                            {
                                let imps = self
                                    .file_imports
                                    .get(owner_file.as_str())
                                    .copied()
                                    .unwrap_or(&[]);
                                for nst in self.resolve_type_states_in_scope(
                                    owner_file, owner_pkg, imps, elem_ty, "", symbols,
                                ) {
                                    push_state(&mut next_states, nst);
                                }
                            }
                        }
                        // Preserve `for (k, v) in map` where HashMap<K, V> normalized to `[]V`.
                        RustChainState::Type(tid, d) if *idx == 1 => {
                            push_state(&mut next_states, RustChainState::Type(tid.clone(), *d));
                        }
                        _ => {}
                    },
                    ReceiverStep::MethodCall { name, result_index } => {
                        let depth = match st {
                            RustChainState::Type(_, d) => *d,
                            RustChainState::Tuple { depth, .. } => *depth,
                        };
                        // First, if the receiver is a concrete local type with a matching local method,
                        // always respect that local method's declared return type.
                        if let RustChainState::Type(tid, 0) = st {
                            let methods = self.lookup_methods_on_type(tid, name, false);
                            if let Some(mid) = methods.first() {
                                if let Some(msym) = symbols.get(mid)
                                    && let Some(ret_ty) = self
                                        .func_return_types
                                        .get(mid)
                                        .and_then(|v| v.get(*result_index))
                                        .filter(|s| !s.is_empty())
                                {
                                    let imps = self
                                        .file_imports
                                        .get(msym.file.as_str())
                                        .copied()
                                        .unwrap_or(&[]);
                                    let mscope = self
                                        .symbol_mod_scopes
                                        .get(mid)
                                        .map(String::as_str)
                                        .unwrap_or("");
                                    for nst in self.resolve_type_states_in_scope(
                                        &msym.file,
                                        &msym.package_or_module,
                                        imps,
                                        ret_ty,
                                        mscope,
                                        symbols,
                                    ) {
                                        push_state(&mut next_states, nst);
                                    }
                                }
                                continue;
                            }
                        }
                        // Fallback for stdlib/smart-pointer/collection wrapper methods when no local
                        // method of that name exists on the receiver type.
                        if depth > 0 {
                            if matches!(
                                name.as_str(),
                                "iter"
                                    | "iter_mut"
                                    | "into_iter"
                                    | "values"
                                    | "values_mut"
                                    | "into_values"
                                    | "entry"
                                    | "enumerate"
                                    | "rev"
                                    | "take"
                                    | "skip"
                                    | "filter"
                                    | "drain"
                                    | "copied"
                                    | "cloned"
                                    | "peekable"
                                    | "cycle"
                                    | "step_by"
                            ) {
                                push_state(&mut next_states, st.clone());
                                continue;
                            }
                            if matches!(
                                name.as_str(),
                                "get"
                                    | "get_mut"
                                    | "first"
                                    | "last"
                                    | "pop"
                                    | "next"
                                    | "remove"
                                    | "or_default"
                                    | "or_insert"
                                    | "or_insert_with"
                                    | "push_mut"
                            ) {
                                let popped = match st {
                                    RustChainState::Type(tid, d) => {
                                        RustChainState::Type(tid.clone(), d - 1)
                                    }
                                    RustChainState::Tuple {
                                        owner_file,
                                        owner_pkg,
                                        elems,
                                        depth: d,
                                    } => RustChainState::Tuple {
                                        owner_file: owner_file.clone(),
                                        owner_pkg: owner_pkg.clone(),
                                        elems: elems.clone(),
                                        depth: d - 1,
                                    },
                                };
                                push_state(&mut next_states, popped);
                                continue;
                            }
                        }
                        if is_transparent_wrapper_method(name) || (depth == 0 && name == "get_mut")
                        {
                            push_state(&mut next_states, st.clone());
                        }
                    }
                    ReceiverStep::ClosureParam {
                        qualifier: None,
                        name,
                        arg_index,
                        param_index,
                    } => {
                        if let RustChainState::Type(tid, 0) = st {
                            let methods = self.lookup_methods_on_type(tid, name, false);
                            for mid in &methods {
                                let res = self.resolve_callback_param_states_on_func(
                                    mid,
                                    *arg_index,
                                    *param_index,
                                    false,
                                    symbols,
                                );
                                if !res.is_empty() {
                                    for nst in res {
                                        push_state(&mut next_states, nst);
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    ReceiverStep::Type(_)
                    | ReceiverStep::Call { .. }
                    | ReceiverStep::ClosureParam { .. } => {}
                }
            }
            if next_states.is_empty() {
                return Vec::new();
            }
            states = next_states;
        }

        let mut out = Vec::new();
        for st in states {
            if let RustChainState::Type(tid, d) = st {
                let item = (tid, d);
                if !out.contains(&item) {
                    out.push(item);
                }
            }
        }
        out
    }
}

fn split_tuple_norm_elems(inner: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in inner.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(inner[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(inner[start..].trim().to_string());
    parts
}

fn push_unique(out: &mut Vec<String>, id: String) {
    if !out.contains(&id) {
        out.push(id);
    }
}

fn normalize_rel_path_join(base_dir: &str, rel_path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in base_dir.split('/').chain(rel_path.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn is_transparent_wrapper_method(name: &str) -> bool {
    matches!(
        name,
        "lock"
            | "try_lock"
            | "blocking_lock"
            | "read"
            | "try_read"
            | "blocking_read"
            | "write"
            | "try_write"
            | "blocking_write"
            | "borrow"
            | "borrow_mut"
            | "try_borrow"
            | "try_borrow_mut"
            | "clone"
            | "cloned"
            | "copied"
            | "as_ref"
            | "as_mut"
            | "as_deref"
            | "as_deref_mut"
            | "unwrap"
            | "expect"
            | "unwrap_or"
            | "unwrap_or_else"
            | "unwrap_or_default"
            | "ok"
            | "ok_or"
            | "ok_or_else"
            | "upgrade"
            | "inspect"
            | "inspect_err"
            | "map_err"
            | "context"
            | "with_context"
            | "into_inner"
            | "deref"
            | "deref_mut"
            | "get_ref"
            | "take"
            | "pin"
            | "load"
            | "load_full"
            | "get_or_init"
            | "recv"
            | "try_recv"
            | "blocking_recv"
            | "flatten"
    )
}

fn is_external_rust_crate_path(path: &str) -> bool {
    let first = path.split("::").next().unwrap_or("");
    matches!(
        first,
        "std"
            | "core"
            | "alloc"
            | "serde"
            | "tokio"
            | "futures"
            | "futures_core"
            | "futures_util"
            | "async_trait"
            | "ratatui"
            | "tower"
            | "axum"
            | "tonic"
            | "clap"
            | "anyhow"
            | "thiserror"
            | "tracing"
            | "tracing_subscriber"
            | "http"
            | "hyper"
            | "bytes"
            | "chrono"
            | "uuid"
            | "url"
            | "reqwest"
            | "sqlx"
            | "rusqlite"
            | "rmcp"
            | "schemars"
    )
}

fn symbol_matches_rust_mod_qual(sym: &Symbol, mod_qual: &str) -> bool {
    let qual_slash = mod_qual.replace("::", "/");
    let file = sym.file.as_str();
    if file == format!("{}.rs", qual_slash)
        || file.ends_with(&format!("/{}.rs", qual_slash))
        || file == format!("{}/mod.rs", qual_slash)
        || file.ends_with(&format!("/{}/mod.rs", qual_slash))
        || file == format!("{}/lib.rs", qual_slash)
        || file.ends_with(&format!("/{}/lib.rs", qual_slash))
    {
        return true;
    }
    let pkg_ident = sym
        .package_or_module
        .rsplit('/')
        .next()
        .unwrap_or("")
        .replace('-', "_");
    !pkg_ident.is_empty()
        && (pkg_ident == mod_qual || qual_slash.starts_with(&format!("{}/", pkg_ident)))
}

pub fn rust_imports_match_symbol(imports: &[String], sym: &Symbol) -> bool {
    let crate_seg = sym
        .package_or_module
        .rsplit('/')
        .next()
        .unwrap_or(&sym.package_or_module);
    let crate_ident = crate_seg.replace('-', "_");
    let file_stem = sym
        .file
        .rsplit('/')
        .next()
        .unwrap_or("")
        .strip_suffix(".rs")
        .unwrap_or("");
    let parent_dir = sym
        .file
        .rsplit_once('/')
        .and_then(|(d, _)| d.rsplit('/').next())
        .unwrap_or("");
    imports.iter().any(|imp| {
        (!crate_ident.is_empty() && imp.contains(&crate_ident))
            || (!file_stem.is_empty()
                && file_stem != "lib"
                && file_stem != "mod"
                && file_stem != "main"
                && imp.contains(file_stem))
            || ((file_stem == "mod" || file_stem == "lib")
                && !parent_dir.is_empty()
                && parent_dir != "src"
                && imp.contains(&parent_dir.replace('-', "_")))
    })
}

pub fn resolve_rust_module_qualified(
    ext: &FileExtract,
    qual: &str,
    target_name: &str,
    by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
    by_lang_name: &HashMap<(Language, &str), Vec<String>>,
    include_types: bool,
    symbols: &HashMap<String, Symbol>,
) -> Vec<(String, Confidence)> {
    let filter = if include_types {
        RustSymFilter::TypesAndConsts
    } else {
        RustSymFilter::ValuesOnly
    };
    if !ext.package_or_module.is_empty()
        && let Some(ids) =
            by_pkg_name.get(&(Language::Rust, ext.package_or_module.as_str(), target_name))
    {
        let narrowed: Vec<String> = ids
            .iter()
            .filter(|id| {
                symbols
                    .get(*id)
                    .map(|s| {
                        filter.matches(s.kind) && symbol_matches_rust_mod_qual(s, qual)
                    })
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if !narrowed.is_empty() && narrowed.len() <= 4 {
            return narrowed
                .into_iter()
                .map(|id| (id, Confidence::Resolved))
                .collect();
        }
    }

    if let Some(ids) = by_lang_name.get(&(Language::Rust, target_name)) {
        let narrowed: Vec<String> = ids
            .iter()
            .filter(|id| {
                symbols
                    .get(*id)
                    .map(|s| {
                        filter.matches(s.kind) && symbol_matches_rust_mod_qual(s, qual)
                    })
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if !narrowed.is_empty() && narrowed.len() <= 4 {
            return narrowed
                .into_iter()
                .map(|id| (id, Confidence::Resolved))
                .collect();
        }
    }

    // Same-file exact inline module match (e.g. mod_a::scoped_cfg), checked after file modules
    // so nested inline modules cannot intercept top-level file modules like worker.rs.
    if !ext.cfg_gated_symbols.is_empty() {
        let inline_hits: Vec<String> = ext
            .symbols
            .iter()
            .filter(|s| {
                s.name == target_name
                    && filter.matches(s.kind)
                    && ext
                        .cfg_gated_symbols
                        .get(&s.id)
                        .is_some_and(|scope| scope == qual)
            })
            .map(|s| s.id.clone())
            .collect();
        if !inline_hits.is_empty() {
            return inline_hits
                .into_iter()
                .map(|id| (id, Confidence::Resolved))
                .collect();
        }
    }

    Vec::new()
}

#[allow(clippy::too_many_arguments)]
pub fn materialize_rust_trait_and_macro_edges(
    extracts: &[FileExtract],
    rust_index: &RustTypeIndex<'_>,
    symbols: &HashMap<String, Symbol>,
    by_file_name: &HashMap<(&str, &str), String>,
    by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
    by_lang_name: &HashMap<(Language, &str), Vec<String>>,
    edges: &mut Vec<Edge>,
    seen: &mut FxHashSet<(String, String, EdgeKind)>,
) -> Vec<InterfaceImpl> {
    let mut interface_impls = Vec::new();

    let mut push_edge =
        |from: &str, to: &str, kind: EdgeKind, conf: Confidence, file: &str, line: usize| {
            if from == to {
                return;
            }
            if seen.insert((from.to_string(), to.to_string(), kind)) {
                edges.push(Edge {
                    from: from.to_string(),
                    to: to.to_string(),
                    kind,
                    confidence: conf,
                    file: file.to_string(),
                    line,
                    cross_lang: false,
                    via_bridge: None,
                });
            }
        };

    for ext in extracts {
        if ext.lang != Language::Rust {
            continue;
        }

        for imp in &ext.trait_impls {
            if imp.is_test || imp.method_ids.is_empty() {
                continue;
            }
            let type_id = rust_index.resolve_type(
                &ext.file,
                &ext.package_or_module,
                &ext.imports,
                &imp.type_name,
                symbols,
            );
            let local_trait_id = rust_index.resolve_trait(
                &ext.file,
                &ext.package_or_module,
                &ext.imports,
                &imp.trait_path,
                &imp.trait_name,
                symbols,
            );

            match (type_id, local_trait_id) {
                (Some(tid), Some(tr_id)) => {
                    // Calling an explicit trait-impl method directly on a concrete receiver
                    // (e.g. `h.run()`) uses both the trait and the receiver type.
                    for mid in &imp.method_ids {
                        if symbols.get(mid).is_some_and(|s| {
                            matches!(s.kind, SymbolKind::Method | SymbolKind::Const)
                        }) {
                            let line = symbols.get(mid).map(|s| s.span.start_line).unwrap_or(1);
                            push_edge(mid, &tr_id, EdgeKind::TypeRef, Confidence::Resolved, &ext.file, line);
                            push_edge(mid, &tid, EdgeKind::TypeRef, Confidence::Resolved, &ext.file, line);
                        }
                    }
                    interface_impls.push(InterfaceImpl {
                        receiver_type_id: tid,
                        interface_id: tr_id,
                        method_ids: imp.method_ids.clone(),
                    });
                }
                (None, Some(tr_id)) => {
                    for mid in &imp.method_ids {
                        let line = symbols.get(mid).map(|s| s.span.start_line).unwrap_or(1);
                        push_edge(
                            &tr_id,
                            mid,
                            EdgeKind::Implements,
                            Confidence::Resolved,
                            &ext.file,
                            line,
                        );
                    }
                }
                (Some(tid), None) => {
                    for mid in &imp.method_ids {
                        let line = symbols.get(mid).map(|s| s.span.start_line).unwrap_or(1);
                        push_edge(
                            &tid,
                            mid,
                            EdgeKind::Implements,
                            Confidence::Resolved,
                            &ext.file,
                            line,
                        );
                    }
                }
                (None, None) => {
                    for arg_ty in &imp.trait_type_args {
                        if let Some(arg_tid) = rust_index.resolve_type(
                            &ext.file,
                            &ext.package_or_module,
                            &ext.imports,
                            arg_ty,
                            symbols,
                        ) {
                            for mid in &imp.method_ids {
                                let line = symbols.get(mid).map(|s| s.span.start_line).unwrap_or(1);
                                push_edge(
                                    &arg_tid,
                                    mid,
                                    EdgeKind::Implements,
                                    Confidence::Resolved,
                                    &ext.file,
                                    line,
                                );
                            }
                        }
                    }
                }
            }
        }

        for (type_name, macro_name, line) in &ext.impl_macros {
            let Some(tid) = rust_index.resolve_type(
                &ext.file,
                &ext.package_or_module,
                &ext.imports,
                type_name,
                symbols,
            ) else {
                continue;
            };
            let macro_id = by_file_name
                .get(&(ext.file.as_str(), macro_name.as_str()))
                .filter(|id| {
                    symbols
                        .get(*id)
                        .map(|s| s.kind == SymbolKind::Const && !s.is_test)
                        .unwrap_or(false)
                })
                .cloned()
                .or_else(|| {
                    by_pkg_name
                        .get(&(
                            Language::Rust,
                            ext.package_or_module.as_str(),
                            macro_name.as_str(),
                        ))
                        .and_then(|ids| {
                            ids.iter().find(|id| {
                                symbols
                                    .get(*id)
                                    .map(|s| s.kind == SymbolKind::Const && !s.is_test)
                                    .unwrap_or(false)
                            })
                        })
                        .cloned()
                })
                .or_else(|| {
                    by_lang_name
                        .get(&(Language::Rust, macro_name.as_str()))
                        .and_then(|ids| {
                            let non_test: Vec<&String> = ids
                                .iter()
                                .filter(|id| {
                                    symbols
                                        .get(*id)
                                        .map(|s| s.kind == SymbolKind::Const && !s.is_test)
                                        .unwrap_or(false)
                                })
                                .collect();
                            if non_test.len() == 1 {
                                Some(non_test[0].clone())
                            } else {
                                None
                            }
                        })
                });
            if let Some(mid) = macro_id {
                push_edge(
                    &tid,
                    &mid,
                    EdgeKind::Calls,
                    Confidence::Resolved,
                    &ext.file,
                    *line,
                );
            }
        }
    }

    interface_impls
}
