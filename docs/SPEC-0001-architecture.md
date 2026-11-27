# SPEC-0001: kagami Architecture Specification

## 1. Pipeline Overview
1. **Discovery (`walker.rs`)**: Walks repository root using `ignore::WalkBuilder`, respecting `.gitignore`, `.ignore`, and skipping known metadata directories (`.git`, `.svn`, `.hg`, `node_modules`, `target`, `vendor`, `.kagami`). Optional `--path <glob>` filters analysis targets.
2. **Incremental Cache (`cache.rs`)**: Computes `blake3` hash per file. Unchanged files reuse cached `FileExtract` (symbols, references, bridge annotations, canonical IR functions).
3. **Language Frontends (`lang/`)**:
   - `go.rs`: packages, functions, methods (receiver-qualified), types/structs/interfaces, calls, imports, `json:"..."` tags, RPC method strings, IR lowering.
   - `rust.rs`: modules, functions, impl methods, structs/enums/traits, calls, `use` imports, `#[serde(rename)]`, `#[test]`, RPC method strings, IR lowering.
   - `ts_js.rs`: functions, arrow-function bindings, classes/methods, interfaces/types, imports/exports, calls, RPC strings, IR lowering.
   - `svelte.rs`: splits `<script>` blocks to `ts_js.rs` with line remapping; extracts template component tags, event handlers, and runes.
   - `bash.rs`: shell functions, sourced files, external command invocations, env exports/reads, IR lowering.
   - `justfile.rs`: recipes, dependencies, aliases, variables, exported env vars; delegates recipe bodies to `bash.rs`.
4. **Graph & Bridges (`graph/`)**:
   - Resolves intra-language references by file-local scope → package/module/import scope → global unique-name fallback (tagged `heuristic`).
   - Materializes `WireContract`, `ExecTarget`, and `EnvKey` bridge nodes linking symbols across languages.
5. **Analyzers**:
   - `blast.rs`: Bounded BFS over incoming/outgoing edges from target symbol(s), grouping impacts by hop distance, language, edge kind, and confidence.
   - `dead.rs`: Root-seeded mark-and-sweep reachability across the unified graph (including cross-language bridges), separating `unreferenced` symbols from `unreachable_cluster` cycles and honoring `.deadcode-allowlist`.
   - `ir/similar.rs`: Canonicalizes IR, indexes exact hashes + WL/CFG MinHash LSH, scores candidate pairs, and formats JSON or Markdown tables.

## 2. Configuration (`.kagami.toml`, optional)
- `ignore`: additional glob patterns to exclude.
- `dead.allowlist`: path to allowlist file (defaults to `.deadcode-allowlist` if present).
- `dead.include_tests`: default scope (`prod` vs `all`).
- `bridges.wire_prefixes`: optional RPC namespace prefixes.
