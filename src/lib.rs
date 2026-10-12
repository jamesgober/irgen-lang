//! # irgen_lang
//!
//! Lowers HIR (the representation every LexerSketch language translates
//! into) to ir-lang 2.0 SSA, the input of the family's own native compiler
//! path: irgen-lang, then opt-lang, then backend-lang's instruction
//! selection and register allocation, then machine code and object files.
//! No Cranelift, no LLVM.
//!
//! ## The lazy path
//!
//! Build or lower a unit, type-check it with typeck-lang, and [`lower`] it:
//!
//! ```
//! use hir_lang::{Builder, Name, OpKind, Prim, Ty, Param};
//! use intern_lang::Interner;
//! use typeck_lang::{check, Profile};
//!
//! // fn add(a: i64, b: i64) -> i64 { a + b }
//! let mut names = Interner::new();
//! let mut b = Builder::new();
//! let mut param = |b: &mut Builder, n: &str| {
//!     let x = b.new_binder(Name::new(names.intern(n)), hir_lang::BinderKind::Param);
//!     let pat = b.bind(x);
//!     let ty = b.ty(Ty::Prim(Prim::I64));
//!     (b.param(Param { ty: Some(ty), ..Param::new(pat) }), x)
//! };
//! let (pa, a) = param(&mut b, "a");
//! let (pb, bb) = param(&mut b, "b");
//! let (ua, ub) = (b.use_binder(a), b.use_binder(bb));
//! let sum = b.op(OpKind::Add, &[ua, ub]);
//! let body = b.block(&[], Some(sum));
//! let ret = b.ty(Ty::Prim(Prim::I64));
//! let params = b.list(&[pa, pb]);
//! let add = b.item(hir_lang::Item::new(
//!     Some(Name::new(names.intern("add"))),
//!     hir_lang::ItemKind::Fn(hir_lang::FnDef { params, ret: Some(ret), body: Some(body), ..Default::default() }),
//! ));
//! let root = b.module(None, &[add]);
//! let hir = b.finish(root)?;
//! let typed = check(&hir, &Profile::static_explicit()).unwrap();
//!
//! let out = irgen_lang::lower(&hir, &typed, &names)?;
//! let f = out.function(add).unwrap();
//! assert!(out.module().display_function(f).to_string().contains("check add<overflow=error>"));
//! out.module().validate()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! `a + b` under HIR's default `overflow = error` policy can raise
//! `ArithOverflow`, so it lowers to a `check` terminator whose error edge
//! raises the error as an exception ([`ErrorMode`]).
//!
//! ## What it lowers
//!
//! The **static-explicit subset**: non-generic functions and module bodies;
//! locals as SSA values (ir-lang's variable construction); blocks, labels,
//! `if`, the loop primitive with its step, `break` and `continue` with
//! labels, `return`; `match` over scalars, tuples, and records (literal,
//! range, constant, binding, wildcard, tuple, record, and or-patterns, with
//! guards; runs of literal arms become one `switch`); direct calls of
//! function items and host (`extern`) functions; records, tuples, and
//! fixed-length arrays in stack slots and globals (ir-lang's aggregate
//! layout), with bounds-checked indexing; `let_place` and compound
//! assignment with the place evaluated once; every OPS operation and cast
//! with its exact policy, as a `check` where ir-lang's `can_error` says so;
//! `throw`, `try`/`catch` (type tests on the thrown value's type)/`finally`
//! and `defer`, through `invoke` and landing pads; host types as GC
//! references, with statepoints inserted by ir-lang's `insert_statepoints`.
//!
//! Anything else (dynamic values, closures, generics, coroutines, sum
//! types, methods, strings, references) is a [`LowerError::Unsupported`]
//! naming the [`Construct`] and the first node that uses it.
//!
//! ## The native path
//!
//! The lowered module is what opt-lang optimizes and backend-lang compiles to
//! x86-64 machine code. The crate's native end-to-end test lowers HIR
//! programs, optimizes them, compiles them, and runs the machine code on the
//! processor (x86-64 Linux and Windows), comparing every outcome with
//! ir-lang's interpreter and an independent HIR evaluator;
//! `examples/native.rs` shows the pipeline. opt-lang and backend-lang are
//! dev-dependencies only: this crate's output is the ir-lang module.
//!
//! ## Guarantees
//!
//! - **Valid output.** Every function passes ir-lang's validator, statepoint
//!   and exception-edge rules included; lowering fails with
//!   [`LowerError::Invalid`] rather than return a module that does not.
//! - **Semantics.** Differential tests run generated programs through this
//!   lowering and ir-lang's reference interpreter, and through an
//!   independent evaluator of the HIR subset, comparing results, thrown
//!   values, run-time errors, and traps.
//! - **No recursion over the input.** Lowering, type mapping, pattern
//!   tests, and constant evaluation use explicit stacks; any nesting depth
//!   lowers in bounded native stack.
//! - **Budgeted and deterministic.** Steps, global initializer bytes, and
//!   statepoint work are capped ([`Options`]); the same input lowers to the
//!   same module.
//!
//! ## `no_std`
//!
//! The crate needs only `alloc`. The default `std` feature is additive.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(unused_must_use)]
#![deny(unused_results)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]

