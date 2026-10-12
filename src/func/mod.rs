//! Lowering one function body.
//!
//! The lowering is an explicit task stack over the HIR (never recursion over
//! the input): an expression task pushes its children's tasks and a
//! continuation, and continuations combine the children's values from a
//! value stack. Scalars become SSA values through ir-lang's variable
//! construction (Braun et al.); aggregates live in stack slots. Control flow
//! is structured, so every block's predecessors are known when lowering
//! reaches it and it is sealed right there.
//!
//! Code after a jump (`return 1; f()`) is lowered into fresh blocks with no
//! predecessors ("dead" blocks); edges out of dead code are never emitted, so
//! dead code cannot add predecessors to live blocks, and every unreachable
//! block is removed before statepoint insertion.

mod expr;
mod flow;
mod ops;
mod pat;
mod place;

use alloc::{collections::BTreeMap, vec, vec::Vec};

use hir_lang::{BinderId, ExprId, ItemId, NodeRef, PatId, StmtId};
use intern_lang::Lookup;
use ir_lang::{
    Block, BuildError, MemFlags, MemType, Module, StackSlot, StatepointStats, Type, Value, Variable,
};

use crate::{
    error::{Limit, LowerError},
    types::{Repr, Scalar},
    unit::{Budget, Ctx, FnEntry, Tables},
};

pub(crate) use pat::Subject;

/// Where a binder's value lives.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Local {
    /// A scalar SSA variable.
    Var(Variable, Scalar),
    /// An aggregate in a stack slot.
    Slot(StackSlot, u32),
    /// An aggregate parameter: the caller's copy, by address.
    Ptr(Value, u32),
    /// Unit or never: nothing.
    Unit,
    /// A `let_place` alias, by index into the alias table.
    Alias(u32),
}

/// The value of an expression.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Val {
    /// Unit, never, or code that does not run.
    None,
    Scalar(Value, Scalar),
    /// An aggregate in a temporary stack slot, owned by the consumer.
    Mem(StackSlot, u32),
}

/// A place: where a read or a write goes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Place {
    Var(Variable, Scalar),
    /// Memory at an address, holding a value of this representation.
    Mem(Value, Repr),
    Unit,
    /// A value standing in for a place (the base of a field of a call's
    /// result); reading it gives the value.
    Value(Val),
}

/// Where a construct's result is collected from its several exits.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ResSlot {
    None,
    Var(Variable, Scalar),
    Slot(StackSlot, u32),
}

/// How the function returns.
#[derive(Clone, Copy, Debug)]
enum Ret {
    Unit,
    Never,
    Scalar(Variable),
    /// An aggregate result written through the hidden first parameter.
    Sret(Value, u32),
}

/// A way out of a cleanup region, after its body ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Exit {
    /// Normal completion: continue after the construct.
    Fallthrough,
    /// `break` to the loop or labeled block at this region index.
    Break(usize),
    /// `continue` the loop at this region index.
    Continue(usize),
    Return,
    /// Continue unwinding with the pending exception.
    Rethrow,
}

/// A cleanup region: a `finally` or a `defer` body that runs on every exit.
#[derive(Clone, Debug)]
pub(crate) struct Cleanup {
    entry: Block,
    /// Which exit to take after the body (an index into `exits`).
    dest: Variable,
    /// The pending exception, for `Rethrow`.
    exn: Variable,
    exits: Vec<Exit>,
    keys: BTreeMap<Exit, u32>,
    pad: Option<Block>,
    err: Option<Block>,
    /// Where `Fallthrough` goes; lowering continues there after the region.
    after: Block,
    body: ExprId,
}

/// A region on the region stack.
#[derive(Clone, Debug)]
pub(crate) enum Region {
    Loop {
        label: Option<BinderId>,
        cont: Block,
        brk: Block,
        res: ResSlot,
    },
    Label {
        label: BinderId,
        brk: Block,
        res: ResSlot,
    },
    /// A `try` with `catch` arms: exceptions inside go to `dispatch`.
    Handler {
        dispatch: Block,
        exn: Variable,
        pad: Option<Block>,
        err: Option<Block>,
    },
    Cleanup(alloc::boxed::Box<Cleanup>),
}

