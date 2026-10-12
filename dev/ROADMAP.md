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

## v0.3.0 - First native compile (DONE, 2026-10-12)
- [x] Move to ir-lang `=2.0.0-alpha.3` (ISSUES P36 fixed there) and drop the 0.2 pre-test of `pow` exponents; behaviour identical, verified by the differentials.
- [x] The native end-to-end test moved from 0.2 (ISSUES P37): HIR -> irgen-lang -> opt-lang -> backend-lang -> x86-64 machine code -> run, compared with ir-lang's interpreter and the reference evaluator.
- [x] ISSUES P43 measured on irgen-lang's output and recorded.

Delivered:
- `ir-lang = "=2.0.0-alpha.3"` (dependency and `interp` dev-dependency). A
  signed `pow<overflow=trap>` is now a plain `check` (alpha.3's
  `can_error`); the exponent pre-test (`negative_exponent_guard`) is removed.
  Re-verified: 20,000 programs against the reference evaluator and 10,000
  against the bytecode path (the `--ignored` long runs), 0 disagreements; the
  feature test pins the new IR shape (a `check pow<overflow=trap>`, no
  compare) and the outcomes (error 6, trap 6 in trap mode, trap 1 on
  overflow).
- `tests/native.rs` and its loader `tests/common/native.rs` (x86-64 Linux and
  Windows, in memory): an isa-lang-assembled entry thunk (SysV or Win64
  callee), a trap handler, host functions behind veneers that adapt the
  convention and can unwind, the float `pow` runtime bound to
  `ir_lang::ls_pow` on bit patterns, and a native unwinder over
  backend-lang's landing-pad records and frame layouts.
- Programs run natively: the generator's programs with floats off (GC
  references in every other program) in a 150-program sweep, a 20-program
  sweep of larger programs, and a 3,000-program long run (24,000 native runs:
  15,692 returned, 5,072 threw, 3,236 trapped, 0 disagreements); and
  hand-written programs: recursive `fib` (to 25), loops with `break` and
  `continue`, integer `pow` under `error`/`wrap`/`trap`, float `pow` through
  `__lexersketch_pow_f64`, records passed by address and returned through the
  hidden pointer with array writes and bounds checks, exceptions and run-time
  errors unwinding through two native frames into `catch` with `finally` and
  `defer`, a throwing host function with a live GC reference, many values
  live across an unwinding call (catches an unwinder that does not restore
  callee-saved registers), and OPS division and shift policies. Each in both
  error modes, lowered and optimized, SysV and Win64, with and without a
  frame pointer, against the interpreter (lowered and optimized module) and
  the reference evaluator, host trace included. Mutation checks: an unwinder
  that skips restoring saved registers, a wrong `pow` runtime, and a corrupted
  trace are each caught.
- `examples/native.rs`: `fib(30)` built as HIR, lowered, optimized, compiled
  for the host convention, run natively, and checked against the interpreter.
- Benchmarks `checked_chain_5k`/`checked_chain_20k`; the README's linear-time
  claim narrowed to lowering itself (see P43 below).

Dependency wiring (decided here, recorded per the anti-deferral rule):
- **ir-lang `=2.0.0-alpha.3`: wired, pinned exactly.** Unchanged use of the
  builder, variable construction, statepoints, `can_error`, compaction, the
  validator.
- **opt-lang 0.3, backend-lang 0.4, isa-lang 0.2: dev-dependencies** for the
  native test and example (backend-lang's results are machine code and
  relocations; isa-lang assembles the test's thunk, unwinder entry, and host
  veneers). Not runtime dependencies: this crate's output is the ir-lang
  module, and the native path stays the caller's to assemble (LexerSketch's
  driver). object-lang is not needed: the test loads code in memory rather
  than writing an ELF, which also makes it run on Windows.
- **hir-lang 0.4, typeck-lang 0.3, intern-lang 1, bcgen-lang 0.3, bvm-lang
  `=2.0.0-alpha.3`:** unchanged.

Not run natively, and why (not deferred work of this crate):
- Float arithmetic other than `pow`, float comparisons, int/float
  conversions, and float arguments and results: backend-lang 0.4 refuses them
  until isa-lang has SSE (ISSUES P11). The generator runs natively with floats
  off; float `pow` is reached through `bitcast`.
- Moving collection: the test's host objects never move, so stack maps are
  exercised (spills and reloads around safepoints) but relocation is not;
  backend-lang's own stack-map tests run a moving collector.
- A system unwinder: backend-lang 0.4 emits no unwind tables; the test's
  unwinder reads backend-lang's records directly.

ISSUES P43 on irgen-lang's output (measured, not reshaped):
- Shape: one function holding a chain of `check`s (operations that can raise
  under the default policies) whose error edges all join the function's (or a
  `try` region's) one error block. ir-lang alpha.3's dominator computation
  walks the chain once per joining edge.
- Release, x86-64 Windows, one function of `x = x + k` (checked): `lower`
  2,500 checks 9 ms; 5,000 30 ms; 10,000 106 ms; 20,000 400 ms; 40,000
  1.56 s, of which the final validation 1.50 s (irgen-lang's own work about
  50 ms). On the same 40,000-check module opt-lang takes 1.63 s and
  backend-lang 1.28 s, each dominated by its own input validation. Criterion:
  `checked_chain_5k` 31.6 ms, `checked_chain_20k` 422 ms (13x for 4x).
- Not hit by the 100k/1M benches: `generated_100k` 54.3 ms (0.2.0 on the
  same machine back to back: 56.9 ms), `straight_100k` 14.4 ms (15.6 ms),
  `generated_1m` 543 ms, `straight_1m` 163 ms. Their functions are small or
  their arithmetic wraps.
- Why irgen-lang does not reshape: forwarding blocks per group of checks only
  divide the cost by the group size (the walk still climbs the chain), and
  opt-lang's `SimplifyCfg` threads edges through such empty blocks back into
  the one join before backend-lang validates again; one error block per
  `check` with no join would duplicate the raise sequence (allocator call and
  `resume`, or the trap switch) at every operation. The fix belongs in
  ir-lang's validator (P43, alpha.4); irgen-lang's numbers will be re-measured
  when it moves.

## v0.5.0 - Implementation
- [ ] Monomorphization from typeck's plan, closure conversion, match-lang decision trees, dynamic values for mixed profiles, coroutines as state machines.
- [ ] The forms 0.2 refuses that a Kraken subset and Zero need (each a `Construct` today): methods and associated items (static dispatch through typeck's `method`/`path_def`), sum types (with the decision trees above), references and pointers (address-taken locals in slots), strings and slices (with rt-lang), function values and indirect calls, named arguments and defaults, function statics, atomics and volatile intrinsics (1:1 to ir-lang), inline assembly (through isa-lang's assembler), nullable types. Union records, structural records, dynamic access, and `global` statements stay refused unless a target language needs them.

## v0.9.0 - Hardening
- [ ] Differential fuzzing across all tiers; compile-speed benchmarks.

## v1.0.0 - Stable
- [ ] Frozen after a Kraken subset and Zero compile natively through it (D18).
