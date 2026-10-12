# irgen-lang - Roadmap

> Path from scaffold to a stable 1.0. Hard parts are front-loaded; each phase has hard exit criteria.
> Master plan: C:/Dev/libraries/Rust/lang-collection/_lexersketch/ROADMAP.md and NEW-LIBS.md
>
> **Anti-deferral rule:** no listed hard task moves to a later phase unless this file records the move and the reason.

## v0.1.0 - Scaffold (DONE)
Compiles, CI green, structure correct, no domain logic.
- [x] Manifest, README, CHANGELOG, REPS, dual license, CI, deny, clippy, rustfmt, DIRECTIVES, ROADMAP.

## v0.2.0 - Foundation (DONE, 2026-10-12)
- [x] Static-explicit HIR subset to ir-lang 2.0: functions, locals as SSA (with an SSA builder), control flow, calls, records/arrays in memory, OPS ops with policies and check edges, exceptions via invoke/landing pads, statepoint insertion.
- [x] Differential tests against the bytecode path; wires hir-lang, ir-lang (pre-release pinned), typeck-lang.

Delivered:
- `lower`, `Lowerer`, `Options` (error mode; step, global-byte, and
  statepoint budgets), `ErrorMode`, `Lowered`, `Stats`, `LowerError`,
  `Construct`, `Limit`, and the `rt` run-time interface (the
  `__irgen_exception` allocator, the exception record's layout and tags, the
  error codes).
- Functions and signatures (aggregates by address, results through a hidden
  pointer, `noreturn`), module bodies, `abi` imports, host imports, qualified
  unique symbols.
- Locals as SSA through ir-lang's `declare_var`/`def_var`/`use_var`/
  `seal_block`, every block sealed as soon as its predecessors are known;
  dead code lowered into unreachable blocks and removed.
- Control flow: labeled blocks with values, `if`, the loop primitive with its
  step, labeled `break`/`continue`, `return`; `match` over scalars, tuples,
  and records with literal/range/constant/binding/wildcard/tuple/
  constructor/record/or-patterns and guards (runs of literal arms as one
  `switch`; `NoMatch` E0200); `let` patterns and `let ... else`.
- Records, tuples, fixed-length arrays as ir-lang aggregates in stack slots
  and globals; bounds-checked indexing (E0102); compile-time global
  initializers; inlined constants (cycles refused).
- Every OPS v2 operation and cast with exactly ir-lang's required policy
  fields, `check` where `can_error` says so; mixed-type shift amounts
  converted with the same outcome; compound assignment and `let_place` with
  the place evaluated once.
- `throw`, `try`/`catch`/`finally`, `defer` through `invoke`, landing pads,
  and cleanup regions lowered once (each exit routed through a `dest`
  switch); run-time errors raised as exceptions or trapped.
- Host types as GC references; ir-lang's `insert_statepoints` (budgeted) on
  every function holding references.
- Verification: an independent reference evaluator of the subset and a
  program generator (`tests/common`); differential tests against ir-lang's
  interpreter in both error modes (20,000 programs in the long run, 0
  disagreements) and against bcgen-lang + bvm-lang (10,000 programs in the
  long run, 0 disagreements); every integer operation over every policy and
  edge value; hostile depth, budgets, and a linear scale test; benches at
  about 100k and 1M HIR nodes.

Dependency wiring (decided here, recorded per the anti-deferral rule):
- **hir-lang 0.4: wired.** The input.
- **typeck-lang 0.3: wired.** Expression, pattern, and binder types, and
  coercions (numeric widening lowered; any other coercion targets a type
  outside the subset). Its monomorphization plan is not read yet: that is
  0.5.
- **ir-lang `=2.0.0-alpha.2`: wired, pinned exactly** (a pre-release whose
  surface may change before 2.0.0). Uses the builder, variable construction,
  `insert_statepoints_within`, `can_error`, compaction, and the validator.
- **intern-lang 1: wired.** Symbol text through `Lookup`.
- **bcgen-lang 0.3 + bvm-lang `=2.0.0-alpha.3`: dev-dependencies** for the
  bytecode-path differential (`tests/bytecode.rs`). bcgen-lang compiles the
  dynamic profile and refuses declared records until its 0.5, so that
  differential covers programs over `i64`, `bool`, and `f64` (operations
  with every policy, locals, control flow, `match`, calls, globals,
  constants, exceptions, `finally`, `defer`, a host trace); aggregates and GC
  references are covered by the differential against the reference
  evaluator.
- **match-lang: not wired.** Decision trees are 0.5; 0.2 tests patterns
  sequentially (with `switch` runs).

Moved (anti-deferral rule):
- **The native end-to-end test moves to the first release of backend-lang
  (and opt-lang) on ir-lang 2.0.0-alpha.2 or later.** Checked 2026-10-11:
  backend-lang 0.3.0 and opt-lang 0.2.0 on crates.io pin `ir-lang =
  "=2.0.0-alpha.1"`. A Cargo probe with `ir-lang = "=2.0.0-alpha.2"` and
  `backend-lang = "0.3.0"` fails to resolve ("failed to select a version
  for `ir-lang` ... all possible versions conflict"): one build holds one 2.0
  pre-release. object-lang 0.2.0 has no ir-lang dependency; linker-lang 1.0
  works on codegen-lang and ir-lang 1.x. Nothing was faked. Exit criterion:
  a dev-dependency test lowers a HIR function, compiles it with backend-lang
  to machine code, runs it, and compares with ir-lang's interpreter.

Found while building (for the ledger; not a change to another crate):
- ir-lang 2.0.0-alpha.2's `pow<overflow=trap>` traps (code 6) on a negative
  exponent (IR.md §4), while OPS v2 (§3, §6) and HIR v3 (§12) make it the
  `NegativeExponent` error under every policy but `promote`, which
  bvm-lang 2.0.0-alpha.3 raises. irgen-lang tests the exponent first and
  raises the error, so the native path agrees with the bytecode path; the
  bytecode differential found it.

## v0.5.0 - Implementation
- [ ] Monomorphization from typeck's plan, closure conversion, match-lang decision trees, dynamic values for mixed profiles, coroutines as state machines.
- [ ] The forms 0.2 refuses that a Kraken subset and Zero need (each a `Construct` today): methods and associated items (static dispatch through typeck's `method`/`path_def`), sum types (with the decision trees above), references and pointers (address-taken locals in slots), strings and slices (with rt-lang), function values and indirect calls, named arguments and defaults, function statics, atomics and volatile intrinsics (1:1 to ir-lang), inline assembly (through isa-lang's assembler), nullable types. Union records, structural records, dynamic access, and `global` statements stay refused unless a target language needs them.
- [ ] The native end-to-end test (moved from 0.2, see above).

## v0.9.0 - Hardening
- [ ] Differential fuzzing across all tiers; compile-speed benchmarks.

## v1.0.0 - Stable
- [ ] Frozen after a Kraken subset and Zero compile natively through it (D18).
