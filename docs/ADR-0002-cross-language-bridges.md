# ADR-0002: Cross-Language Bridge Nodes

## Status
Accepted

## Context
Intra-language call graphs stop at process and wire boundaries. In polyglot repos like `mekugi`, a Rust JSON-RPC protocol type or handler in `codex-rs/` is invoked over stdio/WS by Go code in `mekugi-tui/` and TypeScript/Svelte code in `mekugi-webui/`, while `justfile` recipes and `.sh` scripts invoke Go commands, Rust binaries, and `bun` scripts.

## Decision
Model cross-language relationships via **Virtual Bridge Nodes** rather than hardcoded pairwise language edges:

1. **`WireContract` bridge**: Extracts wire identifiers from:
   - JSON-RPC method string literals (e.g. `"turn/completed"`, `"thread/start"`, `"item/..."`)
   - JSON Schema files (`schema/json/*.json` titles/definitions)
   - Rust `#[serde(rename = "...")]` and protocol enum/struct names
   - Go `json:"..."` struct tags and RPC method constants
   - TypeScript string literal union discriminants and protocol interfaces
   Both producers and consumers attach to the same `WireContract` node, enabling `blast-radius` to traverse `Rust -> WireContract -> Go/TS/Svelte` and preventing `dead` from flagging cross-language RPC handlers as dead.
2. **`ExecTarget` bridge**: Links `justfile` recipes, `.sh` scripts, `exec.Command` / `std::process::Command`, and `package.json` scripts to target binaries, script paths, and `just` recipe names.
3. **`EnvKey` bridge**: Links exported/read environment variables across `justfile`, `.sh`, Go, Rust, and TS/JS (opt-in for `blast-radius` via `--include-env` so shared env vars do not pollute call graphs).

## Consequences
- Adding a new language automatically participates in cross-language traversal once its frontend emits bridge annotations.
- Bridge edges are tagged `confidence: heuristic` (or `resolved` when backed by an exact script path or schema file) so users can filter or inspect them.
