# CONTRACT: Canonical IR & Equivalence Guarantees

## 1. IR Node Set
Every lowered callable unit produces an `IrFunc` containing a canonical tree/CFG of `IrNode`:
- `Seq(Vec<IrNode>)` — sequential statement block (flattened)
- `Loop { kind: LoopKind, body: Box<IrNode> }` where `LoopKind` is:
  - `IterRange { start, end, step }`
  - `IterSeq { seq }`
  - `While { cond }`
  - `Infinite`
- `MultiBranch { scrutinee: Option<Box<IrNode>>, arms: Vec<(IrNode, IrNode)>, default: Option<Box<IrNode>> }`
- `Branch { cond: Box<IrNode>, then_br: Box<IrNode>, else_br: Option<Box<IrNode>> }`
- `Return(Option<Box<IrNode>>)`
- `Break` / `Continue`
- `PropagateErr(Box<IrNode>)`
- `Assign { dst: VarId, val: Box<IrNode> }`
- `Call { callee: CalleeShape, args: Vec<IrNode> }`
- `BinOp { op: OpClass, lhs: Box<IrNode>, rhs: Box<IrNode> }`
- `UnOp { op: UnOpClass, expr: Box<IrNode> }`
- `Member { obj: Box<IrNode>, field: String }`
- `Index { obj: Box<IrNode>, idx: Box<IrNode> }`
- `Var(VarId)` — alpha-renamed local slot (`$0`, `$1`, ...)
- `Lit(LitClass)` — `Zero | One | Int | Float | Str | Bool | Null` (or exact value under `--strict-literals`)
- `Opaque(String)` — fallback for unsupported syntax

## 2. Guaranteed Equivalences (Similarity == 1.0)
1. **Counting loop equivalence**:
   - `for i := 0; i < n; i++ { body }` (Go/JS/TS)
   - `for i := range n { body }` (Go)
   - `for i in 0..n { body }` (Rust)
   All normalize to `Loop { kind: IterRange(Zero, $n, One), body }`.
2. **Branch / dispatch equivalence**:
   - `if x == A { b1 } else if x == B { b2 } else { b3 }`
   - `switch x { case A: b1; case B: b2; default: b3 }`
   - `match x { A => b1, B => b2, _ => b3 }`
   All normalize to `MultiBranch { scrutinee: Some($x), arms: sorted([(A, b1), (B, b2)]), default: Some(b3) }`.
3. **Alpha-renaming invariance**: Renaming local variables or parameters preserves identical canonical IR.
4. **Commutative operator invariance**: `a == b` and `b == a`, `a + b` and `b + a` normalize identically.
5. **Guard-clause / early-return equivalence**: `if !c { return e }; rest` and `if c { rest } else { return e }` normalize to identical branching structure.
6. **Error-propagation equivalence**: Go `if err != nil { return err }` and Rust `expr?` both normalize to `PropagateErr`.
