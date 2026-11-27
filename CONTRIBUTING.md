# Contributing to kagami

Thank you for your interest in contributing to `kagami`.

## Prerequisites

- **Rust (stable toolchain)** — `kagami` uses Rust edition 2024 (`rustup toolchain install stable`).
- **`just`** — optional task runner for local workflows (`just --list`).

## Development Workflow

All common tasks are exposed via the root [`justfile`](justfile):

```bash
just --list       # Show available recipes
just build        # Build debug binary
just release      # Build optimized release binary
just test         # Run all unit and integration tests
just fmt          # Format Rust sources with rustfmt
just fmt-check    # Verify rustfmt formatting without modifying files
just lint         # Run Clippy across all targets
just check        # Run fmt-check, lint, and test
just self-check   # Run kagami against its own repository
```

If you do not have `just` installed, run the equivalent `cargo` commands directly (`cargo test`, `cargo clippy --all-targets`, `cargo fmt --all -- --check`).

## Architecture & Contracts

Before changing CLI flags, graph resolution, cross-language bridges, or canonical IR lowering, review the design documents under `docs/`:

- [Architecture Spec](docs/SPEC-0001-architecture.md)
- [CLI Contract](docs/CONTRACT-cli.md)
- [Canonical IR Contract](docs/CONTRACT-ir.md)
- [ADR-0001: Polyglot Graph & Indexing](docs/ADR-0001-polyglot-graph-and-indexing.md)
- [ADR-0002: Cross-Language Bridges](docs/ADR-0002-cross-language-bridges.md)
- [ADR-0003: Canonical IR & Similarity](docs/ADR-0003-canonical-ir-and-similarity.md)

## Pull Requests

1. Keep PRs focused on a single concern (e.g., language precision, IR lowering, CLI UX, or CI/docs).
2. Add or update integration tests in `tests/` for any behavior change or bug fix.
3. Update `docs/CONTRACT-cli.md`, `docs/CONTRACT-ir.md`, or `README.md` when flags, config keys, or output schemas change.
4. Run `just test`, `just lint`, and `just self-check` locally before opening your PR.
5. Format new or modified Rust files with `rustfmt` so formatting drift does not grow.

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
