//! Why a unit was not lowered.

use core::fmt;

use hir_lang::{ItemId, NodeRef, Span};
use ir_lang::{BuildError, ModuleError, ValidationError};

/// A construct outside the subset irgen-lang lowers.
///
/// 0.3 lowers (as 0.2 did) the **static-explicit subset**: non-generic functions over
/// scalars, records, tuples, and fixed-length arrays, with the control flow,
/// operations, calls, and exceptions of HIR. Every other form is refused with
/// the variant naming it, at the first node (in HIR's canonical order) that
/// uses it. The variants marked *0.5* are on the roadmap's next milestone;
/// the rest are listed there too, so nothing is dropped silently.
///
/// # Examples
///
/// ```
/// use irgen_lang::Construct;
///
/// assert_eq!(Construct::Closure.name(), "closure");
/// assert_eq!(Construct::Generic.to_string(), "generic item (needs monomorphization)");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Construct {
    /// A unit checked under the dynamic (or gradual) discipline: every value is
    /// `any`, which needs the dynamic-value representation. *0.5.*
    DynamicProfile,
    /// A value of type `any`, or an operation on one. *0.5.*
    DynamicValue,
    /// A closure (needs closure conversion). *0.5.*
    Closure,
    /// A generic function, record, or alias, or a use of one with type
    /// arguments (needs monomorphization from typeck-lang's plan). *0.5.*
    Generic,
    /// `async` or generator functions, `await`, `yield`, `yield from`, `spawn`
    /// (coroutines as state machines). *0.5.*
    Coroutine,
    /// A sum type, a variant, or a pattern over one (match-lang decision
    /// trees). *0.5.*
    Sum,
    /// A class, an interface, an `impl` member, `dyn`, or `impl Trait`.
    Class,
    /// A method call (`x.f()`), or an associated item reached through a type
    /// (`T::f`).
    MethodCall,
    /// A function used as a value, or a call of something that is not a
    /// function item or a host function (an indirect call).
    FunctionValue,
    /// A definition of another compilation unit.
    ForeignUnit,
    /// A string, byte string, or big-integer literal, or a value of type `str`.
    String,
    /// `null`, or a nullable, union, or intersection type.
    Nullable,
    /// A map, a slice, or a map literal.
    Collection,
    /// A reference or pointer: `&e`, `*e`, `&T`, `*T`.
    Reference,
    /// A run-time-named access: `$o->$name`, `$o->$m()`, `$$name`, `$a[]`, and
    /// reference assignment (`$a = &$b`).
    DynamicAccess,
    /// An intrinsic (atomics, volatile access, named host intrinsics).
    Intrinsic,
    /// Inline assembly.
    Asm,
    /// A call with named, spread, or place arguments, or a parameter with a
    /// default, a rest or named-only kind, or `by_ref`.
    CallShape,
    /// A function-static or `global` statement.
    Static,
    /// A global initializer or a pattern constant that is not a constant
    /// (literals, tuples, arrays, records, repeats, and constants of those).
    NonConstant,
    /// An implicit conversion other than numeric widening.
    Coercion,
    /// A host (`extern`) value that is not called, or a host function taking
    /// or returning a record, tuple, or array.
    HostBoundary,
    /// A garbage-collected reference (a host type) inside a record, tuple,
    /// array, or global, or thrown: memory is not traced, so references live
    /// only in SSA values.
    GcRefInMemory,
    /// A record that contains itself by value.
    RecursiveType,
    /// A function `abi` other than `C`, `sysv64`, `win64`, or `aapcs64`.
    Abi,
    /// A pattern form this release does not test: slices, references, by-reference
    /// bindings, a type test outside a `catch` arm.
    Pattern,
    /// A `catch` arm that binds a thrown value without testing its type
    /// (the binding would be `any`).
    DynamicCatch,
    /// A thrown value that is not a scalar (`bool`, a number, or `char`).
    ThrowValue,
    /// An error node left by lenient validation.
    ErrorNode,
    /// A path that is not fully resolved.
    Unresolved,
    /// A record with union layout (`is_union`).
    UnionRecord,
    /// A structural record type (the `Structural` record-typing profile).
    StructuralRecord,
    /// Any other type form (typeck-lang may add type forms in minor
    /// releases).
    OtherType,
}

