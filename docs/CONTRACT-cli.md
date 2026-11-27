# CONTRACT: CLI Interface

## Global Flags
- `--root <dir>`: Repository root (default: current directory).
- `--path <glob>`: Repeatable glob filter restricting reported results (or indexed scope) to matching relative paths.
- `--no-cache`: Bypass `.kagami/cache.json`.
- `--scip <path>`: Repeatable path to SCIP index files (`.scip` protobuf or `.scip.json`) to overlay compiler-exact edges (`Confidence::Exact`).
- `--format <md|json>`: Output format (command-specific default: `md`).

## Commands

### 1. `kagami blast-radius <file_path> <symbol>`
Returns the resolved symbol, its direct references, and transitive relationships across languages.
- `<file_path>`: Path relative to `--root` (or absolute path inside `--root`).
- `<symbol>`: Symbol name, `Type.Method`, or `Type::method`.
- `--depth <N>`: Max traversal hops (default: `3`; `0` = unbounded).
- `--direction <callers|callees|both>`: Traversal direction (default: `both`).
- `--no-cross-lang`: Disable cross-language bridge traversal.
- `--include-env`: Include `EnvKey` bridge edges.
- `--no-tests`: Exclude test files/functions from results.
- `--format <md|json>`: Output format (default: `md`).
- **Exit codes**: `0` on success, `2` if file or symbol not found.

### 2. `kagami dead`
Returns symbols with no reachable call site.
- `--scope <prod|all>`: `prod` excludes test callers from keeping production code alive (default: `prod`); `all` treats tests as live roots.
- `--lang <go,rust,ts,js,svelte,just,sh>`: Restrict reported symbols to selected languages.
- `--allowlist <path>`: Allowlist file (auto-detects `.deadcode-allowlist` at repo root if present).
- `--include-clusters`: Also report unreachable call cycles (`unreachable_cluster`, default: `true`).
- `--min-confidence <heuristic|resolved|exact>`: Minimum confidence level to report (default: `heuristic`; use `resolved` to filter out dynamic-dispatch / interface / trait `Heuristic` candidates).
- `--format <md|json>`: Output format (default: `md`).

### 3. `kagami similar`
Returns a JSON array or Markdown table of structurally similar functions/recipes.
- `--min-similarity <0.0-1.0>`: Minimum similarity score (default: `0.85`).
- `--min-nodes <N>`: Minimum canonical IR node count to consider (default: `12`).
- `--cross-lang`: Compare functions across different languages (default: `false`, intra-language only).
- `--strict-literals`: Preserve exact literal values instead of type-class bucketing (default: `false`).
- `--limit <N>`: Max pairs to output (default: `100`).
- `--format <md|json>`: Output format (default: `md`).
