<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br>
    <b>irgen-lang</b>
    <br>
    <sub><sup>HIR TO SSA LOWERING</sup></sub>
</h1>

<div align="center">
    <a href="https://crates.io/crates/irgen-lang"><img alt="Crates.io" src="https://img.shields.io/crates/v/irgen-lang"></a>
    <a href="https://crates.io/crates/irgen-lang"><img alt="Downloads" src="https://img.shields.io/crates/d/irgen-lang?color=%230099ff"></a>
    <a href="https://docs.rs/irgen-lang"><img alt="docs.rs" src="https://img.shields.io/docsrs/irgen-lang"></a>
    <a href="https://github.com/jamesgober/irgen-lang/actions"><img alt="CI" src="https://github.com/jamesgober/irgen-lang/actions/workflows/ci.yml/badge.svg"></a>
    <a href="https://github.com/rust-lang/rfcs/blob/master/text/2495-min-rust-version.md"><img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.85%2B-blue"></a>
</div>

<br>

<div align="left">
    <p>
        <strong>irgen-lang</strong> lowers HIR, the core every LexerSketch language translates into, to <a href="https://crates.io/crates/ir-lang">ir-lang</a> 2.0 SSA: the input of the family's own native compiler path. Type-check a unit with typeck-lang, call <code>lower</code>, and get an ir-lang module whose every function has passed ir-lang's validator: locals in SSA form, records and arrays in stack slots and globals, OPS operations with their exact policies, run-time errors on explicit <code>check</code> edges, exceptions through <code>invoke</code> and landing pads, and GC references relocated at every safepoint.
    </p>
    <p>
        The path from there is opt-lang, then backend-lang's instruction selection and register allocation, then machine code and object files, all of it the family's own: no Cranelift, no LLVM. Today irgen-lang's output runs in ir-lang's reference interpreter; <strong>it is not yet connected to a native backend</strong>, because backend-lang 0.3 and opt-lang 0.2 are built on ir-lang 2.0.0-alpha.1 and this crate on alpha.2, and Cargo cannot hold both pre-releases in one build (see the <a href="./dev/ROADMAP.md">roadmap</a>).
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.85+</strong> (Rust 2024 edition). <code>no_std</code>-compatible (needs only <code>alloc</code>), <code>#![forbid(unsafe_code)]</code>, and only family dependencies: <a href="https://crates.io/crates/hir-lang"><code>hir-lang</code></a> 0.4, <a href="https://crates.io/crates/typeck-lang"><code>typeck-lang</code></a> 0.3, <a href="https://crates.io/crates/ir-lang"><code>ir-lang</code></a> <code>=2.0.0-alpha.2</code>, and <a href="https://crates.io/crates/intern-lang"><code>intern-lang</code></a> 1.
    </p>
    <blockquote>
        <strong>Status: 0.2.0, the foundation.</strong> The static-explicit subset of HIR lowers completely; generics, closures, sum types, coroutines, and dynamic values are precise errors until 0.5. The API is pre-1.0 and may change before <code>1.0.0</code>. See <a href="./CHANGELOG.md"><code>CHANGELOG.md</code></a> and <a href="./dev/ROADMAP.md"><code>dev/ROADMAP.md</code></a>.
    </blockquote>
</div>

<hr>
<br>

## The model

