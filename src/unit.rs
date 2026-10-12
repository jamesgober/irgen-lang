//! Lowering a whole unit: collecting items, declaring functions and globals,
//! then building each body.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    string::String,
    vec,
    vec::Vec,
};

use hir_lang::{Control, ExprId, Hir, ItemId, ItemKind, NodeRef, ParamKind, Pat, PatId, Symbol};
use intern_lang::Lookup;
use ir_lang::{Attrs, CallConv, FuncId, Global, GlobalId, Linkage, Module, Signature, Type};
use typeck_lang::{Discipline, Severity, Typed};

use crate::{
    consteval,
    error::{Construct, Limit, LowerError},
    func,
    options::Options,
    output::{Lowered, Stats},
    rt, scan,
    types::{Repr, TypeMap},
};

/// "No entry" in the dense item-to-entry tables.
pub(crate) const NONE: u32 = u32::MAX;

/// Visits `root` and its descendants in HIR's canonical order (preorder over
/// `children_into`), with an explicit stack. `visit` may skip a node's
/// children or stop the walk.
pub(crate) fn preorder(hir: &Hir, root: NodeRef, mut visit: impl FnMut(NodeRef) -> Control) {
    let mut stack = vec![root];
    let mut kids: Vec<NodeRef> = Vec::new();
    while let Some(n) = stack.pop() {
        match visit(n) {
            Control::Stop => return,
            Control::Skip => continue,
            _ => {}
        }
        kids.clear();
        hir.children_into(n, &mut kids);
        stack.extend(kids.iter().rev());
    }
}

/// An `Unsupported` error at `node`, with its span.
pub(crate) fn unsupported(hir: &Hir, construct: Construct, node: NodeRef) -> LowerError {
    LowerError::Unsupported {
        construct,
        node,
        span: hir.origin(node).span,
    }
}

/// What lowering reads: the unit, its types, the interner, the options.
pub(crate) struct Ctx<'a, L> {
    pub(crate) hir: &'a Hir,
    pub(crate) typed: &'a Typed,
    pub(crate) names: &'a L,
    pub(crate) options: Options,
}

/// Whether a function entry is a function item or a module's body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FnKind {
    Item,
    ModuleBody,
}

/// A function to lower (or an imported one, without a body).
#[derive(Clone, Debug)]
pub(crate) struct FnEntry {
    pub(crate) item: ItemId,
    pub(crate) kind: FnKind,
    pub(crate) func: FuncId,
    /// Each parameter's pattern and representation (unit parameters are in
    /// the HIR signature but not in the ir-lang one).
    pub(crate) params: Vec<(PatId, Repr)>,
    pub(crate) ret: Repr,
    pub(crate) body: Option<ExprId>,
}

/// A host function called from lowered code.
#[derive(Clone, Debug)]
pub(crate) struct ExternFn {
    pub(crate) func: FuncId,
    pub(crate) params: Vec<Repr>,
    pub(crate) ret: Repr,
}

/// A global item.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GlobalEntry {
    /// `None` for a unit-typed global (no storage).
    pub(crate) id: Option<GlobalId>,
    pub(crate) repr: Repr,
}

/// Everything declared about the unit, read while bodies are built.
pub(crate) struct Tables {
    pub(crate) types: TypeMap,
    pub(crate) fns: Vec<FnEntry>,
    fn_index: Vec<u32>,
    /// Item index to a marker for function items that are not lowered.
    pub(crate) fn_skipped: BTreeMap<ItemId, Construct>,
    pub(crate) globals: Vec<GlobalEntry>,
    global_index: Vec<u32>,
    pub(crate) externs: BTreeMap<Symbol, ExternFn>,
    /// `__irgen_exception`, once some function needs it.
    pub(crate) runtime: Option<FuncId>,
    /// Constants whose values have been pre-scanned.
    pub(crate) consts_scanned: BTreeSet<ItemId>,
}

