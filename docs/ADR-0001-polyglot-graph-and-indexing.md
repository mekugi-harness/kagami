# ADR-0001: Polyglot Graph & Incremental Indexing

## Status
Accepted

## Context
Target repositories (such as `mekugi`) combine thousands of files across Rust, Go, TypeScript/JavaScript, Svelte, `justfile`, and Bash/POSIX shell scripts. Running full language servers (`rust-analyzer`, `gopls`, `tsserver`) on every invocation is slow, requires language-specific build environments, and still leaves `svelte`, `justfile`, and `sh` unindexed.

## Decision
1. **Standalone Rust CLI binary** using native `tree-sitter` grammars and `rayon` parallelism as the always-on Layer 1 extractor.
2. **Svelte** is handled by splitting `<script>` / `<script module>` blocks (parsed with TypeScript/JavaScript grammars with exact line-offset remapping) and scanning template markup for component tags, event handlers, and Svelte 5 runes.
3. **Justfile** is parsed by a dedicated recipe/variable/alias frontend whose recipe bodies are delegated to `tree-sitter-bash`, giving full AST and IR coverage of shell commands inside recipes.
4. **File discovery** uses the `ignore` crate to honor `.gitignore`, `.ignore`, and skip VCS/meta directories (`.git`, `.svn`, `.hg`, `node_modules`, `target`, `vendor`, `.kagami`) by default, while supporting `--path <glob>` filtering.
5. **Content-addressed incremental cache** keyed by `blake3(file_content) + parser_version` stored under `.kagami/cache.json` (behind a `CacheStore` abstraction so SQLite can be swapped in if repositories exceed ~50k files).
6. **Optional Layer 2 precision overlay**: Every graph edge carries a `confidence` (`exact | resolved | heuristic`) and `source` (`scip | scope_graph | bridge`), allowing optional SCIP index ingestion without coupling the core binary to compilers.

## Consequences
- Cold index of ~8,000 files finishes in a few seconds; warm incremental queries finish in milliseconds.
- Tree-sitter scope resolution cannot prove all dynamic-dispatch edges (Go interfaces, Rust `dyn Trait`, proc-macros); those are explicitly marked with lower confidence rather than silently dropped or over-claimed.
