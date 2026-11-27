# kagami

Polyglot static analysis CLI for **cross-language blast-radius**, **dead-code reachability**, and **canonical-IR structural similarity**.

Supports Go, Rust, JavaScript/TypeScript, Svelte, `justfile`, and Bash/POSIX shell scripts.

## Commands

- `kagami blast-radius <file_path> <symbol>` — Traces direct and transitive references and relationships across languages.
- `kagami dead` — Finds unreferenced symbols and unreachable call clusters via root-seeded reachability.
- `kagami similar [--min-similarity 0.85]` — Finds syntactically distinct but structurally equivalent/similar functions via canonical IR lowering.

## Documentation

- [Architecture Spec](docs/SPEC-0001-architecture.md)
- [CLI Contract](docs/CONTRACT-cli.md)
- [Canonical IR Contract](docs/CONTRACT-ir.md)
- [ADR-0001: Polyglot Graph & Indexing](docs/ADR-0001-polyglot-graph-and-indexing.md)
- [ADR-0002: Cross-Language Bridges](docs/ADR-0002-cross-language-bridges.md)
- [ADR-0003: Canonical IR & Similarity](docs/ADR-0003-canonical-ir-and-similarity.md)
