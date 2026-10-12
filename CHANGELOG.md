<h1 align="center">
    <img width="90px" height="auto" src="https://raw.githubusercontent.com/jamesgober/jamesgober/main/media/icons/hexagon-3.svg" alt="Triple Hexagon">
    <br><b>CHANGELOG</b>
</h1>
<p>
  All notable changes to <code>irgen-lang</code> will be documented in this file. The format is based on <a href="https://keepachangelog.com/en/1.1.0/">Keep a Changelog</a>,
  and this project adheres to <a href="https://semver.org/spec/v2.0.0.html/">Semantic Versioning</a>.
</p>

---

## [Unreleased]

### Added

### Changed

### Fixed

### Security

---

## [0.3.0] - 2026-10-12

The first native compile (ISSUES P37): HIR lowered by irgen-lang, optimized by
opt-lang 0.3, compiled by backend-lang 0.4 to x86-64 machine code, and run on
the processor, every stage the family's own. The public API is unchanged; the
ir-lang pin moves to `=2.0.0-alpha.3`, whose `Module` `Lowered` returns, so
this is a minor release.

### Added

- **The native end-to-end test** (`tests/native.rs`): generated programs (the
  generator with floats off) and hand-written ones (recursive `fib`, loops
  with `break`/`continue`, integer `pow` under every overflow policy, float
  `pow` through the runtime, records and arrays with bounds checks,
  exceptions unwinding across native frames with `finally` and `defer`, a
  host function that throws with a GC reference live, values live across an
  unwinding call, OPS division and shift policies) are lowered, optimized,
  compiled under the SysV and Win64 conventions with and without a frame
  pointer, and run in this process; each outcome (result, thrown value or
  run-time error, trap code, host trace) is compared with ir-lang's
  interpreter on the lowered and the optimized module and with the
  independent HIR evaluator, in both error modes. A long run behind
  `--ignored`: 3,000 programs, 24,000 native runs, no disagreement.
- The test's loader (`tests/common/native.rs`, test support): executable
  memory through `mmap`/`VirtualAlloc`, relocations, an entry thunk, a trap
  handler, host functions behind convention-adapting veneers that can unwind,
  the float `pow` runtime bound to `ir_lang::ls_pow`, and a native unwinder
  over backend-lang's landing-pad records and frame layouts (backend-lang 0.4
  emits no unwind tables).
- `examples/native.rs`: the pipeline end to end, `fib(30)` run natively and
  against the interpreter.
- Benchmarks `checked_chain_5k` and `checked_chain_20k`: one function of
  checked additions, the shape ir-lang alpha.3's validator handles in
  quadratic time (ISSUES P43).
- Dev-dependencies `opt-lang` 0.3, `backend-lang` 0.4, `isa-lang` 0.2.

### Changed

- **`ir-lang` `=2.0.0-alpha.2` to `=2.0.0-alpha.3`** (both the dependency and
  the `interp` dev-dependency).
- A signed integer `pow` under `overflow = trap` is now a plain `check` (ir-lang
  alpha.3 makes it fallible, ISSUES P36): the 0.2 pre-test of the exponent
  (a compare, a branch, and a raise before the operation) is gone. Behaviour is
  unchanged: a negative exponent is `NegativeExponent` (code 6) as an error or,
  under `ErrorMode::Trap`, a trap with 6. Verified by the long differential
  runs (20,000 programs against the reference evaluator, 10,000 against the
  bytecode path, no disagreement) and the native test.
- `Unsupported`'s message and the `Construct` docs no longer name a release
  ("outside the subset irgen-lang lowers").

### Fixed

