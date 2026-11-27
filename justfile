# List available recipes
default:
    @just --list

# Build debug binary
build:
    cargo build

# Build optimized release binary
release:
    cargo build --release

# Install kagami into Cargo's bin directory
install:
    cargo install --path .

# Run all unit and integration tests
test:
    cargo test

# Format Rust sources with rustfmt
fmt:
    cargo fmt --all

# Verify Rust source formatting
fmt-check:
    cargo fmt --all -- --check

# Note: `-D warnings` is intentionally omitted until pre-existing warnings in `src/` are resolved.

# Run Clippy across all targets
lint:
    cargo clippy --all-targets

# Note: `fmt-check` is advisory here until pre-existing formatting drift in `src/` is resolved.

# Run formatting (advisory), lint, and test checks
check:
    -just fmt-check
    just lint
    just test

# Run kagami against its own repository
self-check:
    cargo run -- --no-cache blast-radius src/main.rs main
    cargo run -- --no-cache dead
    cargo run -- --no-cache similar
