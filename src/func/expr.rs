//! Expressions and statements: dispatch, aggregates, calls, coercions.

use alloc::vec::Vec;

use hir_lang::{Def, Expr, ExprId, ItemKind, Res, Stmt, StmtId};
use intern_lang::Lookup;
use ir_lang::{BlockArg, BlockCall, Call, CmpOp, ConvOp, MemType, Overflow, Policy, Type, Value};

use super::{FnCx, Task, Val, flow, internal};
use crate::{
    error::LowerError,
    types::{Repr, Scalar, Shape},
};

impl<L: Lookup> FnCx<'_, '_, '_, L> {
    /// `Task::Expr(e)`: pushes `e`'s value (through its children's tasks).
    pub(super) fn expr(&mut self, e: ExprId) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        match *hir.expr(e) {
            Expr::Lit(lit) => {
                let v = match self.repr_of(e)? {
                    Repr::Scalar(sc) => {
                        let bits = crate::consteval::lit_bits(lit, sc)
                            .ok_or_else(|| internal(self.node))?;
                        Val::Scalar(self.const_bits(sc, bits)?, sc)
                    }
                    _ => Val::None,
                };
                self.vals.push(v);
            }
            Expr::Path(p) => self.value_path(e, p)?,
            Expr::Tuple(elems) | Expr::Array(elems) => {
                self.tasks.push(Task::Build(e));
                for &el in hir.list(elems).iter().rev() {
                    self.tasks.push(Task::Expr(el));
                }
            }
            Expr::Repeat { elem, .. } => {
                self.tasks.push(Task::Repeat(e));
                self.tasks.push(Task::Expr(elem));
            }
            Expr::Record { fields, base, .. } => {
                self.tasks.push(Task::Build(e));
                if let Some(b) = base {
                    self.tasks.push(Task::Expr(b));
                }
                for fi in hir.list(fields).iter().rev() {
                    self.tasks.push(Task::Expr(fi.value));
                }
            }
            Expr::Call { args, .. } => {
                self.tasks.push(Task::Call(e));
                for a in hir.list(args).iter().rev() {
                    self.tasks.push(Task::Expr(a.value));
                }
            }
            Expr::Field { .. } | Expr::Index { .. } => {
                self.tasks.push(Task::Read);
                self.tasks.push(Task::Place(e));
            }
            Expr::Op { args, .. } => {
                self.tasks.push(Task::Op(e));
                for &a in hir.list(args).iter().rev() {
                    self.tasks.push(Task::Expr(a));
                }
            }
            Expr::Cast { expr, .. } => {
                self.tasks.push(Task::Cast(e));
                self.tasks.push(Task::Expr(expr));
            }
            Expr::Assign { target, op, value } => {
                if op.is_some() {
                    self.tasks.push(Task::Compound(e));
                    self.tasks.push(Task::Expr(value));
                    self.tasks.push(Task::CompoundRead);
                } else {
                    self.tasks.push(Task::Assign);
                    self.tasks.push(Task::Expr(value));
                }
                self.tasks.push(Task::Place(target));
            }
            Expr::LetPlace { place, body, .. } => {
                self.tasks.push(Task::Expr(body));
                self.tasks.push(Task::LetPlace(e));
                self.tasks.push(Task::Place(place));
            }
            Expr::Block(b) => self.block_start(e, b)?,
            Expr::If { cond, .. } => {
                let i = self.if_start(e)?;
                self.tasks.push(Task::IfThen(i));
                self.tasks.push(Task::Expr(cond));
            }
            Expr::Match { scrutinee, .. } => {
                self.tasks.push(Task::MatchScrut(e));
                self.tasks.push(Task::Expr(scrutinee));
            }
            Expr::Loop { .. } => self.loop_start(e)?,
            Expr::Break { value, .. } => {
                self.tasks.push(Task::Break(e));
                self.push_opt(value);
            }
            Expr::Continue { label } => self.continue_now(label)?,
            Expr::Return(value) => {
                self.tasks.push(Task::Return);
                self.push_opt(value);
            }
            Expr::Throw(value) => {
                self.tasks.push(Task::Throw);
                self.tasks.push(Task::Expr(value));
            }
            Expr::Try { .. } => self.try_start(e)?,
            _ => return Err(internal(self.node)),
        }
        Ok(())
    }

    fn push_opt(&mut self, e: Option<ExprId>) {
        match e {
            Some(e) => self.tasks.push(Task::Expr(e)),
            None => self.tasks.push(Task::PushUnit),
        }
    }

    /// `Task::Stmt(s)`.
    pub(super) fn stmt(&mut self, s: StmtId) -> Result<(), LowerError> {
        match *self.cx.hir.stmt(s) {
            Stmt::Let { pat, init, .. } => match init {
                Some(init) => {
                    self.tasks.push(Task::LetInit(s));
                    self.tasks.push(Task::Expr(init));
                }
                None => self.bind_zero(pat)?,
            },
            Stmt::Expr(e) => {
                self.tasks.push(Task::Discard);
                self.tasks.push(Task::Expr(e));
            }
            Stmt::Item(_) => {}
            Stmt::Defer(body) => {
                let _ = self.open_cleanup(body)?;
            }
            _ => return Err(internal(self.node)),
        }
        Ok(())
    }

    /// `Task::LetInit(s)`: binds the initializer's value; a refutable
    /// pattern goes to the `else` block, or raises `NoMatch` without one.
    pub(super) fn let_init(&mut self, s: StmtId) -> Result<(), LowerError> {
        let Stmt::Let { pat, else_, .. } = *self.cx.hir.stmt(s) else {
            return Err(internal(self.node));
        };
        let v = self.pop_val()?;
        let subject = match v {
            Val::Scalar(x, sc) => super::Subject::Scalar(x, sc),
            Val::Mem(slot, agg) => super::Subject::Slot(slot, agg),
            Val::None => super::Subject::Unit,
        };
        let fail = self.bind_root(pat, subject, None)?;
        let Some(fail) = fail else {
            return Ok(());
        };
        let saved = self.save()?;
        self.enter_sealed(fail)?;
        match else_ {
            Some(other) => {
                let i = u32::try_from(self.st.lets.len()).map_err(|_| internal(self.node))?;
                self.st.lets.push(flow::LetSt { saved });
                self.tasks.push(Task::LetElseEnd(i));
                self.tasks.push(Task::Expr(other));
            }
            None => {
                self.raise_const(crate::rt::E_NO_MATCH)?;
                self.restore(saved)?;
            }
        }
        Ok(())
    }

    /// `Task::LetElseEnd(i)`: the `else` block diverges (typeck checked it);
    /// lowering continues on the success path.
    pub(super) fn let_else_end(&mut self, i: u32) -> Result<(), LowerError> {
        let _ = self.pop_val()?;
        if !self.dead {
            self.b.unreachable()?;
        }
        let saved = self
            .st
            .lets
            .get(i as usize)
            .map(|l| l.saved)
            .ok_or_else(|| internal(self.node))?;
        self.restore(saved)
    }

    /// `Task::Coerce(e)`: numeric widening (the only coercion in the subset).
    pub(super) fn coerce(&mut self, e: ExprId) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        let target = self.final_repr(e)?;
        let out = match (v, target) {
            (Val::Scalar(x, from), Repr::Scalar(to)) => Val::Scalar(self.widen(x, from, to)?, to),
            (other, _) => other,
        };
        self.vals.push(out);
        Ok(())
    }

    /// A lossless widening conversion.
    pub(super) fn widen(
        &mut self,
        x: Value,
        from: Scalar,
        to: Scalar,
    ) -> Result<Value, LowerError> {
        let (ft, tt) = (from.ir(), to.ir());
        if ft == tt {
            return Ok(x);
        }
        Ok(match (from, to) {
            (Scalar::Int(_) | Scalar::Char, Scalar::Int(_)) => self.int_resize(x, ft, tt)?,
            (Scalar::Int(_), Scalar::Float(_)) => {
                self.b.convert(ConvOp::IntToFloat, Policy::NONE, x, tt)?
            }
            (Scalar::Float(_), Scalar::Float(_)) => {
                self.b.convert(ConvOp::FloatCast, Policy::NONE, x, tt)?
            }
            _ => return Err(internal(self.node)),
        })
    }

    /// Resizes an integer by bits: sign- or zero-extension by the source's
    /// signedness, truncation, then a same-width reinterpretation.
    pub(super) fn int_resize(
        &mut self,
        x: Value,
        from: Type,
        to: Type,
    ) -> Result<Value, LowerError> {
        if from == to {
            return Ok(x);
        }
        let (fb, tb) = (from.int_bits().unwrap_or(64), to.int_bits().unwrap_or(64));
        let signed = from.is_signed_int();
        let mid = Type::int(tb, signed).ok_or_else(|| internal(self.node))?;
        let y = match tb.cmp(&fb) {
            core::cmp::Ordering::Greater => {
                let op = if signed { ConvOp::Sext } else { ConvOp::Zext };
                self.b.convert(op, Policy::NONE, x, mid)?
            }
            core::cmp::Ordering::Less => self.b.convert(ConvOp::Narrow, Policy::NONE, x, mid)?,
            core::cmp::Ordering::Equal => x,
        };
        if mid == to {
            Ok(y)
        } else {
            Ok(self.b.convert(ConvOp::Bitcast, Policy::NONE, y, to)?)
        }
    }

    /// `Task::Build(e)`: a tuple, array, or record from its parts.
    pub(super) fn build(&mut self, e: ExprId) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        let repr = self.repr_of(e)?;
        let (n, base) = match *hir.expr(e) {
            Expr::Tuple(l) | Expr::Array(l) => (l.len(), None),
            Expr::Record { fields, base, .. } => (fields.len(), base),
            Expr::Call { args, .. } => (args.len(), None),
            _ => return Err(internal(self.node)),
        };
        let base_val = if base.is_some() {
            Some(self.pop_val()?)
        } else {
            None
        };
        let at = self
            .vals
            .len()
            .checked_sub(n)
            .ok_or_else(|| internal(self.node))?;
        let parts: Vec<Val> = self.vals.drain(at..).collect();
        let Repr::Agg(agg) = repr else {
            // `()`: nothing to build.
            self.vals.push(Val::None);
            return Ok(());
        };
        let slot = self.new_slot(agg)?;
        let dst = self.b.stack_addr(slot)?;
        if let Some(Val::Mem(src_slot, _)) = base_val {
            let src = self.b.stack_addr(src_slot)?;
            self.copy(dst, src, agg)?;
        }
        let fields = match *hir.expr(e) {
            Expr::Record { fields, .. } => Some(fields),
            _ => None,
        };
        for (k, v) in parts.into_iter().enumerate() {
            let k = match fields {
                Some(fl) => {
                    let fi = hir.list(fl).get(k).ok_or_else(|| internal(self.node))?;
                    self.agg(agg)?
                        .field_index_by_name(fi.name.sym)
                        .ok_or_else(|| internal(self.node))?
                        .0
                }
                None => u32::try_from(k).map_err(|_| internal(self.node))?,
            };
            self.store_field(dst, agg, k, v)?;
        }
        self.vals.push(Val::Mem(slot, agg));
        Ok(())
    }

    /// Stores `v` into field (or element) `k` of the aggregate at `base`.
    fn store_field(&mut self, base: Value, agg: u32, k: u32, v: Val) -> Result<(), LowerError> {
        let (id, fr, is_array) = {
            let a = self.agg(agg)?;
            let fr = a.field(k).ok_or_else(|| internal(self.node))?;
            (a.id, fr, matches!(a.shape, Shape::Array { .. }))
        };
        if !fr.has_value() {
            return Ok(());
        }
        let addr = if is_array {
            let idx = self.b.iconst(Type::U64, i128::from(k))?;
            let mt = self.elem_mt(fr)?;
            self.b.elem_addr(base, mt, idx)?
        } else {
            self.b.field_addr(base, id, k)?
        };
        self.write_mem(addr, fr, v)
    }

    fn elem_mt(&self, r: Repr) -> Result<MemType, LowerError> {
        self.t
            .types
            .elem_memtype(r)
            .ok_or_else(|| internal(self.node))
    }

    /// `Task::Repeat(e)`: `[x; N]`, by a counted loop (the element evaluated
    /// once).
    pub(super) fn repeat(&mut self, e: ExprId) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        let Repr::Agg(agg) = self.repr_of(e)? else {
            self.vals.push(Val::None);
            return Ok(());
        };
        let (elem, len) = match self.agg(agg)?.shape {
            Shape::Array { elem, len } => (elem, len),
            Shape::Struct { .. } => return Err(internal(self.node)),
        };
        let slot = self.new_slot(agg)?;
        if len > 0 && elem.has_value() {
            let mt = self.elem_mt(elem)?;
            if len <= 4 {
                for k in 0..len {
                    let base = self.b.stack_addr(slot)?;
                    let idx = self.b.iconst(Type::U64, i128::from(k))?;
                    let a = self.b.elem_addr(base, mt, idx)?;
                    self.write_mem(a, elem, v)?;
                }
            } else {
                // i = 0; while i < len { a[i] = v; i += 1 }
                let i = self.b.declare_var(Type::U64)?;
                let zero = self.b.iconst(Type::U64, 0)?;
                self.b.def_var(i, zero)?;
                let head = self.new_block()?;
                let body = self.new_block()?;
                let exit = self.new_block()?;
                self.jump(head)?;
                self.enter(head)?;
                let iv = if self.dead { zero } else { self.b.use_var(i)? };
                let n = self.b.iconst(Type::U64, i128::from(len))?;
                let more = self.b.compare(CmpOp::Lt, iv, n)?;
                self.branch(more, body, exit)?;
                self.enter_sealed(body)?;
                let base = self.b.stack_addr(slot)?;
                let a = self.b.elem_addr(base, mt, iv)?;
                self.write_mem(a, elem, v)?;
                let one = self.b.iconst(Type::U64, 1)?;
                let next = self.b.add(iv, one, Overflow::Wrap)?;
                self.b.def_var(i, next)?;
                self.jump(head)?;
                self.b.seal_block(head)?;
                self.enter_sealed(exit)?;
            }
        }
        self.vals.push(Val::Mem(slot, agg));
        Ok(())
    }

    /// `Task::Call(e)`: a direct call of a function item or a host function,
    /// or a tuple record's constructor.
    pub(super) fn call(&mut self, e: ExprId) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        let Expr::Call { callee, args } = *hir.expr(e) else {
            return Err(internal(self.node));
        };
        let Expr::Path(p) = *hir.expr(callee) else {
            return Err(internal(self.node));
        };
        let (func, params, ret) = match hir.path(p).res {
            Res::Def(d) => {
                let Def::Item(item) = d.def() else {
                    return Err(internal(self.node));
                };
                if let Some(f) = self.t.function(item) {
                    let params: Vec<Repr> = f.params.iter().map(|p| p.1).collect();
                    (f.func, params, f.ret)
                } else if let Some(ItemKind::Record(_)) = hir.get_item(item).map(|i| i.kind) {
                    return self.build(e);
                } else {
                    return Err(internal(self.node));
                }
            }
            Res::Extern(sym) => {
                let x = self
                    .t
                    .externs
                    .get(&sym)
                    .ok_or_else(|| internal(self.node))?;
                (x.func, x.params.clone(), x.ret)
            }
            _ => return Err(internal(self.node)),
        };
        let n = args.len();
        let at = self
            .vals
            .len()
            .checked_sub(n)
            .ok_or_else(|| internal(self.node))?;
        let vals: Vec<Val> = self.vals.drain(at..).collect();
        let mut ir_args: Vec<Value> = Vec::with_capacity(n + 1);
        let sret = match ret {
            Repr::Agg(agg) => {
                let slot = self.new_slot(agg)?;
                ir_args.push(self.b.stack_addr(slot)?);
                Some((slot, agg))
            }
            _ => None,
        };
        for (v, r) in vals.into_iter().zip(params) {
            match (r, v) {
                (Repr::Scalar(_), Val::Scalar(x, _)) => ir_args.push(x),
                (Repr::Scalar(sc), _) => ir_args.push(self.zero(sc.ir())?),
                (Repr::Agg(_), Val::Mem(slot, _)) => ir_args.push(self.b.stack_addr(slot)?),
                (Repr::Agg(agg), _) => {
                    let slot = self.new_slot(agg)?;
                    ir_args.push(self.b.stack_addr(slot)?);
                }
                (Repr::Unit | Repr::Never, _) => {}
            }
        }
        let result = self.emit_call(func, &ir_args, ret)?;
        let v = match (ret, result, sret) {
            (Repr::Scalar(sc), Some(x), _) => Val::Scalar(x, sc),
            (_, _, Some((slot, agg))) => Val::Mem(slot, agg),
            _ => Val::None,
        };
        if ret == Repr::Never && !self.dead {
            self.b.unreachable()?;
            self.go_dead()?;
        }
        self.vals.push(v);
        Ok(())
    }

    /// Emits a call: an `invoke` inside a `try` or cleanup region (its
    /// exception goes to the region's landing pad), a plain `call` outside
    /// (an exception propagates to the caller).
    pub(super) fn emit_call(
        &mut self,
        func: ir_lang::FuncId,
        args: &[Value],
        ret: Repr,
    ) -> Result<Option<Value>, LowerError> {
        let ret_ty = match ret {
            Repr::Scalar(sc) => Some(sc.ir()),
            _ => None,
        };
        if self.dead {
            return match ret_ty {
                Some(t) => Ok(Some(self.zero(t)?)),
                None => Ok(None),
            };
        }
        match self.exception_region() {
            Some(level) => {
                let pad = self.pad(level)?;
                let params: Vec<Type> = ret_ty.into_iter().collect();
                let cont = self.new_block_with(&params)?;
                let normal: Vec<BlockArg> = if ret_ty.is_some() {
                    alloc::vec![BlockArg::Result(0)]
                } else {
                    Vec::new()
                };
                self.b.invoke(
                    Call::direct(func, args),
                    BlockCall::with_args(cont, &normal),
                    BlockCall::with_args(pad, &[BlockArg::Exception]),
                )?;
                self.mark(cont);
                self.mark(pad);
                self.enter_sealed(cont)?;
                match ret_ty {
                    Some(_) => Ok(Some(self.b.block_param(cont, 0)?)),
                    None => Ok(None),
                }
            }
            None => {
                let inst = self.b.call(Call::direct(func, args))?;
                Ok(self.b.results(inst).first())
            }
        }
    }
}
