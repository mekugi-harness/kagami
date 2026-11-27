use crate::graph::InterfaceImpl;
use crate::model::{
    Confidence, Edge, EdgeKind, FileExtract, Language, ReceiverStep, RustUseItem, Symbol,
    SymbolKind,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct CargoCrateInfo {
    /// Directory containing the crate's root module (e.g. "codex-rs/app-server-daemon/src").
    pub src_dir: String,
    /// Normalized library entry file path relative to repo root (e.g. "codex-rs/app-server-daemon/src/lib.rs").
    pub lib_file: String,
}

/// Discover Rust crate names from Cargo.toml files ([lib].name and [package].name)
/// and map each Rust crate identifier (with '-' replaced by '_') to its crate root info.
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
                    let clean_lib_path = lib_path.trim_start_matches("./").trim_matches('/');
                    let lib_file = if dir.is_empty() {
                        clean_lib_path.to_string()
                    } else if clean_lib_path.is_empty() {
                        format!("{}/src/lib.rs", dir)
                    } else {
                        format!("{}/{}", dir, clean_lib_path)
                    };
                    let lib_dir = clean_lib_path
                        .rsplit_once('/')
                        .map(|(d, _)| d.trim_matches('/'))
                        .unwrap_or("");
                    let src_root = match (dir.is_empty(), lib_dir.is_empty()) {
                        (true, true) => String::new(),
                        (true, false) => lib_dir.to_string(),
                        (false, true) => dir.clone(),
                        (false, false) => format!("{}/{}", dir, lib_dir),
                    };
                    let info = CargoCrateInfo {
                        src_dir: src_root,
                        lib_file,
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

pub struct RustTypeIndex<'a> {
    file_imports: HashMap<&'a str, &'a [String]>,
    known_files: HashSet<&'a str>,
    crate_roots: HashMap<String, CargoCrateInfo>,
    file_to_crate_root: HashMap<&'a str, String>,
    crate_root_to_idents: HashMap<String, Vec<String>>,
    type_aliases: HashMap<String, String>,
    symbol_mod_scopes: HashMap<String, String>,
    file_non_method_syms: HashMap<(&'a str, &'a str), Vec<String>>,
    file_pub_uses: HashMap<&'a str, Vec<&'a RustUseItem>>,
    file_all_uses: HashMap<&'a str, Vec<&'a RustUseItem>>,
    types_by_file_name: HashMap<(&'a str, &'a str), String>,
    types_by_pkg_name: HashMap<(&'a str, &'a str), String>,
    types_by_name: HashMap<&'a str, Vec<String>>,
    traits_by_file_name: HashMap<(&'a str, &'a str), String>,
    traits_by_pkg_name: HashMap<(&'a str, &'a str), String>,
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
    cfg_sibling_groups: HashMap<String, Vec<String>>,
}

impl<'a> RustTypeIndex<'a> {
    pub fn build(
        extracts: &'a [FileExtract],
        symbols: &HashMap<String, Symbol>,
        crate_roots: HashMap<String, CargoCrateInfo>,
    ) -> Self {
        let mut file_imports: HashMap<&'a str, &'a [String]> = HashMap::new();
        let mut known_files: HashSet<&'a str> = HashSet::new();
        let mut type_aliases: HashMap<String, String> = HashMap::new();
        let mut symbol_mod_scopes: HashMap<String, String> = HashMap::new();
        let mut file_non_method_syms: HashMap<(&'a str, &'a str), Vec<String>> = HashMap::new();
        let mut file_pub_uses: HashMap<&'a str, Vec<&'a RustUseItem>> = HashMap::new();
        let mut file_all_uses: HashMap<&'a str, Vec<&'a RustUseItem>> = HashMap::new();
        let mut types_by_file_name: HashMap<(&'a str, &'a str), String> = HashMap::new();
        let mut types_by_pkg_name: HashMap<(&'a str, &'a str), String> = HashMap::new();
        let mut types_by_name: HashMap<&'a str, Vec<String>> = HashMap::new();
        let mut traits_by_file_name: HashMap<(&'a str, &'a str), String> = HashMap::new();
        let mut traits_by_pkg_name: HashMap<(&'a str, &'a str), String> = HashMap::new();
        let mut traits_by_name: HashMap<&'a str, Vec<String>> = HashMap::new();
        let mut raw_struct_fields: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut raw_deref_by_name: HashMap<(&'a str, &'a str, String), Vec<String>> =
            HashMap::new();
        let mut func_return_types: HashMap<String, Vec<String>> = HashMap::new();
        let mut func_callback_params: HashMap<String, Vec<(usize, usize, String)>> = HashMap::new();
        let mut trait_methods: HashMap<String, HashSet<String>> = HashMap::new();
        let mut trait_method_ids: HashSet<&str> = HashSet::new();
        let mut cfg_gated_scopes: HashMap<&str, &str> = HashMap::new();

        for ext in extracts {
            if ext.lang != Language::Rust {
                continue;
            }
            file_imports.insert(ext.file.as_str(), ext.imports.as_slice());
            known_files.insert(ext.file.as_str());
            for (k, v) in &ext.type_aliases {
                type_aliases.insert(k.clone(), v.clone());
            }
            for (k, v) in &ext.symbol_mod_scopes {
                symbol_mod_scopes.insert(k.clone(), v.clone());
            }
            for u in &ext.rust_uses {
                file_all_uses.entry(ext.file.as_str()).or_default().push(u);
                if u.is_pub && !u.is_test && u.mod_scope.is_empty() {
                    file_pub_uses.entry(ext.file.as_str()).or_default().push(u);
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
                trait_methods.insert(k.clone(), v.iter().cloned().collect());
            }
            for imp in &ext.trait_impls {
                for mid in &imp.method_ids {
                    trait_method_ids.insert(mid.as_str());
                }
            }
            for (id, scope) in &ext.cfg_gated_symbols {
                cfg_gated_scopes.insert(id.as_str(), scope.as_str());
            }
            for sym in &ext.symbols {
                if sym.kind != SymbolKind::Method {
                    file_non_method_syms
                        .entry((ext.file.as_str(), sym.name.as_str()))
                        .or_default()
                        .push(sym.id.clone());
                }
                if matches!(
                    sym.kind,
                    SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Type | SymbolKind::Trait
                ) {
                    types_by_file_name
                        .entry((ext.file.as_str(), sym.name.as_str()))
                        .or_insert_with(|| sym.id.clone());
                    types_by_pkg_name
                        .entry((sym.package_or_module.as_str(), sym.name.as_str()))
                        .or_insert_with(|| sym.id.clone());
                    types_by_name
                        .entry(sym.name.as_str())
                        .or_default()
                        .push(sym.id.clone());
                }
                if sym.kind == SymbolKind::Trait {
                    traits_by_file_name
                        .entry((ext.file.as_str(), sym.name.as_str()))
                        .or_insert_with(|| sym.id.clone());
                    traits_by_pkg_name
                        .entry((sym.package_or_module.as_str(), sym.name.as_str()))
                        .or_insert_with(|| sym.id.clone());
                    traits_by_name
                        .entry(sym.name.as_str())
                        .or_default()
                        .push(sym.id.clone());
                }
            }
        }

        let mut crate_root_to_idents: HashMap<String, Vec<String>> = HashMap::new();
        for (ident, info) in &crate_roots {
            crate_root_to_idents
                .entry(info.src_dir.clone())
                .or_default()
                .push(ident.clone());
        }
        let mut file_to_crate_root: HashMap<&'a str, String> = HashMap::new();
        for &file in &known_files {
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

        let mut idx = Self {
            file_imports,
            known_files,
            crate_roots,
            file_to_crate_root,
            crate_root_to_idents,
            type_aliases,
            symbol_mod_scopes,
            file_non_method_syms,
            file_pub_uses,
            file_all_uses,
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
            cfg_sibling_groups: HashMap::new(),
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

    /// Resolve a type name for associated function calls (e.g. `Alias::new()`),
    /// following only direct type aliases without flattening smart-pointer wrappers like `Arc<T>`.
    pub fn resolve_direct_type(
        &self,
        owner_file: &str,
        owner_pkg: &str,
        owner_imports: &[String],
        type_name: &str,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<String> {
        let tid =
            self.resolve_type_raw(owner_file, owner_pkg, owner_imports, type_name, symbols)?;
        let (unwrapped, extra_depth) = self.unwrap_type_alias(tid.clone(), symbols, false);
        if extra_depth == 0 {
            Some(unwrapped)
        } else {
            Some(tid)
        }
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
        // Handle module-qualified type paths (e.g. `remote::Worker` or `foo::bar::Worker`).
        if let Some((mod_qual, short_name)) = type_name.rsplit_once("::") {
            let mod_segs: Vec<&str> = mod_qual.split("::").filter(|s| !s.is_empty()).collect();
            if !mod_segs.is_empty() {
                let mut visited = HashSet::new();
                for mf in self.resolve_module_files(owner_file, &mod_segs) {
                    let hits = self.resolve_symbol_in_module(
                        &mf,
                        short_name,
                        true,
                        symbols,
                        0,
                        &mut visited,
                    );
                    if hits.len() == 1 {
                        return Some(hits[0].clone());
                    }
                }
            }
            let cands = self.types_by_name.get(short_name)?;
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
                return Some(matched[0].clone());
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
                return Some(same_pkg[0].clone());
            }
            // Support reexported types (e.g. api::Worker reexporting impl_mod::Worker)
            // only when there is a single unique definition in the entire repository.
            if cands.len() == 1 {
                return Some(cands[0].clone());
            }
            return None;
        }
        if let Some(id) = self.types_by_file_name.get(&(owner_file, type_name)) {
            return Some(id.clone());
        }
        if !owner_pkg.is_empty()
            && let Some(id) = self.types_by_pkg_name.get(&(owner_pkg, type_name))
        {
            return Some(id.clone());
        }
        if let Some(uses) = self.file_all_uses.get(owner_file) {
            for u in uses {
                if u.local_name == type_name
                    && !u.is_test
                    && (u.mod_scope == caller_scope || u.mod_scope.is_empty())
                    && let Some((mod_path, target_sym)) = u.target_path.rsplit_once("::")
                {
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
                            if hits.len() == 1 {
                                return Some(hits[0].clone());
                            }
                        }
                    }
                    let mut hits = Vec::new();
                    let mut visited = HashSet::new();
                    self.resolve_mod_path_into(
                        owner_file,
                        mod_path,
                        target_sym,
                        true,
                        symbols,
                        0,
                        &mut visited,
                        &mut hits,
                    );
                    if hits.len() == 1 {
                        return Some(hits.remove(0));
                    }
                }
            }
        }
        let cands = self.types_by_name.get(type_name)?;
        if cands.len() == 1 {
            return Some(cands[0].clone());
        }
        let matched: Vec<&String> = cands
            .iter()
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
                } else if let Some(id) = self.traits_by_file_name.get(&(owner_file, short_name)) {
                    // Inline module trait (e.g. mod tasks { pub trait Task }) in the same file
                    Some(id.clone())
                } else {
                    None
                };
            }
        }
        if let Some(id) = self.traits_by_file_name.get(&(owner_file, trait_name)) {
            return Some(id.clone());
        }
        if !owner_pkg.is_empty()
            && let Some(id) = self.traits_by_pkg_name.get(&(owner_pkg, trait_name))
        {
            return Some(id.clone());
        }
        let cands = self.traits_by_name.get(trait_name)?;
        if cands.len() == 1 {
            return Some(cands[0].clone());
        }
        let matched: Vec<&String> = cands
            .iter()
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

    fn resolve_type_with_slices(
        &self,
        owner_file: &str,
        owner_pkg: &str,
        owner_imports: &[String],
        raw_type: &str,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<(String, usize)> {
        self.resolve_type_with_slices_in_scope(
            owner_file,
            owner_pkg,
            owner_imports,
            raw_type,
            "",
            symbols,
        )
    }

    fn resolve_type_with_slices_in_scope(
        &self,
        owner_file: &str,
        owner_pkg: &str,
        owner_imports: &[String],
        mut raw_type: &str,
        caller_scope: &str,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<(String, usize)> {
        let mut depth = 0usize;
        while let Some(rest) = raw_type.strip_prefix("[]") {
            depth += 1;
            raw_type = rest;
        }
        let tid = self.resolve_type_raw_in_scope(
            owner_file,
            owner_pkg,
            owner_imports,
            raw_type,
            caller_scope,
            symbols,
        )?;
        let (unwrapped, extra_depth) = self.unwrap_type_alias(tid, symbols, true);
        Some((unwrapped, depth + extra_depth))
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
        let mut out = Vec::new();
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
            return out;
        }
        if first == "super" {
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
            return out;
        }
        if first == "self" {
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
            return out;
        }
        if let Some(crate_info) = self.crate_roots.get(first) {
            if segs.len() == 1 {
                if self.known_files.contains(crate_info.lib_file.as_str())
                    && !out.contains(&crate_info.lib_file)
                {
                    out.push(crate_info.lib_file.clone());
                }
            } else {
                self.push_module_candidates(&crate_info.src_dir, &segs[1..], &mut out);
            }
            return out;
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
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_mod_path_into(
        &self,
        base_file: &str,
        mod_path: &str,
        sym_name: &str,
        only_types: bool,
        symbols: &HashMap<String, Symbol>,
        depth: usize,
        visited: &mut HashSet<(String, String)>,
        out: &mut Vec<String>,
    ) {
        let mod_segs: Vec<&str> = mod_path.split("::").filter(|s| !s.is_empty()).collect();
        for mf in self.resolve_module_files(base_file, &mod_segs) {
            for hit in
                self.resolve_symbol_in_module(&mf, sym_name, only_types, symbols, depth, visited)
            {
                push_unique(out, hit);
            }
        }
    }

    fn resolve_symbol_in_module(
        &self,
        mod_file: &str,
        sym_name: &str,
        only_types: bool,
        symbols: &HashMap<String, Symbol>,
        depth: usize,
        visited: &mut HashSet<(String, String)>,
    ) -> Vec<String> {
        if depth > 4 || !visited.insert((mod_file.to_string(), sym_name.to_string())) {
            return Vec::new();
        }
        let kind_matches = |s: &Symbol| -> bool {
            if only_types {
                matches!(
                    s.kind,
                    SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Type | SymbolKind::Trait
                )
            } else {
                matches!(s.kind, SymbolKind::Function | SymbolKind::Const)
            }
        };
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
                return hits;
            }
        }
        let Some(pub_uses) = self.file_pub_uses.get(mod_file) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for u in pub_uses {
            if u.local_name != sym_name {
                continue;
            }
            if let Some((mod_path, target_sym)) = u.target_path.rsplit_once("::") {
                self.resolve_mod_path_into(
                    mod_file,
                    mod_path,
                    target_sym,
                    only_types,
                    symbols,
                    depth + 1,
                    visited,
                    &mut out,
                );
            } else if u.target_path != sym_name {
                for hit in self.resolve_symbol_in_module(
                    mod_file,
                    &u.target_path,
                    only_types,
                    symbols,
                    depth + 1,
                    visited,
                ) {
                    push_unique(&mut out, hit);
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
        for u in pub_uses {
            if u.local_name != "*" {
                continue;
            }
            self.resolve_mod_path_into(
                mod_file,
                &u.target_path,
                sym_name,
                only_types,
                symbols,
                depth + 1,
                visited,
                &mut out,
            );
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
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<(String, Confidence)> {
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
                        matches!(s.kind, SymbolKind::Function | SymbolKind::Const)
                            && (!s.is_test || caller_is_test)
                    })
                {
                    push_unique(&mut out, id.clone());
                }
            }
        }

        if let Some(uses) = self.file_all_uses.get(caller_file) {
            let (use_lookup_name, macro_suffix) = match target_name.strip_suffix('!') {
                Some(base) => (base, "!"),
                None => (target_name, ""),
            };
            for u in uses {
                if u.local_name != use_lookup_name
                    || u.mod_scope != caller_scope
                    || (u.is_test && !caller_is_test)
                {
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
                                        matches!(s.kind, SymbolKind::Function | SymbolKind::Const)
                                            && (!s.is_test || caller_is_test)
                                    })
                                {
                                    push_unique(&mut out, id.clone());
                                }
                            }
                        }
                        // Also follow same-scope reimports (e.g. use super::helper; use self::helper as alias;).
                        if out.len() == before {
                            for u2 in uses {
                                if u2.local_name == target_sym
                                    && u2.mod_scope == caller_scope
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
                                                        matches!(
                                                            s.kind,
                                                            SymbolKind::Function
                                                                | SymbolKind::Const
                                                        ) && (!s.is_test || caller_is_test)
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
                                            false,
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
                        false,
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
                                    matches!(s.kind, SymbolKind::Function | SymbolKind::Const)
                                        && (!s.is_test || caller_is_test)
                                })
                            {
                                push_unique(&mut out, id.clone());
                            }
                        }
                    }
                }
            }
            if out.is_empty() {
                for u in uses {
                    if u.local_name != "*"
                        || u.mod_scope != caller_scope
                        || (u.is_test && !caller_is_test)
                    {
                        continue;
                    }
                    let mut visited = HashSet::new();
                    self.resolve_mod_path_into(
                        caller_file,
                        &u.target_path,
                        target_name,
                        false,
                        symbols,
                        0,
                        &mut visited,
                        &mut out,
                    );
                }
            }
        }

        out.into_iter()
            .map(|id| (id, Confidence::Resolved))
            .collect()
    }

    /// Resolve a module- or crate-qualified Rust call (e.g. `codex_app_server_daemon::run`,
    /// `codex_app_server_daemon::start_with_features`, `auth::connect`) using Cargo.toml
    /// crate roots, local `use` module aliases, and `pub use` re-exports.
    pub fn resolve_qualified_path(
        &self,
        caller_file: &str,
        caller_id: &str,
        qual: &str,
        target_name: &str,
        caller_is_test: bool,
        symbols: &HashMap<String, Symbol>,
    ) -> Vec<(String, Confidence)> {
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
        if let Some(uses) = self.file_all_uses.get(caller_file) {
            for u in uses {
                if u.local_name == raw_segs[0]
                    && u.mod_scope == caller_scope
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
        // Collect matches across ALL matching alias expansions in the caller's scope
        // (e.g. mutually exclusive `#[cfg(unix)]` / `#[cfg(not(unix))]` module aliases).
        for segs in alias_paths {
            let mut visited = HashSet::new();
            for mf in self.resolve_module_files(caller_file, &segs) {
                for hit in
                    self.resolve_symbol_in_module(&mf, target_name, false, symbols, 0, &mut visited)
                {
                    push_unique(&mut out, hit);
                }
            }
        }
        // Fall back to the raw qualifier path only if no alias expansion resolved.
        if out.is_empty() {
            let mut visited = HashSet::new();
            for mf in self.resolve_module_files(caller_file, &raw_segs) {
                for hit in
                    self.resolve_symbol_in_module(&mf, target_name, false, symbols, 0, &mut visited)
                {
                    push_unique(&mut out, hit);
                }
            }
        }

        out.into_iter()
            .map(|id| (id, Confidence::Resolved))
            .collect()
    }

    fn resolve_field_on_type(
        &self,
        struct_id: &str,
        field_name: &str,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<(String, usize)> {
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
                return self.resolve_type_with_slices(
                    &owner.file,
                    &owner.package_or_module,
                    imps,
                    raw_fty,
                    symbols,
                );
            }
            if let Some(targets) = self.deref_target_ids.get(&curr_id) {
                for tid in targets {
                    queue.push_back((tid.clone(), depth + 1));
                }
            }
        }
        None
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
    fn resolve_func_call(
        &self,
        ext: &FileExtract,
        caller_id: &str,
        qualifier: Option<&str>,
        name: &str,
        result_index: usize,
        by_file_name: &HashMap<(&str, &str), String>,
        by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<(String, usize)> {
        if let Some(fn_id) = self.resolve_callee_func_id(
            ext,
            caller_id,
            qualifier,
            name,
            by_file_name,
            by_pkg_name,
            symbols,
        ) {
            let fn_sym = symbols.get(&fn_id)?;
            if let Some(ret_ty) = self
                .func_return_types
                .get(&fn_id)
                .and_then(|v| v.get(result_index))
                .filter(|s| !s.is_empty())
            {
                let imps = self
                    .file_imports
                    .get(fn_sym.file.as_str())
                    .copied()
                    .unwrap_or(&[]);
                if let Some(res) = self.resolve_type_with_slices(
                    &fn_sym.file,
                    &fn_sym.package_or_module,
                    imps,
                    ret_ty,
                    symbols,
                ) {
                    return Some(res);
                }
            }
        }
        if let Some(q) = qualifier
            && let Some(tid) =
                self.resolve_type(&ext.file, &ext.package_or_module, &ext.imports, q, symbols)
        {
            let methods = self.lookup_methods_on_type(&tid, name, false);
            if methods.is_empty()
                && (name == "new"
                    || name == "default"
                    || name == "from"
                    || name.starts_with("new_")
                    || name.starts_with("with_")
                    || name.starts_with("from_")
                    || matches!(name, "open" | "connect" | "build" | "init"))
            {
                return Some((tid, 0));
            }
        }
        // Tuple struct constructor call: e.g. SlotPair(a, b) or mod_a::SlotPair(a, b).
        if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
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
                    return Some((scoped[0].clone(), 0));
                }
            }
            if qualifier.is_some() || caller_scope.is_empty() {
                let type_name = match qualifier {
                    Some(q) => format!("{q}::{name}"),
                    None => name.to_string(),
                };
                if let Some(res) = self.resolve_type_with_slices_in_scope(
                    &ext.file,
                    &ext.package_or_module,
                    &ext.imports,
                    &type_name,
                    caller_scope,
                    symbols,
                ) {
                    return Some(res);
                }
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_callee_func_id(
        &self,
        ext: &FileExtract,
        caller_id: &str,
        qualifier: Option<&str>,
        name: &str,
        by_file_name: &HashMap<(&str, &str), String>,
        by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<String> {
        if let Some(q) = qualifier {
            if let Some(tid) =
                self.resolve_type(&ext.file, &ext.package_or_module, &ext.imports, q, symbols)
            {
                let methods = self.lookup_methods_on_type(&tid, name, false);
                if let Some(mid) = methods.first() {
                    return Some(mid.clone());
                }
            }
            if q.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
                let hits =
                    self.resolve_qualified_path(&ext.file, caller_id, q, name, false, symbols);
                if let Some((fn_id, _)) = hits.first() {
                    return Some(fn_id.clone());
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
                    let inline_hits: Vec<&String> = ids
                        .iter()
                        .filter(|id| {
                            self.symbol_mod_scopes
                                .get(id.as_str())
                                .map(String::as_str)
                                .unwrap_or("")
                                == q
                                && symbols
                                    .get(*id)
                                    .is_some_and(|s| !s.is_test && s.kind == SymbolKind::Function)
                        })
                        .collect();
                    if inline_hits.len() == 1 {
                        return Some(inline_hits[0].clone());
                    }
                }
            }
            return None;
        }

        self.resolve_bare_call(&ext.file, caller_id, name, false, symbols)
            .into_iter()
            .map(|(id, _)| id)
            .find(|id| {
                symbols
                    .get(id)
                    .map(|s| s.kind == SymbolKind::Function)
                    .unwrap_or(false)
            })
            .or_else(|| {
                by_file_name
                    .get(&(ext.file.as_str(), name))
                    .filter(|id| {
                        symbols
                            .get(*id)
                            .map(|s| s.kind == SymbolKind::Function)
                            .unwrap_or(false)
                    })
                    .cloned()
            })
            .or_else(|| {
                by_pkg_name
                    .get(&(Language::Rust, ext.package_or_module.as_str(), name))
                    .and_then(|ids| {
                        ids.iter().find(|id| {
                            symbols
                                .get(*id)
                                .map(|s| s.kind == SymbolKind::Function)
                                .unwrap_or(false)
                        })
                    })
                    .cloned()
            })
    }

    fn resolve_callback_param_on_func(
        &self,
        fn_id: &str,
        arg_index: usize,
        param_index: usize,
        allow_ufcs_receiver_shift: bool,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<(String, usize)> {
        let fn_sym = symbols.get(fn_id)?;
        let entries = self.func_callback_params.get(fn_id)?;
        let param_ty = entries
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
            .filter(|s| !s.is_empty())?;
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
        self.resolve_type_with_slices_in_scope(
            &fn_sym.file,
            &fn_sym.package_or_module,
            imps,
            param_ty,
            fn_scope,
            symbols,
        )
    }

    pub fn resolve_receiver_chain(
        &self,
        ext: &FileExtract,
        caller_id: &str,
        chain: &[ReceiverStep],
        by_file_name: &HashMap<(&str, &str), String>,
        by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<(String, usize)> {
        let mut iter = chain.iter();
        let first = iter.next()?;
        let caller_scope = self
            .symbol_mod_scopes
            .get(caller_id)
            .map(String::as_str)
            .unwrap_or("");
        let mut state = match first {
            ReceiverStep::Type(ty) => self.resolve_type_with_slices_in_scope(
                &ext.file,
                &ext.package_or_module,
                &ext.imports,
                ty,
                caller_scope,
                symbols,
            )?,
            ReceiverStep::Call {
                qualifier,
                name,
                result_index,
            } => self.resolve_func_call(
                ext,
                caller_id,
                qualifier.as_deref(),
                name,
                *result_index,
                by_file_name,
                by_pkg_name,
                symbols,
            )?,
            ReceiverStep::ClosureParam {
                qualifier,
                name,
                arg_index,
                param_index,
            } => {
                let fn_id = self.resolve_callee_func_id(
                    ext,
                    caller_id,
                    qualifier.as_deref(),
                    name,
                    by_file_name,
                    by_pkg_name,
                    symbols,
                )?;
                self.resolve_callback_param_on_func(
                    &fn_id,
                    *arg_index,
                    *param_index,
                    qualifier.is_some(),
                    symbols,
                )?
            }
            _ => return None,
        };

        for step in iter {
            match step {
                ReceiverStep::Field(fname) => {
                    if state.1 != 0 {
                        return None;
                    }
                    state = self.resolve_field_on_type(&state.0, fname, symbols)?;
                }
                ReceiverStep::Elem => {
                    if state.1 == 0 {
                        return None;
                    }
                    state.1 -= 1;
                }
                ReceiverStep::MethodCall { name, result_index } => {
                    // First, if the receiver is a concrete local type with a matching local method,
                    // always respect that local method's declared return type.
                    if state.1 == 0 {
                        let methods = self.lookup_methods_on_type(&state.0, name, false);
                        if let Some(mid) = methods.first() {
                            let msym = symbols.get(mid)?;
                            let ret_ty = self
                                .func_return_types
                                .get(mid)
                                .and_then(|v| v.get(*result_index))
                                .filter(|s| !s.is_empty())?;
                            let imps = self
                                .file_imports
                                .get(msym.file.as_str())
                                .copied()
                                .unwrap_or(&[]);
                            state = self.resolve_type_with_slices(
                                &msym.file,
                                &msym.package_or_module,
                                imps,
                                ret_ty,
                                symbols,
                            )?;
                            continue;
                        }
                    }
                    // Fallback for stdlib/smart-pointer/collection wrapper methods when no local
                    // method of that name exists on the receiver type.
                    if state.1 > 0 {
                        if matches!(
                            name.as_str(),
                            "iter"
                                | "iter_mut"
                                | "into_iter"
                                | "values"
                                | "values_mut"
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
                            continue;
                        }
                        if matches!(
                            name.as_str(),
                            "get" | "get_mut" | "first" | "last" | "pop" | "next" | "remove"
                        ) {
                            state.1 -= 1;
                            continue;
                        }
                    }
                    if is_transparent_wrapper_method(name) || (state.1 == 0 && name == "get_mut") {
                        continue;
                    }
                    return None;
                }
                ReceiverStep::ClosureParam {
                    qualifier: None,
                    name,
                    arg_index,
                    param_index,
                } => {
                    if state.1 != 0 {
                        return None;
                    }
                    let methods = self.lookup_methods_on_type(&state.0, name, false);
                    let mut resolved = None;
                    for mid in &methods {
                        if let Some(res) = self.resolve_callback_param_on_func(
                            mid,
                            *arg_index,
                            *param_index,
                            false,
                            symbols,
                        ) {
                            resolved = Some(res);
                            break;
                        }
                    }
                    state = resolved?;
                }
                ReceiverStep::Type(_)
                | ReceiverStep::Call { .. }
                | ReceiverStep::ClosureParam { .. } => return None,
            }
        }
        Some(state)
    }
}

fn push_unique(out: &mut Vec<String>, id: String) {
    if !out.contains(&id) {
        out.push(id);
    }
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
    symbols: &HashMap<String, Symbol>,
) -> Vec<(String, Confidence)> {
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
                        matches!(s.kind, SymbolKind::Function | SymbolKind::Const)
                            && symbol_matches_rust_mod_qual(s, qual)
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
                        matches!(s.kind, SymbolKind::Function | SymbolKind::Const)
                            && symbol_matches_rust_mod_qual(s, qual)
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
                    && matches!(s.kind, SymbolKind::Function | SymbolKind::Const)
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
    seen: &mut HashSet<(String, String, EdgeKind)>,
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
