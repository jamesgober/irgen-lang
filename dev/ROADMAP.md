# irgen-lang - Roadmap

> Path from scaffold to a stable 1.0. Hard parts are front-loaded; each phase has hard exit criteria.
> Master plan: C:/Dev/libraries/Rust/lang-collection/_lexersketch/ROADMAP.md and NEW-LIBS.md
>
> **Anti-deferral rule:** no listed hard task moves to a later phase unless this file records the move and the reason.

## v0.1.0 - Scaffold (DONE)
Compiles, CI green, structure correct, no domain logic.
- [x] Manifest, README, CHANGELOG, REPS, dual license, CI, deny, clippy, rustfmt, DIRECTIVES, ROADMAP.

## v0.2.0 - Foundation
- [ ] Static-explicit HIR subset to ir-lang 2.0: functions, locals as SSA (with an SSA builder), control flow, calls, records/arrays in memory, OPS ops with policies and check edges, exceptions via invoke/landing pads, statepoint insertion.
- [ ] Differential tests against the bytecode path; wires hir-lang, ir-lang (pre-release pinned), typeck-lang.

## v0.5.0 - Implementation
- [ ] Monomorphization from typeck's plan, closure conversion, match-lang decision trees, dynamic values for mixed profiles, coroutines as state machines.

## v0.9.0 - Hardening
- [ ] Differential fuzzing across all tiers; compile-speed benchmarks.

## v1.0.0 - Stable
- [ ] Frozen after a Kraken subset and Zero compile natively through it (D18).