impl Tables {
    pub(crate) fn function(&self, item: ItemId) -> Option<&FnEntry> {
        let i = *self.fn_index.get(item.index())?;
        self.fns.get(i as usize)
    }

    pub(crate) fn global(&self, item: ItemId) -> Option<GlobalEntry> {
        let i = *self.global_index.get(item.index())?;
        self.globals.get(i as usize).copied()
    }
}

/// Mutable state shared by every function's lowering: the step budget and
/// scratch tables reused across functions.
pub(crate) struct Budget {
    pub(crate) steps: u64,
    pub(crate) max_steps: u64,
}

impl Budget {
    #[inline]
    pub(crate) fn tick(&mut self) -> Result<(), LowerError> {
        self.steps = self.steps.saturating_add(1);
        if self.steps > self.max_steps {
            return Err(LowerError::BudgetExceeded {
                limit: Limit::Steps,
            });
        }
        Ok(())
    }
}

/// A collected item.
#[derive(Clone, Copy, Debug)]
enum Collected {
    Fn(ItemId),
    ModuleBody(ItemId, ExprId),
    Global(ItemId),
}

/// Lowers a whole unit.
pub(crate) fn lower_unit<L: Lookup>(
    hir: &Hir,
    typed: &Typed,
    names: &L,
    options: Options,
) -> Result<Lowered, LowerError> {
    let root = hir.root();
    match typed.profile().discipline {
        Discipline::StaticExplicit | Discipline::StaticInferred => {}
        _ => {
            return Err(unsupported(
                hir,
                Construct::DynamicProfile,
                NodeRef::Item(root),
            ));
        }
    }
    let count = typed
        .diagnostics()
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    if count > 0 {
        return Err(LowerError::TypeErrors { count });
    }

    let nodes = total_nodes(hir);
    let max_steps = options
        .max_steps()
        .unwrap_or_else(|| (nodes as u64).saturating_mul(64).saturating_add(1 << 20));
    let mut budget = Budget {
        steps: 0,
        max_steps,
    };
    let cx = Ctx {
        hir,
        typed,
        names,
        options,
    };

    let (collected, symbols, skipped) = collect(&cx)?;
    let module_name = module_name(&cx);
    let mut module = Module::new(&module_name);
    let mut tables = Tables {
        types: TypeMap::new(hir, typed),
        fns: Vec::new(),
        fn_index: vec![NONE; hir.count(hir_lang::IdKind::Item)],
        fn_skipped: skipped,
        globals: Vec::new(),
        global_index: vec![NONE; hir.count(hir_lang::IdKind::Item)],
        externs: BTreeMap::new(),
        runtime: None,
        consts_scanned: BTreeSet::new(),
    };

    // Declarations first: every body may call every function.
    for (&c, name) in collected.iter().zip(&symbols) {
        match c {
            Collected::Fn(item) => declare_fn(&cx, &mut module, &mut tables, item, name)?,
            Collected::ModuleBody(item, body) => {
                declare_body(&cx, &mut module, &mut tables, item, body, name)?;
            }
            Collected::Global(_) => {}
        }
    }
    let mut global_bytes = 0u64;
    for (&c, name) in collected.iter().zip(&symbols) {
        if let Collected::Global(item) = c {
            declare_global(
                &cx,
                &mut module,
                &mut tables,
                &mut budget,
                &mut global_bytes,
                item,
                name,
            )?;
        }
    }

    let mut stats = Stats {
        globals: tables.globals.iter().filter(|g| g.id.is_some()).count(),
        ..Stats::default()
    };
    let mut scratch = func::Scratch::new(hir);
    for index in 0..tables.fns.len() {
        let Some(entry) = tables.fns.get(index).cloned() else {
            continue;
        };
        let Some(body) = entry.body else {
            continue;
        };
        scan::scan_function(&cx, &mut module, &mut tables, &mut budget, &entry, body)?;
        let sp =
            func::lower_function(&cx, &mut module, &tables, &mut budget, &mut scratch, &entry)?;
        stats.functions += 1;
        stats.safepoints += sp.safepoints;
        stats.relocations += sp.relocations;
    }
    module.validate()?;

    let mut functions: Vec<(ItemId, FuncId)> = Vec::new();
    let mut bodies: Vec<(ItemId, FuncId)> = Vec::new();
    for f in &tables.fns {
        match f.kind {
            FnKind::Item => functions.push((f.item, f.func)),
            FnKind::ModuleBody => bodies.push((f.item, f.func)),
        }
    }
    functions.sort_unstable_by_key(|p| p.0);
    bodies.sort_unstable_by_key(|p| p.0);
    let mut globals: Vec<(ItemId, GlobalId)> = Vec::new();
    for (index, &slot) in tables.global_index.iter().enumerate() {
        if slot == NONE {
            continue;
        }
        let (Some(item), Some(g)) = (ItemId::from_index(index), tables.globals.get(slot as usize))
        else {
            continue;
        };
        if let Some(id) = g.id {
            globals.push((item, id));
        }
    }
    stats.imports = tables.externs.len() + usize::from(tables.runtime.is_some());
    stats.aggregates = tables.types.aggs.len();
    stats.steps = budget.steps;
    Ok(Lowered {
        module,
        functions,
        bodies,
        globals,
        stats,
    })
}

