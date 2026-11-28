# kagami

Polyglot static analysis CLI for **cross-language blast-radius**, **dead-code reachability**, and **canonical-IR structural similarity**.

Supports Go, Rust, JavaScript/TypeScript, Svelte, `justfile`, and Bash/POSIX shell scripts.

## Installation

### Via Cargo (from Git)

```bash
cargo install --git https://github.com/mekugi-harness/kagami
```

### From a Local Checkout

```bash
just install        # or: cargo install --path .
```

### Prebuilt Binaries

Download release tarballs and SHA256 checksums for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and `aarch64-apple-darwin` from [GitHub Releases](https://github.com/mekugi-harness/kagami/releases).

## Quickstart

### Blast Radius

Trace direct and transitive callers, callees, and cross-language bridges for a symbol:

```bash
kagami blast-radius src/main.rs main
kagami blast-radius src/graph/blast.rs analyze_blast_radius --depth 2 --direction callers
kagami blast-radius src/main.rs main --no-tests --format json
```

### Dead-Code Reachability

Find unreferenced symbols and unreachable call clusters via root-seeded reachability:

```bash
kagami dead
kagami dead --scope all --lang rust,go
kagami dead --no-clusters --format json
```

### Structural Similarity

Find syntactically distinct but structurally equivalent or near-duplicate functions via canonical IR lowering:

```bash
kagami similar
kagami similar --min-similarity 0.85 --min-nodes 12
kagami similar --cross-lang --format json
```

### Compiler-Exact SCIP Overlay

Pass one or more SCIP index files (`.scip` protobuf or `.scip.json`) with the global `--scip` flag to overlay compiler-exact edges (`Confidence::Exact`):

```bash
kagami --scip index.scip blast-radius src/main.rs main
kagami --scip go.scip --scip rust.scip.json dead
```

### Common Global Flags

- `--root <dir>` — Repository root (default: `.`).
- `--path <glob>` — Repeatable relative path glob filter restricting reported results.
- `--no-cache` — Bypass the incremental binary caches under `.kagami/` (`cache.bin`, `manifest.bin`, `graph.bin`).
- `--scip <path>` — Repeatable SCIP index file (`.scip` or `.scip.json`).
- `--format <md|json>` — Output format (default: `md`).

## Configuration

### `.kagami.toml`

Place a `.kagami.toml` file at your repository root (see [`.kagami.toml.example`](.kagami.toml.example)):

```toml
ignore = [
  "vendor/**",
  "dist/**",
  "**/generated/**",
]

[dead]
allowlist = ".deadcode-allowlist"
```

### `.deadcode-allowlist`

`kagami dead` automatically loads `.deadcode-allowlist` at the repository root (or the path configured via `[dead].allowlist` or `--allowlist <path>`). Each non-empty, non-`#` line treats matching symbols as live roots:

```text
# Match by bare symbol name, qualified name, or relative_file:symbol_name
init_plugin
Server::health_check
src/hooks.rs:register_hooks
```

## Development Workflow

Requires Rust (stable, edition 2024) and optionally [`just`](https://github.com/casey/just):

```bash
just check        # Run fmt-check, lint, and test
just test         # Run unit and integration tests
just self-check   # Run kagami against its own repository
just lint         # Run cargo clippy --all-targets
just fmt          # Format Rust sources
just release      # Build optimized release binary
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution guidelines.

## Documentation

- [Architecture Spec](docs/SPEC-0001-architecture.md)
- [CLI Contract](docs/CONTRACT-cli.md)
- [Canonical IR Contract](docs/CONTRACT-ir.md)
- [ADR-0001: Polyglot Graph & Indexing](docs/ADR-0001-polyglot-graph-and-indexing.md)
- [ADR-0002: Cross-Language Bridges](docs/ADR-0002-cross-language-bridges.md)
- [ADR-0003: Canonical IR & Similarity](docs/ADR-0003-canonical-ir-and-similarity.md)

## License

Licensed under the [MIT License](LICENSE).
