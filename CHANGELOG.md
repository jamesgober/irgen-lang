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

[Unreleased]: https://github.com/jamesgober/irgen-lang/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jamesgober/irgen-lang/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/irgen-lang/releases/tag/v0.1.0