fn total_nodes(hir: &Hir) -> usize {
    use hir_lang::IdKind;
    [
        IdKind::Item,
        IdKind::Expr,
        IdKind::Stmt,
        IdKind::Pat,
        IdKind::Ty,
        IdKind::Path,
    ]
    .iter()
    .map(|&k| hir.count(k))
    .fold(0usize, usize::saturating_add)
}

fn symbol_text<L: Lookup>(names: &L, sym: Symbol) -> String {
    names
        .resolve_with(sym, |s| String::from(s))
        .unwrap_or_else(|| format!("sym{}", sym.as_u32()))
}

fn module_name<L: Lookup>(cx: &Ctx<'_, L>) -> String {
    match cx.hir.item(cx.hir.root()).name {
        Some(n) => symbol_text(cx.names, n.sym),
        None => format!("unit{}", cx.hir.unit().as_u32()),
    }
}

/// One walk over the item tree: the functions, module bodies, and globals to
/// lower, with qualified, unique symbol names, and the function items that
/// are deliberately not lowered (generic ones, members).
#[allow(clippy::type_complexity)]
fn collect<L: Lookup>(
    cx: &Ctx<'_, L>,
) -> Result<(Vec<Collected>, Vec<String>, BTreeMap<ItemId, Construct>), LowerError> {
    enum Step {
        Enter(NodeRef),
        /// Leaving an item: pop its name segment.
        Leave,
    }
    let hir = cx.hir;
    let mut out: Vec<Collected> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut skipped: BTreeMap<ItemId, Construct> = BTreeMap::new();
    let mut used: BTreeSet<String> = BTreeSet::new();
    // The enclosing items' name segments (`None` for unnamed ones).
    let mut path: Vec<Option<String>> = Vec::new();
    let mut stack = vec![Step::Enter(NodeRef::Item(hir.root()))];
    let mut kids: Vec<NodeRef> = Vec::new();
    while let Some(step) = stack.pop() {
        let n = match step {
            Step::Leave => {
                let _ = path.pop();
                continue;
            }
            Step::Enter(n) => n,
        };
        let id = match n {
            NodeRef::Item(id) => id,
            // Types never hold items.
            NodeRef::Ty(_) => continue,
            other => {
                kids.clear();
                hir.children_into(other, &mut kids);
                stack.extend(kids.iter().rev().map(|&k| Step::Enter(k)));
                continue;
            }
        };
        let item = hir.item(id);
        let segment = item.name.map(|n| symbol_text(cx.names, n.sym));
        let mut qualified = String::new();
        for seg in path.iter().flatten() {
            qualified.push_str(seg);
            qualified.push_str("::");
        }
        let mut emit = |leaf: &str, c: Collected| {
            let mut name = qualified.clone();
            name.push_str(leaf);
            if used.contains(&name) {
                name = format!("{name}#{}", id.index());
            }
            let _ = used.insert(name.clone());
            out.push(c);
            names.push(name);
        };
        let mut descend = true;
        match item.kind {
            ItemKind::Fn(def) => {
                if def.generics.params.is_empty() {
                    emit(segment.as_deref().unwrap_or("fn"), Collected::Fn(id));
                } else {
                    let _ = skipped.insert(id, Construct::Generic);
                }
            }
            ItemKind::Module {
                body: Some(body), ..
            } => {
                let leaf = match &segment {
                    Some(s) => format!("{s}::__module_body"),
                    None => String::from("__module_body"),
                };
                emit(&leaf, Collected::ModuleBody(id, body));
            }
            ItemKind::Global { .. } => {
                emit(
                    segment.as_deref().unwrap_or("global"),
                    Collected::Global(id),
                );
            }
            ItemKind::Impl(_) | ItemKind::Class(_) | ItemKind::Interface(_) => {
                // Members are reached through methods and associated paths,
                // which this release refuses; record them so a direct use says why.
                let members = match item.kind {
                    ItemKind::Impl(d) => d.items,
                    ItemKind::Class(d) => d.items,
                    ItemKind::Interface(d) => d.items,
                    _ => hir_lang::List::EMPTY,
                };
                for &m in hir.list(members) {
                    let _ = skipped.insert(m, Construct::Class);
                }
                descend = false;
            }
            _ => {}
        }
        if descend {
            path.push(segment);
            stack.push(Step::Leave);
            kids.clear();
            hir.children_into(n, &mut kids);
            stack.extend(kids.iter().rev().map(|&k| Step::Enter(k)));
        }
    }
    Ok((out, names, skipped))
}