- The README's "linear time" guarantee is stated precisely: lowering is
  linear, but the validation `lower` ends with is quadratic on a long chain of
  `check`s in one function (ir-lang's validator, ISSUES P43; the same in 0.2).
  The numbers are in the README's Performance section.

---

## [0.2.0] - 2026-10-12

The foundation: the static-explicit subset of HIR (hir-lang 0.4, typed by
typeck-lang 0.3) lowers to ir-lang 2.0.0-alpha.2 SSA, every function passing
ir-lang's validator. This is the first crate that feeds the family's own
native path (ISSUES P29). Its output runs in ir-lang's reference interpreter;
a native compile waits for backend-lang to move from ir-lang alpha.1 to
alpha.2 (recorded in the roadmap).

### Added

- `lower` (Tier 1), `Lowerer`, `Options` (`with_errors`, `with_max_steps`,
  `with_max_global_bytes`, `with_statepoint_budget`), `ErrorMode` (`Raise`,
  `Trap`), `DEFAULT_MAX_GLOBAL_BYTES`, `Lowered` (`module`, `into_module`,
  `function`, `module_body`, `global`, `functions`, `stats`), `Stats`,
  `LowerError`, `Construct`, `Limit`, and the `rt` module (the exception
  allocator `__irgen_exception`, the exception record's layout, value tags,
  `TAG_ERROR`, `E_INDEX_OUT_OF_BOUNDS`, `E_NO_MATCH`).
- Functions and signatures: scalars by value, aggregates by address,
  aggregate results through a hidden first pointer, `noreturn` for `!`;
  module bodies; `abi` functions without a body as imports with their
  calling convention; host (`extern`) functions as imports; qualified,
  unique symbol names.
- Locals as SSA through ir-lang's variable construction, every block sealed
  as soon as its predecessors are known; zero-initialized `let`s.
- Control flow: blocks with labels and values, `if`, the loop primitive with
  its step, labeled `break`/`continue`, `return`; dead code lowered into
  unreachable blocks and removed.
- `match` over scalars, tuples, and records: literal, range, constant,
  binding, wildcard, tuple, constructor, record, and or-patterns, with
  guards; runs of literal arms as one `switch`; `NoMatch` (E0200) when no arm
  matches. `let` with patterns, `let ... else`.
- Records, tuples, and fixed-length arrays as ir-lang aggregates in stack
  slots and globals; field access, bounds-checked indexing
  (`IndexOutOfBounds`, E0102), record literals with a base, `[x; N]`.
- Every OPS v2 operation and cast with exactly the policy fields ir-lang
  requires, as a `check` where `can_error` says the operation can fail;
  shift amounts of another integer type converted with the same outcome;
  `NegativeExponent` raised under `overflow = trap` as OPS v2 specifies.
- Compound assignment and `let_place` with the place evaluated once;
  assignments evaluate to the value written.
- Exceptions: `throw`, `try`/`catch` (wildcard and primitive type tests, with
  guards)/`finally`, and `defer`, through `invoke`, landing pads, and cleanup
  regions lowered once; run-time errors raised as exceptions or trapped
  (`ErrorMode`).
- Host types as GC references, with statepoints inserted by ir-lang's
  `insert_statepoints` (budgeted).
- Global initializers evaluated at compile time into bytes; constants
  inlined at each use; constant cycles refused.
- Precise `Unsupported` errors naming the construct and the node for
  everything outside the subset (dynamic values, closures, generics,
  coroutines, sum types: 0.5; the rest listed in the roadmap).
- Tests: an independent reference evaluator of the subset and a generator of
  well-typed programs (`tests/common`); differential tests against ir-lang's
  interpreter in both error modes (`tests/differential.rs`, 20,000 programs
  in the long run) and against bcgen-lang 0.3 + bvm-lang 2.0.0-alpha.3
  (`tests/bytecode.rs`); per-construct tests (`tests/features.rs`,
  `tests/lower.rs`), refusals (`tests/unsupported.rs`), and hostile input,
  budgets, and a linear scale test (`tests/hostile.rs`).
- Benchmarks at about 100k and 1M HIR nodes; `examples/lower.rs`.

### Changed

- Dependencies wired: `hir-lang` 0.4, `typeck-lang` 0.3, `ir-lang`
  `=2.0.0-alpha.2`, `intern-lang` 1 (all `default-features = false`; the
  `std` feature forwards). Dev-dependencies: `ir-lang` with `interp`,
  `bcgen-lang` 0.3, `bvm-lang` `=2.0.0-alpha.3`.

---

## [0.1.0] - 2026-10-08

Initial scaffold and repository bootstrap. No domain logic yet &mdash; this release establishes the structure, tooling, and quality gates the implementation will be built on.

### Added

- `Cargo.toml` with crate metadata, Rust 2024 edition, MSRV 1.85.
- Dual `Apache-2.0 OR MIT` license files.
- `README.md`, `CHANGELOG.md`, and a documentation skeleton.
- `REPS.md` compliance baseline.
- `.github/workflows/ci.yml` CI matrix; `deny.toml`, `clippy.toml`, `rustfmt.toml`.
- `dev/DIRECTIVES.md` and `dev/ROADMAP.md` (committed engineering standards + plan).

[Unreleased]: https://github.com/jamesgober/irgen-lang/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/jamesgober/irgen-lang/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jamesgober/irgen-lang/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/irgen-lang/releases/tag/v0.1.0
