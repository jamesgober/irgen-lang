//! The result of lowering.

use alloc::vec::Vec;

use hir_lang::ItemId;
use ir_lang::{FuncId, GlobalId, Module};

/// What lowering did, for observability and benchmarks.
///
/// # Examples
///
/// ```
/// let s = irgen_lang::Stats::default();
/// assert_eq!(s.functions, 0);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Stats {
    /// Function bodies built (functions and module bodies).
    pub functions: usize,
    /// Host functions imported (`extern` callees and the exception
    /// allocator).
    pub imports: usize,
    /// Globals declared.
    pub globals: usize,
    /// Aggregates declared (records, tuples, arrays).
    pub aggregates: usize,
    /// Lowering steps spent (HIR nodes lowered, counting inlined constants
    /// once per use).
    pub steps: u64,
    /// Safepoints whose GC lists statepoint insertion filled.
    pub safepoints: usize,
    /// GC references relocated across safepoints, over all functions.
    pub relocations: usize,
}

/// A lowered unit: the ir-lang module and where each HIR item went.
///
/// Every function in the module has passed ir-lang's validator, statepoint
/// and exception-edge rules included.
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
/// let f = b.func(Name::new(names.intern("main")), &[], body);
/// let root = b.module(None, &[f]);
/// let hir = b.finish(root)?;
/// let typed = check(&hir, &Profile::static_explicit()).unwrap();
///
/// let out = irgen_lang::lower(&hir, &typed, &names)?;
/// let main = out.function(f).unwrap();
/// assert_eq!(out.module().func_decl(main).unwrap().name(), "main");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct Lowered {
    pub(crate) module: Module,
    pub(crate) functions: Vec<(ItemId, FuncId)>,
    pub(crate) bodies: Vec<(ItemId, FuncId)>,
    pub(crate) globals: Vec<(ItemId, GlobalId)>,
    pub(crate) stats: Stats,
}

impl Lowered {
    /// The ir-lang module.
    ///
    /// # Examples
    ///
    /// See [`Lowered`].
    #[must_use]
    pub fn module(&self) -> &Module {
        &self.module
    }

    /// The module, by value (for opt-lang or a backend).
    ///
    /// # Examples
    ///
    /// ```
    /// # use hir_lang::Builder;
    /// # use intern_lang::Interner;
    /// # use typeck_lang::{check, Profile};
    /// # let mut b = Builder::new();
    /// # let root = b.module(None, &[]);
    /// # let hir = b.finish(root)?;
    /// # let typed = check(&hir, &Profile::static_explicit()).unwrap();
    /// let module = irgen_lang::lower(&hir, &typed, &Interner::new())?.into_module();
    /// assert_eq!(module.function_count(), 0);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn into_module(self) -> Module {
        self.module
    }

    /// The function a HIR function item was lowered to (`None` for items that
    /// are not lowered: generic functions, members of impls, classes, and
    /// interfaces, and non-function items).
    ///
    /// # Examples
    ///
    /// See [`Lowered`].
    #[must_use]
    pub fn function(&self, item: ItemId) -> Option<FuncId> {
        lookup(&self.functions, item)
    }

    /// The function holding a module item's body (its top-level code), if it
    /// has one.
    ///
    /// # Examples
    ///
    /// ```
    /// use hir_lang::{Builder, Effects, Item, ItemKind, List};
    /// use intern_lang::Interner;
    /// use typeck_lang::{check, Profile};
    ///
    /// let mut b = Builder::new();
    /// let one = b.int(1);
    /// let body = b.block(&[], Some(one));
    /// let root = b.item(Item::new(None, ItemKind::Module { items: List::EMPTY, body: Some(body), effects: Effects::NONE }));
    /// let hir = b.finish(root)?;
    /// let typed = check(&hir, &Profile::static_explicit()).unwrap();
    /// let out = irgen_lang::lower(&hir, &typed, &Interner::new())?;
    /// assert!(out.module_body(root).is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn module_body(&self, module: ItemId) -> Option<FuncId> {
        lookup(&self.bodies, module)
    }

    /// The global a HIR global item was lowered to (`None` for unit-typed
    /// globals, which need no storage).
    ///
    /// # Examples
    ///
    /// ```
    /// use hir_lang::{Builder, Item, ItemKind, Name, Prim, Ty};
    /// use intern_lang::Interner;
    /// use typeck_lang::{check, Profile};
    ///
    /// let mut names = Interner::new();
    /// let mut b = Builder::new();
    /// let ty = b.ty(Ty::Prim(Prim::I64));
    /// let init = b.int(7);
    /// let g = b.item(Item::new(Some(hir_lang::Name::new(names.intern("G"))),
    ///     ItemKind::Global { ty: Some(ty), mutable: true, init: Some(init) }));
    /// let root = b.module(None, &[g]);
    /// let hir = b.finish(root)?;
    /// let typed = check(&hir, &Profile::static_explicit()).unwrap();
    /// let out = irgen_lang::lower(&hir, &typed, &names)?;
    /// let id = out.global(g).unwrap();
    /// assert_eq!(out.module().global_name(id), Some("G"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn global(&self, item: ItemId) -> Option<GlobalId> {
        lookup(&self.globals, item)
    }

    /// Every lowered function item and its function, in item order.
    ///
    /// # Examples
    ///
    /// See [`Lowered::function`].
    #[must_use]
    pub fn functions(&self) -> &[(ItemId, FuncId)] {
        &self.functions
    }

    /// What lowering did.
    ///
    /// # Examples
    ///
    /// ```
    /// # use hir_lang::Builder;
    /// # use intern_lang::Interner;
    /// # use typeck_lang::{check, Profile};
    /// # let mut b = Builder::new();
    /// # let root = b.module(None, &[]);
    /// # let hir = b.finish(root)?;
    /// # let typed = check(&hir, &Profile::static_explicit()).unwrap();
    /// let out = irgen_lang::lower(&hir, &typed, &Interner::new())?;
    /// assert_eq!(out.stats().functions, 0);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.stats
    }
}

fn lookup<T: Copy>(table: &[(ItemId, T)], item: ItemId) -> Option<T> {
    table
        .binary_search_by(|(k, _)| k.cmp(&item))
        .ok()
        .and_then(|i| table.get(i))
        .map(|&(_, v)| v)
}