/// The ir-lang calling convention of a function `abi`.
fn conv_of<L: Lookup>(cx: &Ctx<'_, L>, abi: Symbol, at: NodeRef) -> Result<CallConv, LowerError> {
    let text = symbol_text(cx.names, abi);
    match text.as_str() {
        "C" | "c" => Ok(CallConv::C),
        "sysv64" => Ok(CallConv::SysV64),
        "win64" => Ok(CallConv::Win64),
        "aapcs64" => Ok(CallConv::Aapcs64),
        _ => Err(unsupported(cx.hir, Construct::Abi, at)),
    }
}

/// The ir-lang signature of a function with these parameters and result:
/// aggregates travel by address (a result through a hidden first pointer),
/// unit and never take no slot.
pub(crate) fn ir_signature(params: &[Repr], ret: Repr) -> Signature {
    let mut ps: Vec<Type> = Vec::with_capacity(params.len() + 1);
    if let Repr::Agg(_) = ret {
        ps.push(Type::Ptr);
    }
    for p in params {
        match p {
            Repr::Scalar(s) => ps.push(s.ir()),
            Repr::Agg(_) => ps.push(Type::Ptr),
            Repr::Unit | Repr::Never => {}
        }
    }
    let rs: Vec<Type> = match ret {
        Repr::Scalar(s) => vec![s.ir()],
        _ => Vec::new(),
    };
    Signature::new(&ps, &rs)
}

