use crate::graph::InterfaceImpl;
use crate::model::{
    Confidence, Edge, EdgeKind, FileExtract, Language, ReceiverStep, Symbol, SymbolKind,
};
use std::collections::{HashMap, HashSet};

pub struct RustTypeIndex<'a> {
    file_imports: HashMap<&'a str, &'a [String]>,
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
    pub trait_methods: HashMap<String, HashSet<String>>,
    cfg_sibling_groups: HashMap<String, Vec<String>>,
}

impl<'a> RustTypeIndex<'a> {
    pub fn build(extracts: &'a [FileExtract], symbols: &HashMap<String, Symbol>) -> Self {
        let mut file_imports: HashMap<&'a str, &'a [String]> = HashMap::new();
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
        let mut trait_methods: HashMap<String, HashSet<String>> = HashMap::new();
        let mut trait_method_ids: HashSet<&str> = HashSet::new();
        let mut cfg_gated_scopes: HashMap<&str, &str> = HashMap::new();

        for ext in extracts {
            if ext.lang != Language::Rust {
                continue;
            }
            file_imports.insert(ext.file.as_str(), ext.imports.as_slice());
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

        let mut idx = Self {
            file_imports,
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
        // Handle module-qualified type paths (e.g. `remote::Worker` or `foo::bar::Worker`).
        if let Some((mod_qual, short_name)) = type_name.rsplit_once("::") {
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
        let cands = self.types_by_name.get(type_name)?;
        if cands.len() == 1 {
            return Some(cands[0].clone());
        }
        let matched: Vec<&String> = cands
            .iter()
            .filter(|id| {
                symbols
                    .get(*id)
                    .map(|s| rust_imports_match_symbol(owner_imports, s))
                    .unwrap_or(false)
            })
            .collect();
        if matched.len() == 1 {
            return Some(matched[0].clone());
        }
        None
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
                    .map(|s| rust_imports_match_symbol(owner_imports, s))
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
        mut raw_type: &str,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<(String, usize)> {
        let mut depth = 0usize;
        while let Some(rest) = raw_type.strip_prefix("[]") {
            depth += 1;
            raw_type = rest;
        }
        let tid = self.resolve_type(owner_file, owner_pkg, owner_imports, raw_type, symbols)?;
        Some((tid, depth))
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
            if let Some(raw_fty) = self
                .raw_struct_fields
                .get(&curr_id)
                .and_then(|f| f.get(field_name))
                && !raw_fty.is_empty()
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
            if let Some(targets) = self.deref_target_ids.get(&curr_id) {
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
        qualifier: Option<&str>,
        name: &str,
        result_index: usize,
        by_file_name: &HashMap<(&str, &str), String>,
        by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<(String, usize)> {
        if let Some(q) = qualifier {
            if let Some(tid) =
                self.resolve_type(&ext.file, &ext.package_or_module, &ext.imports, q, symbols)
            {
                let methods = self.lookup_methods_on_type(&tid, name, false);
                if let Some(mid) = methods.first()
                    && let Some(msym) = symbols.get(mid)
                    && let Some(ret_ty) = self
                        .func_return_types
                        .get(mid)
                        .and_then(|v| v.get(result_index))
                        .filter(|s| !s.is_empty())
                {
                    let imps = self
                        .file_imports
                        .get(msym.file.as_str())
                        .copied()
                        .unwrap_or(&[]);
                    if let Some(res) = self.resolve_type_with_slices(
                        &msym.file,
                        &msym.package_or_module,
                        imps,
                        ret_ty,
                        symbols,
                    ) {
                        return Some(res);
                    }
                }
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
            return None;
        }

        let fn_id = by_file_name
            .get(&(ext.file.as_str(), name))
            .filter(|id| {
                symbols
                    .get(*id)
                    .map(|s| s.kind == SymbolKind::Function)
                    .unwrap_or(false)
            })
            .cloned()
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
            })?;
        let fn_sym = symbols.get(&fn_id)?;
        let ret_ty = self
            .func_return_types
            .get(&fn_id)
            .and_then(|v| v.get(result_index))
            .filter(|s| !s.is_empty())?;
        let imps = self
            .file_imports
            .get(fn_sym.file.as_str())
            .copied()
            .unwrap_or(&[]);
        self.resolve_type_with_slices(
            &fn_sym.file,
            &fn_sym.package_or_module,
            imps,
            ret_ty,
            symbols,
        )
    }

    pub fn resolve_receiver_chain(
        &self,
        ext: &FileExtract,
        chain: &[ReceiverStep],
        by_file_name: &HashMap<(&str, &str), String>,
        by_pkg_name: &HashMap<(Language, &str, &str), Vec<String>>,
        symbols: &HashMap<String, Symbol>,
    ) -> Option<(String, usize)> {
        let mut iter = chain.iter();
        let first = iter.next()?;
        let mut state = match first {
            ReceiverStep::Type(ty) => self.resolve_type_with_slices(
                &ext.file,
                &ext.package_or_module,
                &ext.imports,
                ty,
                symbols,
            )?,
            ReceiverStep::Call {
                qualifier,
                name,
                result_index,
            } => self.resolve_func_call(
                ext,
                qualifier.as_deref(),
                name,
                *result_index,
                by_file_name,
                by_pkg_name,
                symbols,
            )?,
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
                    if is_transparent_wrapper_method(name) {
                        continue;
                    }
                    if state.1 > 0 {
                        if matches!(
                            name.as_str(),
                            "iter" | "iter_mut" | "into_iter" | "values" | "values_mut"
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
                    return None;
                }
                ReceiverStep::Type(_) | ReceiverStep::Call { .. } => return None,
            }
        }
        Some(state)
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
            | "ok"
            | "into_inner"
            | "deref"
            | "deref_mut"
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
