//! Lowering options.

/// What a run-time error does in lowered code.
///
/// Run-time errors are the OPS errors of operations whose policy is `error`
/// (E0001–E0006), an index out of bounds (E0102), and a `match` with no
/// matching arm (E0200). HIR leaves the choice to the language's
/// `[runtime] errors` setting (IR.md §8.1).
///
/// # Examples
///
/// ```
/// use irgen_lang::{ErrorMode, Options};
///
/// assert_eq!(Options::new().errors(), ErrorMode::Raise);
/// assert_eq!(Options::new().with_errors(ErrorMode::Trap).errors(), ErrorMode::Trap);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorMode {
    /// Raise the error as an exception: catchable by a wildcard `catch` arm,
    /// running `finally` and `defer` bodies, unwinding out of the function
    /// otherwise. The payload is an exception record with
    /// [`rt::TAG_ERROR`](crate::rt::TAG_ERROR) and the error's code.
    #[default]
    Raise,
    /// Abort the program deterministically: `trap` with the error's code.
    Trap,
}

/// How [`Lowerer`](crate::Lowerer) lowers: the error mode and the budgets.
///
/// # Examples
///
/// ```
/// use irgen_lang::{ErrorMode, Options};
///
/// let o = Options::new().with_max_steps(1 << 20).with_errors(ErrorMode::Trap);
/// assert_eq!(o.max_steps(), Some(1 << 20));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Options {
    errors: ErrorMode,
    max_steps: Option<u64>,
    max_global_bytes: u64,
    statepoint_budget: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self::new()
    }
}

/// The default cap on the bytes of all global initializers together: 64 MiB.
pub const DEFAULT_MAX_GLOBAL_BYTES: u64 = 64 << 20;

impl Options {
    /// The defaults: [`ErrorMode::Raise`], a step budget proportional to the
    /// unit's size, 64 MiB of global initializers, and ir-lang's default
    /// statepoint budget.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(irgen_lang::Options::new().max_steps(), None);
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            errors: ErrorMode::Raise,
            max_steps: None,
            max_global_bytes: DEFAULT_MAX_GLOBAL_BYTES,
            statepoint_budget: ir_lang::DEFAULT_STATEPOINT_BUDGET,
        }
    }

    /// What run-time errors do.
    ///
    /// # Examples
    ///
    /// ```
    /// use irgen_lang::{ErrorMode, Options};
    ///
    /// assert_eq!(Options::new().with_errors(ErrorMode::Trap).errors(), ErrorMode::Trap);
    /// ```
    #[must_use]
    pub const fn with_errors(mut self, errors: ErrorMode) -> Self {
        self.errors = errors;
        self
    }

    /// The error mode.
    ///
    /// # Examples
    ///
    /// See [`Options::with_errors`].
    #[must_use]
    pub const fn errors(&self) -> ErrorMode {
        self.errors
    }

    /// Caps lowering work at `steps` (HIR nodes lowered, counting each inlined
    /// use of a constant once per use). Without a cap, the budget is 64 steps
    /// per HIR node of the unit plus 2^20, so only constant inlining that
    /// multiplies work can reach it.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(irgen_lang::Options::new().with_max_steps(5).max_steps(), Some(5));
    /// ```
    #[must_use]
    pub const fn with_max_steps(mut self, steps: u64) -> Self {
        self.max_steps = Some(steps);
        self
    }

    /// The explicit step cap, if one was set.
    ///
    /// # Examples
    ///
    /// See [`Options::with_max_steps`].
    #[must_use]
    pub const fn max_steps(&self) -> Option<u64> {
        self.max_steps
    }

    /// Caps the bytes of all global initializers together (default
    /// [`DEFAULT_MAX_GLOBAL_BYTES`]), so `[0; 1 << 40]` in a global is an
    /// error rather than an allocation.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(irgen_lang::Options::new().with_max_global_bytes(64).max_global_bytes(), 64);
    /// ```
    #[must_use]
    pub const fn with_max_global_bytes(mut self, bytes: u64) -> Self {
        self.max_global_bytes = bytes;
        self
    }

    /// The global-initializer cap.
    ///
    /// # Examples
    ///
    /// See [`Options::with_max_global_bytes`].
    #[must_use]
    pub const fn max_global_bytes(&self) -> u64 {
        self.max_global_bytes
    }

    /// The work budget passed to ir-lang's statepoint insertion for each
    /// function (relocations plus block parameters; default
    /// [`ir_lang::DEFAULT_STATEPOINT_BUDGET`]).
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(irgen_lang::Options::new().with_statepoint_budget(9).statepoint_budget(), 9);
    /// ```
    #[must_use]
    pub const fn with_statepoint_budget(mut self, budget: usize) -> Self {
        self.statepoint_budget = budget;
        self
    }

    /// The statepoint budget.
    ///
    /// # Examples
    ///
    /// See [`Options::with_statepoint_budget`].
    #[must_use]
    pub const fn statepoint_budget(&self) -> usize {
        self.statepoint_budget
    }
}