impl Construct {
    /// The construct's short name.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(irgen_lang::Construct::Coroutine.name(), "coroutine");
    /// ```
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DynamicProfile => "dynamic profile",
            Self::DynamicValue => "dynamic value",
            Self::Closure => "closure",
            Self::Generic => "generic item",
            Self::Coroutine => "coroutine",
            Self::Sum => "sum type",
            Self::Class => "class or interface",
            Self::MethodCall => "method call",
            Self::FunctionValue => "function value",
            Self::ForeignUnit => "foreign definition",
            Self::String => "string",
            Self::Nullable => "nullable or union type",
            Self::Collection => "map or slice",
            Self::Reference => "reference or pointer",
            Self::DynamicAccess => "dynamic access",
            Self::Intrinsic => "intrinsic",
            Self::Asm => "inline assembly",
            Self::CallShape => "call shape",
            Self::Static => "static or global statement",
            Self::NonConstant => "non-constant initializer",
            Self::Coercion => "coercion",
            Self::HostBoundary => "host boundary",
            Self::GcRefInMemory => "GC reference in memory",
            Self::RecursiveType => "recursive type",
            Self::Abi => "abi",
            Self::Pattern => "pattern",
            Self::DynamicCatch => "untyped catch binding",
            Self::ThrowValue => "thrown value",
            Self::ErrorNode => "error node",
            Self::Unresolved => "unresolved path",
            Self::UnionRecord => "union record",
            Self::StructuralRecord => "structural record",
            Self::OtherType => "type",
        }
    }

    /// Whether the roadmap's next milestone (0.5) lowers this construct.
    ///
    /// # Examples
    ///
    /// ```
    /// use irgen_lang::Construct;
    ///
    /// assert!(Construct::Closure.planned_for_0_5());
    /// assert!(!Construct::Asm.planned_for_0_5());
    /// ```
    #[must_use]
    pub const fn planned_for_0_5(self) -> bool {
        matches!(
            self,
            Self::DynamicProfile
                | Self::DynamicValue
                | Self::Closure
                | Self::Generic
                | Self::Coroutine
                | Self::Sum
        )
    }
}

impl fmt::Display for Construct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generic => f.write_str("generic item (needs monomorphization)"),
            Self::Closure => f.write_str("closure (needs closure conversion)"),
            Self::Coroutine => f.write_str("coroutine (needs a state-machine transform)"),
            Self::Sum => f.write_str("sum type (needs a decision tree)"),
            other => f.write_str(other.name()),
        }
    }
}

/// Which budget ran out.
///
/// # Examples
///
/// ```
/// assert_eq!(irgen_lang::Limit::Steps.to_string(), "lowering steps");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Limit {
    /// Lowering steps ([`Options::with_max_steps`](crate::Options::with_max_steps)):
    /// HIR nodes lowered, counting every inlined use of a constant.
    Steps,
    /// Bytes of global initializers ([`Options::with_max_global_bytes`](crate::Options::with_max_global_bytes)).
    GlobalBytes,
    /// A stack slot larger than ir-lang allows (2^32 − 1 bytes).
    FrameSize,
    /// Statepoint insertion work ([`Options::with_statepoint_budget`](crate::Options::with_statepoint_budget)).
    Statepoints,
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Steps => "lowering steps",
            Self::GlobalBytes => "global initializer bytes",
            Self::FrameSize => "stack slot size",
            Self::Statepoints => "statepoint insertion work",
        })
    }
}