extern crate alloc;

mod consteval;
mod error;
mod func;
mod options;
mod output;
pub mod rt;
mod scan;
mod types;
mod unit;

pub use error::{Construct, Limit, LowerError};
pub use options::{DEFAULT_MAX_GLOBAL_BYTES, ErrorMode, Options};
pub use output::{Lowered, Stats};

/// Compiles and runs the `rust` code blocks in `README.md`, `docs/API.md`, and
/// the current release notes as part of `cargo test`, so the published
/// examples cannot drift from the API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
#[doc = include_str!("../docs/API.md")]
#[doc = include_str!("../docs/release/v0.3.0.md")]
pub struct MarkdownDocTests;

use hir_lang::Hir;
use intern_lang::Lookup;
use typeck_lang::Typed;

/// Lowers a type-checked unit to an ir-lang module with the default
/// [`Options`].
///
/// `typed` is typeck-lang's result for `hir` (under a static profile);
/// `names` is the interner the unit's names live in (function and global
/// symbols are read from it).
///
/// # Errors
///
/// A [`LowerError`]: type errors in `typed`, a construct outside the subset
/// (the first in the unit's canonical order), a budget, or a constant that
/// refers to itself.
///
/// # Examples
///
/// ```
/// use hir_lang::{Builder, Name};
/// use intern_lang::Interner;
/// use typeck_lang::{check, Profile};
///
/// let mut names = Interner::new();
/// let mut b = Builder::new();
/// let body = b.block(&[], None);
/// let main = b.func(Name::new(names.intern("main")), &[], body);
/// let root = b.module(None, &[main]);
/// let hir = b.finish(root)?;
/// let typed = check(&hir, &Profile::static_explicit()).unwrap();
/// let out = irgen_lang::lower(&hir, &typed, &names)?;
/// assert_eq!(out.stats().functions, 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn lower<L: Lookup>(hir: &Hir, typed: &Typed, names: &L) -> Result<Lowered, LowerError> {
    Lowerer::new(Options::new()).lower(hir, typed, names)
}

/// A configured lowering: [`Options`] for the error mode and the budgets.
///
/// # Examples
///
/// ```
/// use hir_lang::Builder;
/// use intern_lang::Interner;
/// use irgen_lang::{ErrorMode, Lowerer, Options};
/// use typeck_lang::{check, Profile};
///
/// let mut b = Builder::new();
/// let root = b.module(None, &[]);
/// let hir = b.finish(root)?;
/// let typed = check(&hir, &Profile::static_explicit()).unwrap();
/// let lowerer = Lowerer::new(Options::new().with_errors(ErrorMode::Trap));
/// assert_eq!(lowerer.options().errors(), ErrorMode::Trap);
/// let out = lowerer.lower(&hir, &typed, &Interner::new())?;
/// assert_eq!(out.module().function_count(), 0);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lowerer {
    options: Options,
}

impl Lowerer {
    /// A lowering with these options.
    ///
    /// # Examples
    ///
    /// See [`Lowerer`].
    #[must_use]
    pub const fn new(options: Options) -> Self {
        Self { options }
    }

    /// The options.
    ///
    /// # Examples
    ///
    /// See [`Lowerer`].
    #[must_use]
    pub const fn options(&self) -> &Options {
        &self.options
    }

    /// Lowers a type-checked unit.
    ///
    /// # Errors
    ///
    /// As [`lower`].
    ///
    /// # Examples
    ///
    /// See [`Lowerer`].
    pub fn lower<L: Lookup>(
        &self,
        hir: &Hir,
        typed: &Typed,
        names: &L,
    ) -> Result<Lowered, LowerError> {
        unit::lower_unit(hir, typed, names, self.options)
    }
}