fn declare_fn<L: Lookup>(
    cx: &Ctx<'_, L>,
    module: &mut Module,
    tables: &mut Tables,
    item: ItemId,
    name: &str,
) -> Result<(), LowerError> {
    let hir = cx.hir;
    let at = NodeRef::Item(item);
    let it = hir.item(item);
    let ItemKind::Fn(def) = it.kind else {
        return Ok(());
    };
    if def.effects.contains(hir_lang::Effects::ASYNC)
        || def.effects.contains(hir_lang::Effects::YIELD)
    {
        return Err(unsupported(hir, Construct::Coroutine, at));
    }
    let mut params = Vec::new();
    for &p in hir.list(def.params) {
        let param = hir.param(p);
        let pat_at = NodeRef::Param(p);
        if param.default.is_some()
            || param.by_ref
            || !matches!(param.kind, ParamKind::Normal | ParamKind::PositionalOnly)
        {
            return Err(unsupported(hir, Construct::CallShape, pat_at));
        }
        let t = cx
            .typed
            .pat(param.pat)
            .ok_or(LowerError::MissingType { node: pat_at })?;
        let r = tables.types.of_type(hir, cx.typed, module, t, pat_at)?;
        params.push((param.pat, r));
    }
    let ret = match def.ret {
        Some(ty) => tables.types.of_ty(hir, cx.typed, module, ty, at)?,
        None => Repr::Unit,
    };
    let reprs: Vec<Repr> = params.iter().map(|p| p.1).collect();
    let mut sig = ir_signature(&reprs, ret);
    if let Some(abi) = def.abi {
        sig = sig.with_conv(conv_of(cx, abi, at)?);
    }
    let linkage = match (def.body, it.vis) {
        (None, _) if def.abi.is_some() => Linkage::Import,
        (None, _) => {
            // A body-less function outside an interface: nothing to lower.
            let _ = tables.fn_skipped.insert(item, Construct::Class);
            return Ok(());
        }
        (Some(_), hir_lang::Vis::Public) => Linkage::Export,
        (Some(_), _) => Linkage::Local,
    };
    let func = module.declare_function(name, &sig, linkage)?;
    if ret == Repr::Never {
        module.set_attrs(func, Attrs::NORETURN)?;
    }
    let index = u32::try_from(tables.fns.len()).map_err(|_| capacity())?;
    if let Some(slot) = tables.fn_index.get_mut(item.index()) {
        *slot = index;
    }
    tables.fns.push(FnEntry {
        item,
        kind: FnKind::Item,
        func,
        params,
        ret,
        body: def.body,
    });
    Ok(())
}

fn declare_body<L: Lookup>(
    cx: &Ctx<'_, L>,
    module: &mut Module,
    tables: &mut Tables,
    item: ItemId,
    body: ExprId,
    name: &str,
) -> Result<(), LowerError> {
    if let ItemKind::Module { effects, .. } = cx.hir.item(item).kind {
        if effects.contains(hir_lang::Effects::ASYNC) || effects.contains(hir_lang::Effects::YIELD)
        {
            return Err(unsupported(
                cx.hir,
                Construct::Coroutine,
                NodeRef::Item(item),
            ));
        }
    }
    let func = module.declare_function(name, &Signature::new(&[], &[]), Linkage::Export)?;
    tables.fns.push(FnEntry {
        item,
        kind: FnKind::ModuleBody,
        func,
        params: Vec::new(),
        ret: Repr::Unit,
        body: Some(body),
    });
    Ok(())
}