/// Why a unit was not lowered.
///
/// Every variant names what was refused and where. The `Build` and `Invalid`
/// variants mean irgen-lang produced IR that ir-lang refused, which is a bug
/// in irgen-lang: please report it with the input.
///
/// # Examples
///
/// ```
/// use hir_lang::{Builder, Name};
/// use intern_lang::Interner;
/// use irgen_lang::{Construct, LowerError};
/// use typeck_lang::{check, Profile};
///
/// // A unit checked under the dynamic profile is outside the static subset.
/// let mut names = Interner::new();
/// let mut b = Builder::new();
/// let body = b.block(&[], None);
/// let f = b.func(Name::new(names.intern("f")), &[], body);
/// let root = b.module(None, &[f]);
/// let hir = b.finish(root)?;
/// let typed = check(&hir, &Profile::dynamic()).unwrap();
/// let err = irgen_lang::lower(&hir, &typed, &names).unwrap_err();
/// assert!(matches!(err, LowerError::Unsupported { construct: Construct::DynamicProfile, .. }));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LowerError {
    /// The type check reported errors: lowering needs a well-typed unit. Fix
    /// the program (typeck-lang's diagnostics say what is wrong).
    TypeErrors {
        /// How many error diagnostics typeck-lang reported.
        count: usize,
    },
    /// A construct outside the static subset this release lowers.
    Unsupported {
        /// What it is.
        construct: Construct,
        /// The first node using it.
        node: NodeRef,
        /// That node's source span.
        span: Span,
    },
    /// A budget ran out (hostile or very large input).
    BudgetExceeded {
        /// Which one.
        limit: Limit,
    },
    /// A constant whose value refers to itself, directly or through other
    /// constants.
    ConstCycle {
        /// A constant on the cycle.
        item: ItemId,
    },
    /// typeck-lang's result has no type for a node lowering needs: the
    /// [`Typed`](typeck_lang::Typed) was not produced from this HIR.
    MissingType {
        /// The node.
        node: NodeRef,
    },
    /// ir-lang refused a declaration, for example a symbol name used twice by
    /// a host function and a defined function.
    Module(ModuleError),
    /// ir-lang refused an instruction. A bug in irgen-lang.
    Build {
        /// The HIR node being lowered, when known.
        node: Option<NodeRef>,
        /// What ir-lang said.
        error: BuildError,
    },
    /// A lowered function failed ir-lang's validator. A bug in irgen-lang.
    Invalid(ValidationError),
    /// An invariant of the lowering did not hold (the pre-scan accepted a
    /// node the lowering cannot handle). A bug in irgen-lang.
    Internal {
        /// The HIR node being lowered, when known.
        node: Option<NodeRef>,
    },
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TypeErrors { count } => {
                write!(
                    f,
                    "the unit has {count} type error(s); lowering needs a well-typed unit"
                )
            }
            Self::Unsupported {
                construct,
                node,
                span,
            } => write!(
                f,
                "{construct} at {node:?} ({}..{}) is outside the subset irgen-lang lowers",
                span.start(),
                span.end()
            ),
            Self::BudgetExceeded { limit } => write!(f, "budget exceeded: {limit}"),
            Self::ConstCycle { item } => write!(f, "constant {item:?} refers to itself"),
            Self::MissingType { node } => {
                write!(
                    f,
                    "no type for {node:?}: the type-check result is not from this HIR"
                )
            }
            Self::Module(e) => write!(f, "ir-lang refused a declaration: {e}"),
            Self::Build { node, error } => match node {
                Some(n) => write!(f, "ir-lang refused an instruction for {n:?}: {error}"),
                None => write!(f, "ir-lang refused an instruction: {error}"),
            },
            Self::Invalid(e) => write!(f, "lowered IR failed validation: {e}"),
            Self::Internal { node } => match node {
                Some(n) => write!(f, "internal lowering error at {n:?}"),
                None => f.write_str("internal lowering error"),
            },
        }
    }
}

impl core::error::Error for LowerError {}

impl From<ModuleError> for LowerError {
    fn from(e: ModuleError) -> Self {
        Self::Module(e)
    }
}

impl From<ValidationError> for LowerError {
    fn from(e: ValidationError) -> Self {
        Self::Invalid(e)
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::*;

    #[test]
    fn test_construct_names_are_distinct() {
        let all = [
            Construct::DynamicProfile,
            Construct::DynamicValue,
            Construct::Closure,
            Construct::Generic,
            Construct::Coroutine,
            Construct::Sum,
            Construct::Class,
            Construct::MethodCall,
            Construct::FunctionValue,
            Construct::ForeignUnit,
            Construct::String,
            Construct::Nullable,
            Construct::Collection,
            Construct::Reference,
            Construct::DynamicAccess,
            Construct::Intrinsic,
            Construct::Asm,
            Construct::CallShape,
            Construct::Static,
            Construct::NonConstant,
            Construct::Coercion,
            Construct::HostBoundary,
            Construct::GcRefInMemory,
            Construct::RecursiveType,
            Construct::Abi,
            Construct::Pattern,
            Construct::DynamicCatch,
            Construct::ThrowValue,
            Construct::ErrorNode,
            Construct::Unresolved,
            Construct::UnionRecord,
            Construct::StructuralRecord,
            Construct::OtherType,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.name(), b.name());
            }
        }
    }

    #[test]
    fn test_error_display_names_the_problem() {
        let e = LowerError::BudgetExceeded {
            limit: Limit::Steps,
        };
        assert_eq!(e.to_string(), "budget exceeded: lowering steps");
        let e = LowerError::TypeErrors { count: 2 };
        assert!(e.to_string().contains("2 type error"));
    }
}
