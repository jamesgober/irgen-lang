# irgen-lang &mdash; API Reference

> Complete reference for every public item in `irgen-lang` 0.2.0, with examples.
> **Status: pre-1.0 (0.2.0, the foundation).** The surface may change before
> `1.0.0`, which comes after a Kraken subset and Zero compile natively through
> it (LexerSketch decision D18). See [`../dev/ROADMAP.md`](../dev/ROADMAP.md).
> The normative definitions are the LexerSketch specs `specs/HIR.md` (the
> input), `specs/IR.md` (the output), and `specs/OPS.md` (operation
> semantics).

<sub>Copyright &copy; 2026 <strong>James Gober</strong>.</sub>

## Table of contents

- [Overview](#overview)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Concepts](#concepts)
  - [The pipeline](#the-pipeline)
  - [The subset](#the-subset)
  - [How types are represented](#how-types-are-represented)
  - [Functions, signatures, and symbols](#functions-signatures-and-symbols)
  - [Locals and SSA](#locals-and-ssa)
  - [Operations and policies](#operations-and-policies)
  - [Run-time errors](#run-time-errors)
  - [Exceptions, `finally`, and `defer`](#exceptions-finally-and-defer)
  - [GC references and statepoints](#gc-references-and-statepoints)
  - [Globals and constants](#globals-and-constants)
  - [Budgets and determinism](#budgets-and-determinism)
- [`lower`](#lower)
- [`Lowerer`](#lowerer)
- [`Options`](#options)
- [`ErrorMode`](#errormode)
- [`DEFAULT_MAX_GLOBAL_BYTES`](#default_max_global_bytes)
- [`Lowered`](#lowered)
- [`Stats`](#stats)
- [`LowerError`](#lowererror)
- [`Construct`](#construct)
- [`Limit`](#limit)
- [`rt`](#rt)
- [Feature flags](#feature-flags)
- [Limits](#limits)
- [Stability](#stability)

## Overview

| Item | Kind | Purpose |
|---|---|---|
| [`lower`](#lower) | function | Tier-1: lower a type-checked unit with the default options. |
| [`Lowerer`](#lowerer) | struct | A lowering with [`Options`](#options). |
| [`Options`](#options), [`ErrorMode`](#errormode) | struct, enum | The error mode and the budgets. |
| [`Lowered`](#lowered), [`Stats`](#stats) | structs | The ir-lang module, where each item went, and what lowering did. |
| [`LowerError`](#lowererror), [`Construct`](#construct), [`Limit`](#limit) | enums | Why a unit was not lowered. |
| [`rt`](#rt) | module | The run-time interface: the exception allocator, record layout, tags, error codes. |

## Installation

```toml
[dependencies]
irgen-lang = "0.2"
hir-lang = "0.4"        # the input
typeck-lang = "0.3"     # its types
intern-lang = "1"       # names
ir-lang = "=2.0.0-alpha.2"   # the output (add features = ["interp"] to run it)
```

## Quick start

```rust
use hir_lang::{Builder, Name};
use intern_lang::Interner;
use typeck_lang::{check, Profile};

// fn main() {}
let mut names = Interner::new();
let mut b = Builder::new();
let body = b.block(&[], None);
let main = b.func(Name::new(names.intern("main")), &[], body);
let root = b.module(None, &[main]);
let hir = b.finish(root)?;
let typed = check(&hir, &Profile::static_explicit()).unwrap();

let out = irgen_lang::lower(&hir, &typed, &names)?;
let f = out.function(main).unwrap();
assert_eq!(out.module().func_decl(f).unwrap().name(), "main");
out.module().validate()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Concepts

### The pipeline

```text
lower-lang -> resolve-lang -> typeck-lang -> irgen-lang -> ir-lang 2.0 -> opt-lang -> backend-lang -> object/linker
                                                 ^ this crate
```

The bytecode path (bcgen-lang to bvm-lang) starts from the same HIR; the
differential tests run both. Not connected yet: opt-lang 0.2 and backend-lang
0.3 are built on ir-lang `=2.0.0-alpha.1`, this crate on `=2.0.0-alpha.2`, and
Cargo resolves one 2.0 pre-release per build, so a native compile of
irgen-lang's output waits for those crates to move to alpha.2 (recorded in the
roadmap).

### The subset

0.2 lowers HIR checked under typeck-lang's **static disciplines**
(`StaticExplicit`, `StaticInferred`):

- non-generic functions (top-level, nested in modules or in blocks), module
  bodies, and functions with an `abi` and no body (imports);
- parameters of the `Normal` and `PositionalOnly` kinds without defaults,
  passed positionally;
- scalars (`bool`, all integers, `f32`, `f64`, `char`), host (`extern`) types,
  tuples, non-generic records (named, tuple, and unit shapes), and arrays with
  a constant length;
- every expression form of HIR v3 except those listed under
  [`Construct`](#construct): literals, paths, tuples, arrays, repeats, record
  literals with a base, direct calls, fields, indexing, operations, casts,
  assignments (compound too), `let_place`, blocks with labels, `if`, `match`,
  the loop primitive, `break`, `continue`, `return`, `throw`, `try`;
- statements `let` (with `else`, or without an initializer), expression
  statements, nested items, `defer`;
- patterns: wildcard, binding (by value), literal, range, constant path,
  unit record, tuple, tuple-record constructor, record, or-pattern, and in
  `catch` arms a type test against a primitive type.

Implicit conversions: numeric widening (`Coercion::Widen`); any other
coercion targets a type outside the subset.

### How types are represented

| HIR type | ir-lang |
|---|---|
| `bool`, `i8`..`i64`, `u8`..`u64`, `f32`, `f64` | the same value type |
| `isize`, `usize` | `i64`, `u64` (modules are 64-bit) |
| `char` | `u32` |
| a host type (`Type::Extern`) | `ref`: a GC reference |
| `()`, `!` | no value (an empty struct where one is stored) |
| tuple, record | `struct { .. }` aggregate, fields in order (C layout) |
| `[T; N]` | `[T x N]` aggregate |

Aggregates live in memory: a local is a stack slot, an expression's value a
temporary slot, and reading a place of aggregate type copies it, so a later
write cannot change a value already read. A tuple, record, or array that
holds a `ref` is refused ([`GcRefInMemory`](#construct)): a stack slot is not
a GC root (IR.md §9).

### Functions, signatures, and symbols

A function's ir-lang signature has one parameter per HIR parameter of scalar
type, a `ptr` per aggregate parameter (the caller passes a fresh copy, which
the callee owns), none for unit; a scalar result is the return value, an
aggregate result is written through a hidden first `ptr` parameter, and unit
or never returns nothing. A function returning `!` is declared `noreturn`.

Public items are `export`, others `local`. A function with an `abi` (`C`,
`sysv64`, `win64`, `aapcs64`) and no body is an `import` with that
convention; host (`extern`) functions are imports with the `fast`
convention. Symbol names are the item's name qualified by its enclosing named
items (`outer::inner`); a repeated name gets `#<item index>`. A module body is
the function `<module>::__module_body` (`__module_body` for an unnamed root).

### Locals and SSA

Scalar locals and parameters are ir-lang variables (`declare_var`,
`def_var`, `use_var`), turned into SSA by ir-lang's construction (Braun et
al.); every block is sealed as soon as its predecessors are known. A local
declared without an initializer starts at zero (aggregates zero-filled), the
same on every tier. Dead code (after a `return`, say) is lowered into blocks
with no predecessors and removed before the function is finished.

### Operations and policies

Each HIR operation becomes one ir-lang instruction carrying **exactly** the
policy fields ir-lang requires for the operand type (a float operation's
`overflow` field is dropped: IEEE operations consult none). An operation that
can raise an error (ir-lang's `can_error`, which depends on the operand type)
is the operation of a `check` terminator. Two adjustments keep the native tier
equal to OPS:

- **Shift amounts of another type.** HIR shifts by an amount of any integer
  type; ir-lang's operands share one type. The amount is converted keeping
  its low bits (`mask`), clamped to the width or to `-1` so the policy sees
  the same case (`error`, `saturate`), and a negative amount under
  `saturate` on an unsigned operand raises `ShiftOutOfRange` explicitly.
- **Negative exponents under `trap`.** OPS v2 makes integer `pow` with a
  negative exponent `NegativeExponent` (E0006) under every overflow policy
  but `promote`; ir-lang's `pow<overflow=trap>` traps instead (IR.md §4).
  irgen-lang tests the exponent first and raises the error.

`promote` is refused (it needs dynamic values).

### Run-time errors

OPS errors (E0001–E0006), `IndexOutOfBounds` (E0102), and `NoMatch` (E0200)
follow [`ErrorMode`](#errormode): **raise** (default) makes an exception
record with [`rt::TAG_ERROR`](#rt) and the code, which a wildcard `catch` arm
catches and `finally`/`defer` bodies see; **trap** ends the program with
`trap <code>`. A `trap` *policy* traps in both modes.

### Exceptions, `finally`, and `defer`

`throw v` allocates an exception record through the host
([`rt::EXCEPTION`](#rt)) holding the value's [tag](#rt) and bits. Inside a
`try` with `catch` arms, or a `finally` or `defer` region, calls are
`invoke`s whose landing pad forwards the payload to the region; a `throw` or
an error edge jumps there directly. Outside every region, `resume` unwinds to
the caller.

A `catch` arm is `_` (catches everything, run-time errors included) or a
type test against a primitive type, optionally binding the value, with an
optional guard; when no arm matches, the exception continues outward.

`finally` and `defer` bodies are lowered **once**. A jump that leaves one
records which exit it takes (fall through, `break`, `continue`, `return`,
rethrow) and enters the body; after it, a `switch` continues that exit,
possibly into the next enclosing region. A jump out of the body overrides the
pending one; a trap skips it.

### GC references and statepoints

Values of host types are `ref`s, kept in SSA values only. After a function is
built, ir-lang's `insert_statepoints` (within
[`Options::with_statepoint_budget`](#options)) lists every reference live
across each safepoint (a call or `invoke` that is not `nogc`) and rewrites
later uses to the relocated values, along normal and unwind edges alike.
Functions without references skip it. `__irgen_exception` is `nogc` and
`nounwind`: allocating an exception record is not a safepoint.

### Globals and constants

A global item becomes an ir-lang global with its initializer evaluated at
compile time into bytes in ir-lang's layout: literals, tuples, arrays,
repeats, records, blocks holding only a tail, and constants of those (others
are [`NonConstant`](#construct)). A unit global needs no storage. A constant
item is inlined at each use, as Rust does; a constant whose value refers to
itself is [`ConstCycle`](#lowererror). Constant patterns are evaluated at
compile time.

### Budgets and determinism

Lowering is an explicit task stack over the HIR; type mapping, pattern tests,
and constant evaluation are iterative too. Steps (HIR nodes lowered, each
inlined constant once per use), the total bytes of global initializers, stack
slot sizes, and statepoint work are capped ([`Limit`](#limit)). Every table is
ordered, so the same unit lowers to the same module.

## `lower`

```rust,ignore
pub fn lower<L: Lookup>(hir: &Hir, typed: &Typed, names: &L) -> Result<Lowered, LowerError>
```

Lowers a type-checked unit with the default [`Options`](#options).

- `hir`: the unit (validated; paths resolved).
- `typed`: typeck-lang's result for `hir`, under a static profile.
- `names`: the interner `hir`'s names live in; symbols are read from it.

**Errors:** [`LowerError`](#lowererror): type errors in `typed`; a construct
outside the subset (the first in canonical order); a budget; a constant cycle.

```rust
use hir_lang::{Builder, Name};
use intern_lang::Interner;
use typeck_lang::{check, Profile};

let mut names = Interner::new();
let mut b = Builder::new();
let one = b.int(1);
let x = b.new_binder(Name::new(names.intern("x")), hir_lang::BinderKind::Local);
let pat = b.bind(x);
let stmt = b.let_stmt(pat, Some(one));
let body = b.block(&[stmt], None);
let f = b.func(Name::new(names.intern("f")), &[], body);
let root = b.module(None, &[f]);
let hir = b.finish(root)?;
let typed = check(&hir, &Profile::static_explicit()).unwrap();
let out = irgen_lang::lower(&hir, &typed, &names)?;
assert_eq!(out.stats().functions, 1);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## `Lowerer`

```rust,ignore
pub struct Lowerer { /* private */ }   // Clone, Copy, Debug, Default, PartialEq, Eq
```

| Method | Description |
|---|---|
| `Lowerer::new(Options) -> Lowerer` | A lowering with these options. |
| `options(&self) -> &Options` | The options. |
| `lower(&self, &Hir, &Typed, &impl Lookup) -> Result<Lowered, LowerError>` | As [`lower`](#lower). |

```rust
use hir_lang::Builder;
use intern_lang::Interner;
use irgen_lang::{ErrorMode, Lowerer, Options};
use typeck_lang::{check, Profile};

let mut b = Builder::new();
let root = b.module(None, &[]);
let hir = b.finish(root)?;
let typed = check(&hir, &Profile::static_explicit()).unwrap();
let lowerer = Lowerer::new(Options::new().with_errors(ErrorMode::Trap));
assert_eq!(lowerer.options().errors(), ErrorMode::Trap);
assert_eq!(lowerer.lower(&hir, &typed, &Interner::new())?.module().function_count(), 0);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## `Options`

```rust,ignore
pub struct Options { /* private */ }   // Clone, Copy, Debug, Default, PartialEq, Eq, Hash
```

| Method | Default | Description |
|---|---|---|
| `Options::new()` | | The defaults. |
| `with_errors(ErrorMode)`, `errors()` | `Raise` | What run-time errors do. |
| `with_max_steps(u64)`, `max_steps() -> Option<u64>` | `None`: 64 per HIR node + 2^20 | Lowering steps (HIR nodes lowered; an inlined constant counts at each use). |
| `with_max_global_bytes(u64)`, `max_global_bytes()` | [`DEFAULT_MAX_GLOBAL_BYTES`](#default_max_global_bytes) | Total bytes of global initializers. |
| `with_statepoint_budget(usize)`, `statepoint_budget()` | `ir_lang::DEFAULT_STATEPOINT_BUDGET` | Statepoint insertion work per function. |

```rust
use irgen_lang::{ErrorMode, Options};

let o = Options::new()
    .with_errors(ErrorMode::Trap)
    .with_max_steps(1 << 24)
    .with_max_global_bytes(1 << 20)
    .with_statepoint_budget(1 << 20);
assert_eq!(o.max_steps(), Some(1 << 24));
assert_eq!(o.max_global_bytes(), 1 << 20);
assert_eq!(o.statepoint_budget(), 1 << 20);
assert_eq!(Options::default(), Options::new());
```

## `ErrorMode`

`Raise` (default) or `Trap` (`#[non_exhaustive]`). See
[Run-time errors](#run-time-errors).

```rust
use irgen_lang::ErrorMode;

assert_eq!(ErrorMode::default(), ErrorMode::Raise);
```

## `DEFAULT_MAX_GLOBAL_BYTES`

`64 << 20` (64 MiB): the default cap on global initializer bytes.

```rust
assert_eq!(irgen_lang::DEFAULT_MAX_GLOBAL_BYTES, 64 << 20);
```

## `Lowered`

```rust,ignore
pub struct Lowered { /* private */ }   // Clone, Debug
```

| Method | Returns |
|---|---|
| `module()` | `&ir_lang::Module`: every function validated. |
| `into_module()` | The module, by value. |
| `function(ItemId) -> Option<FuncId>` | The function a function item became (`None` for generic functions, members of impls, classes, and interfaces, and other items). |
| `module_body(ItemId) -> Option<FuncId>` | The function holding a module's body. |
| `global(ItemId) -> Option<GlobalId>` | The global a global item became (`None` for unit globals). |
| `functions() -> &[(ItemId, FuncId)]` | Every lowered function item, in item order. |
| `stats() -> Stats` | What lowering did. |

```rust
use hir_lang::{Builder, Effects, Item, ItemKind, Name, Prim, Ty};
use intern_lang::Interner;
use typeck_lang::{check, Profile};

let mut names = Interner::new();
let mut b = Builder::new();
// global COUNT: i64 = 7;  and a module body
let ty = b.ty(Ty::Prim(Prim::I64));
let seven = b.int(7);
let g = b.item(Item::new(
    Some(Name::new(names.intern("COUNT"))),
    ItemKind::Global { ty: Some(ty), mutable: true, init: Some(seven) },
));
let body = b.block(&[], None);
let items = b.list(&[g]);
let root = b.item(Item::new(None, ItemKind::Module { items, body: Some(body), effects: Effects::NONE }));
let hir = b.finish(root)?;
let typed = check(&hir, &Profile::static_explicit()).unwrap();
let out = irgen_lang::lower(&hir, &typed, &names)?;

let gid = out.global(g).unwrap();
assert_eq!(out.module().global_name(gid), Some("COUNT"));
let body = out.module_body(root).unwrap();
assert_eq!(out.module().func_decl(body).unwrap().name(), "__module_body");
assert!(out.functions().is_empty());
# Ok::<(), Box<dyn std::error::Error>>(())
```

## `Stats`

`#[non_exhaustive]`, `Default`; public fields:

| Field | Meaning |
|---|---|
| `functions` | Bodies built (functions and module bodies). |
| `imports` | Host functions imported (`extern` callees and the exception allocator). |
| `globals` | Globals declared. |
| `aggregates` | Aggregates declared. |
| `steps` | Lowering steps spent. |
| `safepoints` | Safepoints whose GC lists statepoint insertion filled. |
| `relocations` | References relocated, over all functions. |

```rust
let s = irgen_lang::Stats::default();
assert_eq!((s.functions, s.relocations), (0, 0));
```

## `LowerError`

`#[non_exhaustive]`; `Clone`, `Debug`, `PartialEq`, `Eq`, `Display`,
`core::error::Error`.

| Variant | Meaning | What to do |
|---|---|---|
| `TypeErrors { count }` | typeck-lang reported errors. | Fix the program; typeck's diagnostics say what. |
| `Unsupported { construct, node, span }` | A [construct](#construct) outside the 0.2 subset, at its first node. | Avoid it, or wait for the milestone that lowers it. |
| `BudgetExceeded { limit }` | A [budget](#limit) ran out. | Raise it in [`Options`](#options), or reject the input. |
| `ConstCycle { item }` | A constant's value refers to itself. | Fix the program. |
| `MissingType { node }` | `typed` has no type for a node: it is not this HIR's check. | Pass the matching `Typed`. |
| `Module(ModuleError)` | ir-lang refused a declaration (a host function named like a defined one). | Rename. |
| `Build { node, error }`, `Invalid(ValidationError)`, `Internal { node }` | irgen-lang produced IR ir-lang refused, or broke an invariant: a bug. | Report it with the input. |

```rust
use irgen_lang::{Limit, LowerError};

let e = LowerError::BudgetExceeded { limit: Limit::Steps };
assert_eq!(e.to_string(), "budget exceeded: lowering steps");
```

## `Construct`

What an `Unsupported` error names (`#[non_exhaustive]`, `Copy`, `Ord`,
`Display`). `name()` is a short name; `planned_for_0_5()` says whether the
roadmap's 0.5 lowers it.

| Construct | What | Planned |
|---|---|---|
| `DynamicProfile` | A unit checked under the dynamic or gradual discipline. | 0.5 |
| `DynamicValue` | A value of type `any`; `promote`. | 0.5 |
| `Closure` | A closure. | 0.5 (closure conversion) |
| `Generic` | A generic item or a use of one with type arguments. | 0.5 (monomorphization) |
| `Coroutine` | `async`/generator functions, `await`, `yield`, `yield from`, `spawn`. | 0.5 (state machines) |
| `Sum` | Sum types and variants. | 0.5 (decision trees) |
| `Class` | Classes, interfaces, impl members, `dyn`, `impl Trait`. | roadmap |
| `MethodCall` | `x.f()`, `T::f`. | roadmap |
| `FunctionValue` | A function used as a value; an indirect call. | roadmap |
| `ForeignUnit` | A definition of another unit. | roadmap |
| `String` | Strings, byte strings, big integers. | roadmap |
| `Nullable` | `null`, nullable, union, intersection types. | roadmap |
| `Collection` | Maps, slices. | roadmap |
| `Reference` | `&e`, `*e`, references and pointers. | roadmap |
| `DynamicAccess` | `$o->$name`, `$$name`, `$a[]`, reference assignment. | roadmap |
| `Intrinsic`, `Asm` | Intrinsics; inline assembly. | roadmap |
| `CallShape` | Named, spread, or place arguments; defaults, rest, named-only, `by_ref` parameters. | roadmap |
| `Static` | Function statics, `global` statements. | roadmap |
| `NonConstant` | A global initializer or pattern constant that is not constant. | |
| `Coercion` | A coercion other than widening; a cast outside the scalar casts. | |
| `HostBoundary` | A host value not called; aggregates across the host boundary. | |
| `GcRefInMemory` | A `ref` in an aggregate or a global, or thrown. | |
| `RecursiveType` | A record containing itself by value. | |
| `Abi` | An `abi` other than `C`, `sysv64`, `win64`, `aapcs64`. | |
| `Pattern` | Slice, reference, by-reference patterns; a type test outside `catch`; a refutable parameter. | |
| `DynamicCatch` | A `catch` arm binding without a type test. | |
| `ThrowValue` | A thrown value that is not a scalar. | |
| `ErrorNode` | An error node left by lenient validation. | |
| `Unresolved` | A path not fully resolved. | |
| `UnionRecord`, `StructuralRecord`, `OtherType` | Union-layout records; structural record types; any other type form. | |

```rust
use irgen_lang::Construct;

assert_eq!(Construct::Closure.name(), "closure");
assert!(Construct::Generic.planned_for_0_5());
assert_eq!(Construct::Sum.to_string(), "sum type (needs a decision tree)");
```

## `Limit`

`Steps`, `GlobalBytes`, `FrameSize` (a stack slot over 2^32 − 1 bytes),
`Statepoints` (`#[non_exhaustive]`, `Display`).

```rust
assert_eq!(irgen_lang::Limit::FrameSize.to_string(), "stack slot size");
```

## `rt`

The run-time interface lowered code relies on.

| Item | Value | Meaning |
|---|---|---|
| `EXCEPTION` | `"__irgen_exception"` | `import (u32 tag, u64 bits) -> (ptr) [nounwind, nogc]`: allocates an exception record. |
| `RECORD_SIZE`, `RECORD_ALIGN` | 16, 8 | The record's layout. |
| `TAG_OFFSET`, `BITS_OFFSET` | 0, 8 | Where the `u32` tag and the `u64` bits are (little-endian). |
| `TAG_ERROR` | 0 | A run-time error; the bits are its code. |
| `E_INDEX_OUT_OF_BOUNDS` | 102 | An index outside `0..len`. |
| `E_NO_MATCH` | 200 | A `match` with no matching arm. |
| `tag(Prim) -> Option<u32>` | 1..=14 | The tag of a thrown value of that primitive type (`None` for `str`). |
| `prim_of_tag(u32) -> Option<Prim>` | | The inverse. |

A thrown value's bits are its bits zero-extended to 64 (`bool` 0/1, floats
their IEEE bits). An exception that leaves the outermost function is a
`resume` whose payload is the record's address. A host implements
`__irgen_exception`; ir-lang's interpreter does it through its `Host` trait:

```rust
use hir_lang::Prim;
use irgen_lang::rt;

// What a host stores for `throw 7i64`:
let mut record = [0u8; rt::RECORD_SIZE as usize];
let tag = rt::tag(Prim::I64).unwrap();
record[rt::TAG_OFFSET as usize..][..4].copy_from_slice(&tag.to_le_bytes());
record[rt::BITS_OFFSET as usize..][..8].copy_from_slice(&7u64.to_le_bytes());
assert_eq!(rt::prim_of_tag(u32::from_le_bytes(record[..4].try_into().unwrap())), Some(Prim::I64));
```

## Feature flags

| Feature | Default | Effect |
|---|---|---|
| `std` | yes | The standard library (forwarded to hir-lang, typeck-lang, ir-lang, intern-lang); without it the crate is `no_std` + `alloc`. |

## Limits

| Limit | Value |
|---|---|
| Lowering steps | 64 per HIR node + 2^20 by default ([`Options::with_max_steps`](#options)) |
| Global initializer bytes | 64 MiB by default |
| Stack slot size | 2^32 − 1 bytes (ir-lang's) |
| Aggregate size | 2^48 bytes (ir-lang's) |
| Statepoint work per function | 2^26 by default |
| Pointer width | 64 bits |

## Stability

0.2.0 is pre-1.0: the surface may change in 0.x minor releases, each change
recorded in [`CHANGELOG.md`](../CHANGELOG.md). Every public enum is
`#[non_exhaustive]`; `Stats` is too, so new statistics are additive. The
ir-lang dependency is pinned to `=2.0.0-alpha.2` because ir-lang's 2.0 surface
may still change before 2.0.0. MSRV is 1.85.