fn declare_global<L: Lookup>(
    cx: &Ctx<'_, L>,
    module: &mut Module,
    tables: &mut Tables,
    budget: &mut Budget,
    total: &mut u64,
    item: ItemId,
    name: &str,
) -> Result<(), LowerError> {
    let hir = cx.hir;
    let at = NodeRef::Item(item);
    let it = hir.item(item);
    let ItemKind::Global { ty, mutable, init } = it.kind else {
        return Ok(());
    };
    let Some(ty) = ty else {
        return Err(LowerError::MissingType { node: at });
    };
    let repr = tables.types.of_ty(hir, cx.typed, module, ty, at)?;
    let id = match repr {
        Repr::Unit | Repr::Never => {
            if let Some(e) = init {
                // Still a constant: refuse what could not be evaluated.
                let _ = consteval::eval(cx, &tables.types, budget, e, repr, 0)?;
            }
            None
        }
        Repr::Scalar(crate::types::Scalar::Host) => {
            return Err(unsupported(hir, Construct::GcRefInMemory, at));
        }
        Repr::Scalar(_) | Repr::Agg(_) => {
            let (size, align) = consteval::size_align(&tables.types, repr);
            let remaining = cx.options.max_global_bytes().saturating_sub(*total);
            if size > remaining {
                return Err(LowerError::BudgetExceeded {
                    limit: Limit::GlobalBytes,
                });
            }
            *total = total.saturating_add(size);
            let mut g = match init {
                Some(e) => {
                    let bytes = consteval::eval(cx, &tables.types, budget, e, repr, size)?;
                    Global::bytes(&bytes, align.max(1))
                }
                None => Global::zeroed(size, align.max(1)),
            };
            if mutable {
                g = g.mutable();
            }
            let linkage = match it.vis {
                hir_lang::Vis::Public => Linkage::Export,
                _ => Linkage::Local,
            };
            Some(module.declare_global(name, g.with_linkage(linkage))?)
        }
    };
    let index = u32::try_from(tables.globals.len()).map_err(|_| capacity())?;
    if let Some(slot) = tables.global_index.get_mut(item.index()) {
        *slot = index;
    }
    tables.globals.push(GlobalEntry { id, repr });
    Ok(())
}

/// Declares (once) the host function that allocates exception records.
pub(crate) fn runtime_exception(
    module: &mut Module,
    tables: &mut Tables,
) -> Result<FuncId, LowerError> {
    if let Some(f) = tables.runtime {
        return Ok(f);
    }
    let sig = Signature::new(&[Type::U32, Type::U64], &[Type::Ptr]);
    let f = module.declare_function(rt::EXCEPTION, &sig, Linkage::Import)?;
    module.set_attrs(f, Attrs::NOUNWIND | Attrs::NOGC)?;
    tables.runtime = Some(f);
    Ok(f)
}

/// Declares (once) a host function called by name.
pub(crate) fn declare_extern<L: Lookup>(
    cx: &Ctx<'_, L>,
    module: &mut Module,
    tables: &mut Tables,
    sym: Symbol,
    params: Vec<Repr>,
    ret: Repr,
) -> Result<(), LowerError> {
    if tables.externs.contains_key(&sym) {
        return Ok(());
    }
    let sig = ir_signature(&params, ret);
    let func = module.declare_function(&symbol_text(cx.names, sym), &sig, Linkage::Import)?;
    if ret == Repr::Never {
        module.set_attrs(func, Attrs::NORETURN)?;
    }
    let _ = tables.externs.insert(sym, ExternFn { func, params, ret });
    Ok(())
}

fn capacity() -> LowerError {
    LowerError::Build {
        node: None,
        error: ir_lang::BuildError::CapacityExceeded,
    }
}

/// Whether a pattern can fail to match (a `let` needs an `else` then).
pub(crate) fn refutable(hir: &Hir, pat: PatId) -> bool {
    let mut stack = vec![pat];
    while let Some(p) = stack.pop() {
        match *hir.pat(p) {
            Pat::Wild => {}
            Pat::Bind { sub, .. } => stack.extend(sub),
            Pat::Tuple { elems, .. } | Pat::Ctor { elems, .. } => {
                stack.extend_from_slice(hir.list(elems));
            }
            Pat::Record { fields, .. } => stack.extend(hir.list(fields).iter().map(|f| f.pat)),
            Pat::Ident { path, .. } => {
                // A binding unless it names a constant.
                if matches!(hir.path(path).res, hir_lang::Res::Def(_)) {
                    return true;
                }
            }
            _ => return true,
        }
    }
    false
}
