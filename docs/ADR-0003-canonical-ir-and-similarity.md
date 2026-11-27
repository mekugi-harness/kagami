# ADR-0003: Canonical Control-Dataflow IR & Two-Stage Similarity

## Status
Accepted

## Context
Token-based or raw-AST clone detectors fail to recognize semantically equivalent constructs written with different syntax, such as:
- Go `for i := 0; i < n; i++` vs `for i := range n` (or Rust `for i in 0..n`)
- `if / else if / else` chains vs `switch / case` (or Rust `match`)
- Go `if err != nil { return err }` vs Rust `?` error propagation
- Guard clauses vs nested `if / else`

At the same time, naive pairwise comparison over ~40k+ functions in a large repository is $O(N^2)$ and too slow.

## Decision
1. **Lower every callable unit** (function, method, closure-bound const, shell function, justfile recipe) from tree-sitter CST into a small language-agnostic **Canonical IR** (~18 node kinds).
2. **Apply deterministic canonicalization passes** before hashing/matching:
   - Loop normalization (`for i := 0; i < n; i++` and `for i := range n` both lower to `Loop(IterRange(0, n, 1))`).
   - Branch/dispatch normalization (equality `if-else-if` chains, `switch-case`, and `match` all lower to `MultiBranch(scrutinee, sorted_arms, default)`).
   - Early-return / guard-clause normalization and error-propagation desugaring (`PropagateErr`).
   - Alpha-renaming of local variables by first-definition order (`$0`, `$1`, ...), literal type-class bucketing (with `--strict-literals` opt-in), and commutative operand sorting.
3. **Two-stage similarity engine**:
   - **Stage 1 ($O(N)$)**: Exact BLAKE3 hash of canonical IR for instant `1.0` equivalence buckets + Weisfeiler-Lehman subtree/edge shingles with MinHash LSH for near-duplicate candidate retrieval.
   - **Stage 2**: Weighted multiset Dice/Jaccard scoring over WL subtree + CFG edge features on candidate pairs, returning a score in `[0.0, 1.0]` and a concise structural diff reason.
4. **Intra-language by default**, with `--cross-lang` opt-in to compare canonical IR across languages.

## Consequences
- Equivalent loop and branching idioms deterministically score `1.0`.
- Scales linearly in candidate retrieval to repositories with tens of thousands of functions.
- Tiny functions (`< --min-nodes`, default `12`) are filtered out by default to eliminate trivial getter/wrapper noise.