- **[`lower`](./docs/API.md#lower)** takes a validated `hir_lang::Hir`, typeck-lang's `Typed` for it (under a static profile), and the interner its names live in, and returns a **[`Lowered`](./docs/API.md#lowered)**: the `ir_lang::Module` and where each function, module body, and global item went.
- **[`Lowerer`](./docs/API.md#lowerer)** with **[`Options`](./docs/API.md#options)** chooses what run-time errors do (**[`ErrorMode`](./docs/API.md#errormode)**: raise as an exception, or trap) and sets the budgets.
- **[`LowerError`](./docs/API.md#lowererror)** says why a unit was not lowered; anything outside the subset is `Unsupported` with the **[`Construct`](./docs/API.md#construct)** it uses and the first node that uses it.
- **[`rt`](./docs/API.md#rt)** documents the one host function lowered code calls (the exception-record allocator) and the record's layout and tags.

<br>

What it guarantees, and how each guarantee is checked:

| Guarantee | How it is held |
|---|---|
| Every function of every module passes ir-lang's validator, statepoint and exception-edge rules included. | `lower` validates the module before returning it; every test lowers through it, and the IR text of generated programs is parsed back by ir-lang and validated again. |
| The lowered code means what the HIR means (OPS semantics, policies, evaluation order, exceptions, `finally`/`defer`). | Differential tests run generated programs (20,000 per long run) through irgen-lang and ir-lang's interpreter and through an independent evaluator of the HIR subset written in the tests, in both error modes, comparing the result, the thrown value or error, the trap, and a host-visible trace; every integer operation is checked over every policy and edge value. |
| It agrees with the bytecode path. | The same kind of programs (restricted to `i64`, `bool`, and `f64`, where the dynamic profile means the same) run through bcgen-lang and bvm-lang; results, errors, traps, and traces must match (10,000 per long run). |
| Every GC reference live across a safepoint is relocated there. | ir-lang's `insert_statepoints` fills the GC lists; the validator checks rule 1, and the interpreter fails any use of a stale reference, over generated programs that keep host references across calls, loops, and landing pads. |
| No recursion over the input; budgets on everything input can drive. | 100,000-deep nests lower on a 1 MiB stack; step, global-byte, frame-size, and statepoint budgets are errors, and random budgets on random programs never panic. |
| Linear time, deterministic output. | A 4x larger body takes about 4x the time (scale test at 100k and 400k statements); lowering the same unit twice prints the same module. |

<hr>
<br>

## Installation

```toml
[dependencies]
irgen-lang = "0.2"
```

Without the standard library:

```toml
[dependencies]
irgen-lang = { version = "0.2", default-features = false }
```

<hr>
<br>

## Quick start

`fn add(a: i64, b: i64) -> i64 { a + b }`, lowered and run in ir-lang's reference interpreter:

```rust
use hir_lang::{Builder, FnDef, Item, ItemKind, Name, OpKind, Param, Prim, Ty};
use intern_lang::Interner;
use ir_lang::interp::{self, Outcome, Val};
use typeck_lang::{check, Profile};

let mut names = Interner::new();
let mut b = Builder::new();
let mut params = Vec::new();
let mut binders = Vec::new();
for n in ["a", "b"] {
    let x = b.new_binder(Name::new(names.intern(n)), hir_lang::BinderKind::Param);
    let pat = b.bind(x);
    let ty = b.ty(Ty::Prim(Prim::I64));
    params.push(b.param(Param { ty: Some(ty), ..Param::new(pat) }));
    binders.push(x);
}
let (a, c) = (b.use_binder(binders[0]), b.use_binder(binders[1]));
let sum = b.op(OpKind::Add, &[a, c]); // overflow = error, OPS's default
let body = b.block(&[], Some(sum));
let ret = b.ty(Ty::Prim(Prim::I64));
let params = b.list(&params);
let add = b.item(Item::new(
    Some(Name::new(names.intern("add"))),
    ItemKind::Fn(FnDef { params, ret: Some(ret), body: Some(body), ..FnDef::default() }),
));
let root = b.module(None, &[add]);
let hir = b.finish(root)?;
let typed = check(&hir, &Profile::static_explicit()).unwrap();

let out = irgen_lang::lower(&hir, &typed, &names)?;
let f = out.function(add).unwrap();
// The addition can overflow, so it is a `check` with an error edge.
assert!(out.module().display_function(f).to_string().contains("check add<overflow=error>"));
assert_eq!(
    interp::run(out.module(), f, &[Val::i64(2), Val::i64(3)])?,
    Outcome::Return(vec![Val::i64(5)]),
);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Outside the subset, the error names the construct:

```rust
use hir_lang::{Builder, Name};
use intern_lang::Interner;
use irgen_lang::{Construct, LowerError};
use typeck_lang::{check, Profile};

let mut names = Interner::new();
let mut b = Builder::new();
// fn greet() { let s = "hello"; }
let s = b.str_lit("hello");
let x = b.new_binder(Name::new(names.intern("s")), hir_lang::BinderKind::Local);
let pat = b.bind(x);
let stmt = b.let_stmt(pat, Some(s));
let body = b.block(&[stmt], None);
let f = b.func(Name::new(names.intern("greet")), &[], body);
let root = b.module(None, &[f]);
let hir = b.finish(root)?;
let typed = check(&hir, &Profile::static_explicit()).unwrap();
match irgen_lang::lower(&hir, &typed, &names) {
    Err(LowerError::Unsupported { construct, .. }) => assert_eq!(construct, Construct::String),
    other => panic!("{other:?}"),
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

<hr>
<br>

## What 0.2 lowers

| HIR | ir-lang |
|---|---|
| Non-generic functions, module bodies, `abi` functions without a body | Functions (`export` for public items); imports with their calling convention |
| Parameters and locals of scalar type (`bool`, integers, floats, `char`, host types) | SSA values through ir-lang's variable construction (`declare_var`/`def_var`/`use_var`/`seal_block`) |
| Tuples, records, fixed-length arrays | ir-lang aggregates in stack slots and globals; passed by address, returned through a hidden first pointer |
| Blocks with labels and values, `if`, the loop primitive with its step, `break`/`continue` with labels, `return` | Blocks and edges; every block sealed as soon as its predecessors are known |
| `match` (literal, range, constant, binding, wildcard, tuple, record, and or-patterns; guards) | A sequential decision procedure; a run of literal arms is one `switch`; no match raises `NoMatch` (E0200) |
| Calls of function items, tuple-record constructors, host (`extern`) functions | `call`, or `invoke` inside a `try`, `finally`, or `defer` region |
| Every OPS v2 operation and cast | One instruction carrying exactly the policy fields ir-lang requires; a `check` where `can_error` says the operation can fail |
| Array indexing | A bounds check raising `IndexOutOfBounds` (E0102), then `elem_addr` |
| `let_place`, compound assignment | The place's operands evaluated once |
| `throw`, `try`/`catch`/`finally`, `defer` | Exception records, landing pads, and cleanup regions that each exit (fallthrough, `break`, `continue`, `return`, rethrow) routes through once |
| Values of host types | `ref` values; statepoints inserted by ir-lang's `insert_statepoints` |

Everything else is a [`LowerError::Unsupported`](./docs/API.md#lowererror): dynamic values, closures, generics, coroutines, and sum types are the roadmap's 0.5; strings, methods, references, and the rest are listed in [`Construct`](./docs/API.md#construct) and the roadmap.

<hr>
<br>

## Performance

Lowering is one pass over each body with an explicit task stack, then ir-lang's sealing, statepoint insertion (only where GC references exist), compaction, and one validation of the whole module. Measured with [`benches/bench.rs`](./benches/bench.rs), x86_64 Windows, Rust stable, release profile, typeck-lang's check excluded:

| Benchmark | HIR nodes | Time | Throughput |
|---|---:|---:|---:|
| `generated_100k`: generated programs (many small functions with loops, matches, aggregates, exceptions, host calls, GC references) | 99,174 | ~49 ms | ~2.0 M nodes/s |
| `generated_1m` | 992,262 | ~514 ms | ~1.9 M nodes/s |
| `straight_100k`: one function of `x = x + k` statements | 133,336 | ~13 ms | ~9.9 M nodes/s |
| `straight_1m` | 1,333,336 | ~146 ms | ~9.2 M nodes/s |

On generated programs, about a third of the time is ir-lang's validator and statepoint insertion. Numbers vary by CPU; use the trend across runs.

```bash
cargo bench --bench bench
```

<hr>
<br>

## Testing

```bash
cargo test                       # unit, integration, property, and doc tests
cargo test --no-default-features # no_std + alloc
cargo test --release --test differential --test bytecode -- --ignored   # the long differential runs
cargo bench --bench bench
```

[`tests/common/reference.rs`](./tests/common/reference.rs) is an independent evaluator of the HIR subset (OPS in `i128` arithmetic and native IEEE floats); [`tests/common/progs.rs`](./tests/common/progs.rs) generates well-typed programs of the whole subset. [`tests/differential.rs`](./tests/differential.rs) compares the two; [`tests/bytecode.rs`](./tests/bytecode.rs) compares against bcgen-lang + bvm-lang; [`tests/features.rs`](./tests/features.rs) pins each construct; [`tests/unsupported.rs`](./tests/unsupported.rs) checks the refusals; [`tests/hostile.rs`](./tests/hostile.rs) covers depth, budgets, and scale. Every `rust` example here and in [`docs/API.md`](./docs/API.md) runs as a doctest.

<hr>
<br>

## Cross-platform support

- Linux (x86_64, aarch64)
- macOS (x86_64, Apple Silicon)
- Windows (x86_64)

The crate uses no operating-system facilities; the same unit lowers to the same module on every platform.

<hr>
<br>

## Contributing

See [`dev/DIRECTIVES.md`](./dev/DIRECTIVES.md) for engineering standards and the definition of done. Before a PR: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` must be clean.

<br>

<div id="license">
    <h2>License</h2>
    <p>Licensed under either of</p>
    <ul>
        <li><b>Apache License, Version 2.0</b> &mdash; <a href="./LICENSE-APACHE">LICENSE-APACHE</a></li>
        <li><b>MIT License</b> &mdash; <a href="./LICENSE-MIT">LICENSE-MIT</a></li>
    </ul>
    <p>at your option.</p>
</div>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>James Gober.</strong></sup>
</div>