/// Work items of the lowering machine.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Task {
    Expr(ExprId),
    Place(ExprId),
    Stmt(StmtId),
    Coerce(ExprId),
    PushUnit,
    Discard,
    Op(ExprId),
    Cast(ExprId),
    Build(ExprId),
    Repeat(ExprId),
    Call(ExprId),
    Read,
    PlaceField(ExprId),
    PlaceIndex(ExprId),
    PlaceOfVal,
    Assign,
    CompoundRead,
    Compound(ExprId),
    LetPlace(ExprId),
    ConstExit,
    BlockEnd(u32),
    BlockDefers(u32),
    BlockFinish(u32),
    IfThen(u32),
    IfElse(u32),
    IfJoin(u32),
    LoopBody(u32),
    LoopStep(u32),
    MatchScrut(ExprId),
    MatchArm(u32),
    MatchArmEnd(u32),
    SwitchArm(u32),
    SwitchArmEnd(u32),
    Guard(u32),
    Break(ExprId),
    Return,
    Throw,
    TryBody(u32),
    CatchArm(u32),
    CatchArmEnd(u32),
    TryEnd(u32),
    TryResult(u32),
    CleanupDispatch(u32),
    LetInit(StmtId),
    LetElseEnd(u32),
}

/// Per-unit scratch reused by every function: the binder table, cleared
/// after each function through the list of entries it touched (so the cost
/// is linear in the function, not in the unit).
pub(crate) struct Scratch {
    binders: Vec<Option<Local>>,
    touched: Vec<usize>,
}

impl Scratch {
    pub(crate) fn new(hir: &hir_lang::Hir) -> Scratch {
        Scratch {
            binders: vec![None; hir.count(hir_lang::IdKind::Binder)],
            touched: Vec::new(),
        }
    }

    fn clear(&mut self) {
        for i in self.touched.drain(..) {
            if let Some(slot) = self.binders.get_mut(i) {
                *slot = None;
            }
        }
    }
}

/// Lowers one function into its declared body.
pub(crate) fn lower_function<L: Lookup>(
    cx: &Ctx<'_, L>,
    module: &mut Module,
    tables: &Tables,
    budget: &mut Budget,
    scratch: &mut Scratch,
    entry: &FnEntry,
) -> Result<StatepointStats, LowerError> {
    let b = module.build(entry.func)?;
    let mut f = FnCx {
        cx,
        t: tables,
        budget,
        scratch,
        b,
        tasks: Vec::new(),
        vals: Vec::new(),
        places: Vec::new(),
        aliases: Vec::new(),
        regions: Vec::new(),
        cleanup_idx: Vec::new(),
        exn_idx: Vec::new(),
        loop_idx: Vec::new(),
        labels: BTreeMap::new(),
        closings: Vec::new(),
        st: States::default(),
        consts: Vec::new(),
        const_set: alloc::collections::BTreeSet::new(),
        dead: false,
        has_pred: Vec::new(),
        ret: Ret::Unit,
        fn_err: None,
        node: None,
    };
    let r = f.lower(entry);
    f.scratch.clear();
    r
}

/// Side states of multi-step constructs, by index (push only, per function).
#[derive(Default)]
pub(crate) struct States {
    ifs: Vec<flow::IfSt>,
    loops: Vec<flow::LoopSt>,
    matches: Vec<flow::MatchSt>,
    tries: Vec<flow::TrySt>,
    blocks: Vec<flow::BlockSt>,
    lets: Vec<flow::LetSt>,
}

pub(crate) struct FnCx<'a, 'c, 'm, L> {
    cx: &'a Ctx<'c, L>,
    t: &'a Tables,
    budget: &'a mut Budget,
    scratch: &'a mut Scratch,
    b: ir_lang::Builder<'m>,
    tasks: Vec<Task>,
    vals: Vec<Val>,
    places: Vec<Place>,
    aliases: Vec<Place>,
    regions: Vec<Region>,
    /// Indices into `regions`, innermost last: the cleanups, the exception
    /// regions (handlers and cleanups), and the loops; and each label's
    /// region. Jumps and raises find their region in O(log n), whatever the
    /// nesting depth.
    cleanup_idx: Vec<usize>,
    exn_idx: Vec<usize>,
    loop_idx: Vec<usize>,
    labels: BTreeMap<BinderId, usize>,
    closings: Vec<(Cleanup, usize)>,
    st: States,
    /// Constants being inlined, innermost last, and the same as a set (the
    /// cycle check).
    consts: Vec<ItemId>,
    const_set: alloc::collections::BTreeSet<ItemId>,
    /// The current block is unreachable.
    dead: bool,
    /// Blocks that have a live predecessor edge.
    has_pred: Vec<bool>,
    ret: Ret,
    /// The function-level error block of `check` edges.
    fn_err: Option<Block>,
    /// The node being lowered, for error reports.
    node: Option<NodeRef>,
}

impl<L: Lookup> FnCx<'_, '_, '_, L> {
    fn lower(&mut self, entry: &FnEntry) -> Result<StatepointStats, LowerError> {
        let body = entry.body.ok_or_else(|| internal(None))?;
        let start = self.b.func().entry();
        self.mark(start);
        self.prologue(entry)?;
        self.tasks.push(Task::Expr(body));
        while let Some(task) = self.tasks.pop() {
            if let Err(e) = self.step(task) {
                return Err(match e {
                    LowerError::Build { node: None, error } => LowerError::Build {
                        node: self.node,
                        error,
                    },
                    LowerError::Internal { node: None } => LowerError::Internal { node: self.node },
                    other => other,
                });
            }
        }
        let v = self.pop_val()?;
        self.epilogue(v)?;
        self.finish()
    }

    fn prologue(&mut self, entry: &FnEntry) -> Result<(), LowerError> {
        let mut next = 0usize;
        self.ret = match entry.ret {
            Repr::Unit => Ret::Unit,
            Repr::Never => Ret::Never,
            Repr::Scalar(sc) => Ret::Scalar(self.b.declare_var(sc.ir())?),
            Repr::Agg(i) => {
                next = 1;
                Ret::Sret(self.b.param(0)?, i)
            }
        };
        for &(pat, repr) in &entry.params {
            let subject = match repr {
                Repr::Scalar(sc) => {
                    let v = self.b.param(next)?;
                    next += 1;
                    Subject::Scalar(v, sc)
                }
                Repr::Agg(i) => {
                    let v = self.b.param(next)?;
                    next += 1;
                    Subject::Mem(v, i)
                }
                Repr::Unit | Repr::Never => Subject::Unit,
            };
            if let Some(fail) = self.bind_root(pat, subject, None)? {
                // The pre-scan refuses refutable parameters; stay total.
                let saved = self.save()?;
                self.enter_sealed(fail)?;
                self.raise_const(crate::rt::E_NO_MATCH)?;
                self.restore(saved)?;
            }
        }
        Ok(())
    }

    fn epilogue(&mut self, v: Val) -> Result<(), LowerError> {
        match self.ret {
            Ret::Never => {
                if !self.dead {
                    self.b.unreachable()?;
                }
            }
            Ret::Unit => self.ret_now(&[])?,
            Ret::Scalar(var) => {
                if let Val::Scalar(x, _) = v {
                    self.b.def_var(var, x)?;
                }
                let x = self.b.use_var(var)?;
                self.ret_now(&[x])?;
            }
            Ret::Sret(dst, agg) => {
                if let Val::Mem(slot, _) = v {
                    let src = self.b.stack_addr(slot)?;
                    self.copy(dst, src, agg)?;
                }
                self.ret_now(&[])?;
            }
        }
        Ok(())
    }

    fn ret_now(&mut self, values: &[Value]) -> Result<(), LowerError> {
        if self.dead {
            self.b.unreachable()?;
        } else {
            self.b.ret(values)?;
        }
        Ok(())
    }

    /// Seals, removes unreachable blocks, inserts statepoints, compacts, and
    /// validates.
    fn finish(&mut self) -> Result<StatepointStats, LowerError> {
        self.b.seal_all_blocks()?;
        self.remove_unreachable()?;
        // Statepoint insertion (which validates its result) only matters
        // where GC references exist; the module is validated as a whole at
        // the end either way.
        let stats = if self.has_refs() {
            self.b
                .insert_statepoints_within(self.cx.options.statepoint_budget())
                .map_err(|e| match e {
                    BuildError::BudgetExceeded => LowerError::BudgetExceeded {
                        limit: Limit::Statepoints,
                    },
                    other => LowerError::Build {
                        node: None,
                        error: other,
                    },
                })?
        } else {
            StatepointStats::default()
        };
        let _ = self.b.compact();
        Ok(stats)
    }

    fn has_refs(&self) -> bool {
        let f = self.b.func();
        (0..f.value_count()).any(|i| {
            u32::try_from(i)
                .ok()
                .and_then(|i| f.value_type(Value::from_u32(i)))
                == Some(Type::Ref)
        })
    }

    fn remove_unreachable(&mut self) -> Result<(), LowerError> {
        let func = self.b.func();
        let n = func.block_count();
        let mut seen = vec![false; n];
        let mut stack = vec![func.entry()];
        if let Some(s) = seen.get_mut(func.entry().index()) {
            *s = true;
        }
        while let Some(b) = stack.pop() {
            if let Some(term) = func.terminator(b) {
                for succ in term.successors() {
                    if let Some(s) = seen.get_mut(succ.block.index()) {
                        if !*s {
                            *s = true;
                            stack.push(succ.block);
                        }
                    }
                }
            }
        }
        let dead: Vec<Block> = func
            .blocks()
            .filter(|b| !seen.get(b.index()).copied().unwrap_or(false))
            .collect();
        for b in dead {
            self.b.remove_block(b)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ regions

    fn push_region(&mut self, r: Region) -> usize {
        let i = self.regions.len();
        match &r {
            Region::Loop { label, .. } => {
                self.loop_idx.push(i);
                if let Some(l) = label {
                    let _ = self.labels.insert(*l, i);
                }
            }
            Region::Label { label, .. } => {
                let _ = self.labels.insert(*label, i);
            }
            Region::Handler { .. } => self.exn_idx.push(i),
            Region::Cleanup(_) => {
                self.cleanup_idx.push(i);
                self.exn_idx.push(i);
            }
        }
        self.regions.push(r);
        i
    }

    fn pop_region(&mut self) -> Option<Region> {
        let r = self.regions.pop()?;
        let i = self.regions.len();
        let pop = |v: &mut Vec<usize>| {
            if v.last() == Some(&i) {
                let _ = v.pop();
            }
        };
        match &r {
            Region::Loop { label, .. } => {
                pop(&mut self.loop_idx);
                if let Some(l) = label {
                    if self.labels.get(l) == Some(&i) {
                        let _ = self.labels.remove(l);
                    }
                }
            }
            Region::Label { label, .. } => {
                if self.labels.get(label) == Some(&i) {
                    let _ = self.labels.remove(label);
                }
            }
            Region::Handler { .. } => pop(&mut self.exn_idx),
            Region::Cleanup(_) => {
                pop(&mut self.cleanup_idx);
                pop(&mut self.exn_idx);
            }
        }
        Some(r)
    }

    // ------------------------------------------------------------ helpers

    fn tick(&mut self) -> Result<(), LowerError> {
        self.budget.tick()
    }

    fn pop_val(&mut self) -> Result<Val, LowerError> {
        self.vals.pop().ok_or_else(|| internal(self.node))
    }

    fn pop_place(&mut self) -> Result<Place, LowerError> {
        self.places.pop().ok_or_else(|| internal(self.node))
    }

    fn ensure(&mut self, b: Block) {
        let i = b.index();
        if self.has_pred.len() <= i {
            self.has_pred.resize(i + 1, false);
        }
    }

    fn mark(&mut self, b: Block) {
        self.ensure(b);
        if let Some(p) = self.has_pred.get_mut(b.index()) {
            *p = true;
        }
    }

    fn has_pred(&self, b: Block) -> bool {
        self.has_pred.get(b.index()).copied().unwrap_or(false)
    }

    fn new_block(&mut self) -> Result<Block, LowerError> {
        let b = self.b.create_block(&[])?;
        self.ensure(b);
        Ok(b)
    }

    fn new_block_with(&mut self, params: &[Type]) -> Result<Block, LowerError> {
        let b = self.b.create_block(params)?;
        self.ensure(b);
        Ok(b)
    }

    /// Continues lowering at the end of `b`.
    fn enter(&mut self, b: Block) -> Result<(), LowerError> {
        self.b.switch_to(b)?;
        self.dead = !self.has_pred(b);
        Ok(())
    }

    fn enter_sealed(&mut self, b: Block) -> Result<(), LowerError> {
        self.enter(b)?;
        self.b.seal_block(b)?;
        Ok(())
    }

    /// After a terminator, continues in a fresh unreachable block.
    fn go_dead(&mut self) -> Result<(), LowerError> {
        let b = self.new_block()?;
        self.b.switch_to(b)?;
        self.b.seal_block(b)?;
        self.dead = true;
        Ok(())
    }

    fn jump(&mut self, target: Block) -> Result<(), LowerError> {
        if self.dead {
            self.b.unreachable()?;
        } else {
            self.b.jump(target, &[])?;
            self.mark(target);
        }
        Ok(())
    }

    fn branch(&mut self, c: Value, then: Block, other: Block) -> Result<(), LowerError> {
        if self.dead {
            self.b.unreachable()?;
        } else {
            self.b.branch(c, then, &[], other, &[])?;
            self.mark(then);
            self.mark(other);
        }
        Ok(())
    }

    /// Branches on `c` and continues on the `true` side.
    fn branch_cont(&mut self, c: Value, fail: Block) -> Result<(), LowerError> {
        let cont = self.new_block()?;
        self.branch(c, cont, fail)?;
        self.enter_sealed(cont)
    }

    /// The current position, to come back to after emitting elsewhere.
    fn save(&self) -> Result<(Block, bool), LowerError> {
        let b = self.b.current_block().ok_or_else(|| internal(self.node))?;
        Ok((b, self.dead))
    }

    fn restore(&mut self, (b, dead): (Block, bool)) -> Result<(), LowerError> {
        self.b.switch_to(b)?;
        self.dead = dead;
        Ok(())
    }

    fn zero(&mut self, ty: Type) -> Result<Value, LowerError> {
        Ok(match ty {
            Type::Bool => self.b.bconst(false)?,
            Type::F32 => self.b.f32const(0.0)?,
            Type::F64 => self.b.f64const(0.0)?,
            Type::Ptr | Type::Ref => self.b.null(ty)?,
            _ => self.b.iconst(ty, 0)?,
        })
    }

    /// A constant of scalar `sc` from bits zero-extended to 64.
    fn const_bits(&mut self, sc: Scalar, bits: u64) -> Result<Value, LowerError> {
        let ty = sc.ir();
        Ok(match ty {
            Type::Bool => self.b.bconst(bits != 0)?,
            Type::F32 => self.b.f32const(f32::from_bits(bits as u32))?,
            Type::F64 => self.b.f64const(f64::from_bits(bits))?,
            _ => {
                let width = ty.int_bits().unwrap_or(64);
                let v = if ty.is_signed_int() {
                    let shift = 64 - width;
                    i128::from(((bits << shift) as i64) >> shift)
                } else {
                    i128::from(bits)
                };
                self.b.iconst(ty, v)?
            }
        })
    }

    fn agg(&self, i: u32) -> Result<&crate::types::AggInfo, LowerError> {
        self.t.types.agg(i).ok_or_else(|| internal(self.node))
    }

    fn new_slot(&mut self, agg: u32) -> Result<StackSlot, LowerError> {
        let info = self.agg(agg)?;
        if info.size > u64::from(u32::MAX) {
            return Err(LowerError::BudgetExceeded {
                limit: Limit::FrameSize,
            });
        }
        let id = info.id;
        Ok(self.b.stack_slot_for(MemType::Agg(id))?)
    }

    /// Copies an aggregate.
    fn copy(&mut self, dst: Value, src: Value, agg: u32) -> Result<(), LowerError> {
        let (size, align) = {
            let a = self.agg(agg)?;
            (a.size, a.align)
        };
        if size == 0 {
            return Ok(());
        }
        let len = self.b.iconst(Type::U64, i128::from(size))?;
        let _ = self.b.memcpy(dst, src, len, MemFlags::new(align.max(1)))?;
        Ok(())
    }

    fn load(&mut self, sc: Scalar, addr: Value) -> Result<Value, LowerError> {
        Ok(self.b.load(sc.ir(), addr, MemFlags::new(sc.size()))?)
    }

    fn store(&mut self, v: Value, sc: Scalar, addr: Value) -> Result<(), LowerError> {
        let _ = self.b.store(v, addr, MemFlags::new(sc.size()))?;
        Ok(())
    }

    /// Writes a value into memory holding representation `r`.
    fn write_mem(&mut self, addr: Value, r: Repr, v: Val) -> Result<(), LowerError> {
        match (r, v) {
            (Repr::Scalar(sc), Val::Scalar(x, _)) => self.store(x, sc, addr),
            (Repr::Agg(i), Val::Mem(slot, _)) => {
                let src = self.b.stack_addr(slot)?;
                self.copy(addr, src, i)
            }
            // Unit and never have no bytes; dead code has no value.
            _ => Ok(()),
        }
    }

    /// The representation of `e`'s type, before any coercion.
    fn repr_of(&self, e: ExprId) -> Result<Repr, LowerError> {
        let t = self.cx.typed.expr(e).ok_or(LowerError::MissingType {
            node: NodeRef::Expr(e),
        })?;
        self.t.types.known(t).ok_or(LowerError::MissingType {
            node: NodeRef::Expr(e),
        })
    }

    /// The representation of `e`'s value as its parent sees it (after its
    /// coercion).
    fn final_repr(&self, e: ExprId) -> Result<Repr, LowerError> {
        match self.cx.typed.coercion(e) {
            Some((_, t)) => self.t.types.known(t).ok_or(LowerError::MissingType {
                node: NodeRef::Expr(e),
            }),
            None => self.repr_of(e),
        }
    }

    fn res_for(&mut self, r: Repr) -> Result<ResSlot, LowerError> {
        Ok(match r {
            Repr::Scalar(sc) => ResSlot::Var(self.b.declare_var(sc.ir())?, sc),
            Repr::Agg(i) => ResSlot::Slot(self.new_slot(i)?, i),
            Repr::Unit | Repr::Never => ResSlot::None,
        })
    }

    fn store_res(&mut self, res: ResSlot, v: Val) -> Result<(), LowerError> {
        match (res, v) {
            (ResSlot::Var(var, _), Val::Scalar(x, _)) => self.b.def_var(var, x)?,
            (ResSlot::Slot(slot, agg), Val::Mem(src, _)) if src != slot => {
                let d = self.b.stack_addr(slot)?;
                let s = self.b.stack_addr(src)?;
                self.copy(d, s, agg)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn load_res(&mut self, res: ResSlot) -> Result<Val, LowerError> {
        Ok(match res {
            ResSlot::None => Val::None,
            ResSlot::Var(var, sc) => {
                if self.dead {
                    Val::Scalar(self.zero(sc.ir())?, sc)
                } else {
                    Val::Scalar(self.b.use_var(var)?, sc)
                }
            }
            ResSlot::Slot(slot, agg) => Val::Mem(slot, agg),
        })
    }

    fn local(&self, b: BinderId) -> Result<Local, LowerError> {
        self.scratch
            .binders
            .get(b.index())
            .copied()
            .flatten()
            .ok_or_else(|| internal(self.node))
    }

    fn set_local(&mut self, b: BinderId, l: Local) {
        let i = b.index();
        if let Some(slot) = self.scratch.binders.get_mut(i) {
            if slot.is_none() {
                self.scratch.touched.push(i);
            }
            *slot = Some(l);
        }
    }

    fn get_local(&self, b: BinderId) -> Option<Local> {
        self.scratch.binders.get(b.index()).copied().flatten()
    }

    /// Binds a root pattern (a parameter or a `let`) to a subject. A plain
    /// binding adopts the subject's storage; anything else runs the pattern
    /// test, and `fail` (or a no-match error) takes the refutable cases.
    fn bind_root(
        &mut self,
        pat: PatId,
        subject: Subject,
        fail: Option<Block>,
    ) -> Result<Option<Block>, LowerError> {
        if let hir_lang::Pat::Bind {
            binder, sub: None, ..
        } = *self.cx.hir.pat(pat)
        {
            let local = match subject {
                Subject::Scalar(v, sc) => {
                    let var = self.b.declare_var(sc.ir())?;
                    self.b.def_var(var, v)?;
                    Local::Var(var, sc)
                }
                Subject::Mem(addr, agg) => {
                    if self.is_param_ptr(addr) {
                        Local::Ptr(addr, agg)
                    } else {
                        let slot = self.new_slot(agg)?;
                        let d = self.b.stack_addr(slot)?;
                        self.copy(d, addr, agg)?;
                        Local::Slot(slot, agg)
                    }
                }
                Subject::Slot(slot, agg) => Local::Slot(slot, agg),
                Subject::Unit => Local::Unit,
            };
            self.set_local(binder, local);
            return Ok(fail);
        }
        self.test_pat(pat, subject, fail)
    }

    fn is_param_ptr(&self, v: Value) -> bool {
        matches!(
            self.b.func().value_def(v),
            Some(ir_lang::ValueDef::Param { block, .. }) if block == self.b.func().entry()
        )
    }

    /// Runs one task.
    fn step(&mut self, task: Task) -> Result<(), LowerError> {
        match task {
            Task::Expr(e) => {
                self.node = Some(NodeRef::Expr(e));
                self.tick()?;
                if self.cx.typed.coercion(e).is_some() {
                    self.tasks.push(Task::Coerce(e));
                }
                self.expr(e)
            }
            Task::Place(e) => {
                self.node = Some(NodeRef::Expr(e));
                self.tick()?;
                self.place(e)
            }
            Task::Stmt(s) => {
                self.node = Some(NodeRef::Stmt(s));
                self.tick()?;
                self.stmt(s)
            }
            Task::Coerce(e) => self.coerce(e),
            Task::PushUnit => {
                self.vals.push(Val::None);
                Ok(())
            }
            Task::Discard => self.pop_val().map(|_| ()),
            Task::Op(e) => self.op_task(e),
            Task::Cast(e) => self.cast_task(e),
            Task::Build(e) => self.build(e),
            Task::Repeat(e) => self.repeat(e),
            Task::Call(e) => self.call(e),
            Task::Read => {
                let p = self.pop_place()?;
                let v = self.read(p)?;
                self.vals.push(v);
                Ok(())
            }
            Task::PlaceField(e) => self.place_field(e),
            Task::PlaceIndex(e) => self.place_index(e),
            Task::PlaceOfVal => {
                let v = self.pop_val()?;
                let p = self.place_of_val(v)?;
                self.places.push(p);
                Ok(())
            }
            Task::Assign => {
                let v = self.pop_val()?;
                let p = self.pop_place()?;
                self.write(p, v)?;
                self.vals.push(v);
                Ok(())
            }
            Task::CompoundRead => {
                let p = *self.places.last().ok_or_else(|| internal(self.node))?;
                let v = self.read(p)?;
                self.vals.push(v);
                Ok(())
            }
            Task::Compound(e) => self.compound(e),
            Task::LetPlace(e) => self.let_place(e),
            Task::ConstExit => {
                if let Some(item) = self.consts.pop() {
                    let _ = self.const_set.remove(&item);
                }
                Ok(())
            }
            Task::BlockEnd(i) => self.block_end(i),
            Task::BlockDefers(i) => self.block_defers(i),
            Task::BlockFinish(i) => self.block_finish(i),
            Task::IfThen(i) => self.if_then(i),
            Task::IfElse(i) => self.if_else(i),
            Task::IfJoin(i) => self.if_join(i),
            Task::LoopBody(i) => self.loop_body(i),
            Task::LoopStep(i) => self.loop_step(i),
            Task::MatchScrut(e) => self.match_scrut(e),
            Task::MatchArm(i) => self.match_arm(i),
            Task::MatchArmEnd(i) => self.match_arm_end(i),
            Task::SwitchArm(i) => self.switch_arm(i),
            Task::SwitchArmEnd(i) => self.switch_arm_end(i),
            Task::Guard(i) => self.guard(i),
            Task::Break(e) => self.break_task(e),
            Task::Return => self.return_task(),
            Task::Throw => self.throw_task(),
            Task::TryBody(i) => self.try_body(i),
            Task::CatchArm(i) => self.catch_arm(i),
            Task::CatchArmEnd(i) => self.catch_arm_end(i),
            Task::TryEnd(i) => self.try_end(i),
            Task::TryResult(i) => self.try_result(i),
            Task::CleanupDispatch(i) => self.cleanup_dispatch(i),
            Task::LetInit(s) => self.let_init(s),
            Task::LetElseEnd(i) => self.let_else_end(i),
        }
    }
}

/// An instruction ir-lang refused, or an assumption the pre-scan should have
/// guaranteed that did not hold: a bug in irgen-lang.
pub(crate) fn internal(node: Option<NodeRef>) -> LowerError {
    LowerError::Internal { node }
}

impl From<BuildError> for LowerError {
    fn from(error: BuildError) -> Self {
        LowerError::Build { node: None, error }
    }
}
