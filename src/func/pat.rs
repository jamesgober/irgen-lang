//! Pattern tests: a sequential decision procedure over a subject, with an
//! explicit work stack (patterns nest arbitrarily deep). Each test branches
//! to a fail block or falls through to the next test; bindings happen on the
//! success path.

use alloc::{vec, vec::Vec};

use hir_lang::{BinderId, Def, ItemKind, Lit, Pat, PatId, Res};
use intern_lang::Lookup;
use ir_lang::{Block, CmpOp, StackSlot, Value};

use super::{FnCx, Local, internal};
use crate::{
    consteval,
    error::LowerError,
    types::{Repr, Scalar, Shape},
};

/// What a pattern is tested against.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Subject {
    Scalar(Value, Scalar),
    /// An aggregate at an address.
    Mem(Value, u32),
    /// An aggregate in a temporary slot (a binding may adopt it).
    Slot(StackSlot, u32),
    Unit,
}

#[derive(Clone, Copy, Debug)]
enum Fail {
    /// The caller's fail block (created on first use).
    Outer,
    Block(Block),
}

#[derive(Clone, Copy, Debug)]
enum PTask {
    Test(PatId, Subject, Fail),
    Bind(BinderId, Subject),
    Jump(Block),
    Enter(Block),
}

impl<L: Lookup> FnCx<'_, '_, '_, L> {
    /// Tests `root` against `subject`, binding its binders on success.
    /// Returns the fail block if the pattern can fail (`outer`, or one made
    /// for it); the caller decides what failing does.
    pub(super) fn test_pat(
        &mut self,
        root: PatId,
        subject: Subject,
        outer: Option<Block>,
    ) -> Result<Option<Block>, LowerError> {
        let mut outer = outer;
        let mut stack = vec![PTask::Test(root, subject, Fail::Outer)];
        while let Some(t) = stack.pop() {
            match t {
                PTask::Jump(b) => self.jump(b)?,
                PTask::Enter(b) => self.enter_sealed(b)?,
                PTask::Bind(binder, s) => self.bind(binder, s)?,
                PTask::Test(p, s, fail) => {
                    self.tick()?;
                    self.test_one(p, s, fail, &mut outer, &mut stack)?;
                }
            }
        }
        Ok(outer)
    }

    fn fail_block(&mut self, f: Fail, outer: &mut Option<Block>) -> Result<Block, LowerError> {
        match f {
            Fail::Block(b) => Ok(b),
            Fail::Outer => match *outer {
                Some(b) => Ok(b),
                None => {
                    let b = self.new_block()?;
                    *outer = Some(b);
                    Ok(b)
                }
            },
        }
    }

    fn test_one(
        &mut self,
        p: PatId,
        s: Subject,
        fail: Fail,
        outer: &mut Option<Block>,
        stack: &mut Vec<PTask>,
    ) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        match *hir.pat(p) {
            Pat::Wild => {}
            Pat::Bind { binder, sub, .. } => {
                stack.push(PTask::Bind(binder, s));
                if let Some(sub) = sub {
                    stack.push(PTask::Test(sub, s, fail));
                }
            }
            Pat::Ident { binder, path } => match hir.path(path).res {
                Res::Def(d) => self.def_test(d, s, fail, outer)?,
                _ => stack.push(PTask::Bind(binder, s)),
            },
            Pat::Path(path) => match hir.path(path).res {
                Res::Def(d) => self.def_test(d, s, fail, outer)?,
                _ => return Err(internal(self.node)),
            },
            Pat::Lit(lit) => {
                let Subject::Scalar(v, sc) = s else {
                    return Err(internal(self.node));
                };
                let bits = consteval::lit_bits(lit, sc).ok_or_else(|| internal(self.node))?;
                let c = self.const_bits(sc, bits)?;
                let cond = self.b.compare(CmpOp::Eq, v, c)?;
                let f = self.fail_block(fail, outer)?;
                self.branch_cont(cond, f)?;
            }
            Pat::Range { lo, hi, inclusive } => {
                let Subject::Scalar(v, sc) = s else {
                    return Err(internal(self.node));
                };
                if let Some(lo) = lo {
                    let bits = self.bound_bits(lo, sc)?;
                    let c = self.const_bits(sc, bits)?;
                    let cond = self.b.compare(CmpOp::Ge, v, c)?;
                    let f = self.fail_block(fail, outer)?;
                    self.branch_cont(cond, f)?;
                }
                if let Some(hi) = hi {
                    let bits = self.bound_bits(hi, sc)?;
                    let c = self.const_bits(sc, bits)?;
                    let op = if inclusive { CmpOp::Le } else { CmpOp::Lt };
                    let cond = self.b.compare(op, v, c)?;
                    let f = self.fail_block(fail, outer)?;
                    self.branch_cont(cond, f)?;
                }
            }
            Pat::Tuple { elems, rest } | Pat::Ctor { elems, rest, .. } => {
                let elems = hir.list(elems);
                if elems.is_empty() {
                    return Ok(());
                }
                let (base, agg) = self.subject_mem(s)?;
                let n = self.agg(agg)?.len();
                let m = elems.len() as u64;
                let mut subs = Vec::with_capacity(elems.len());
                for (k, &el) in elems.iter().enumerate() {
                    let k = k as u64;
                    let field = match rest {
                        Some(r) if k >= u64::from(r) => n.saturating_sub(m) + k,
                        _ => k,
                    };
                    let field = u32::try_from(field).map_err(|_| internal(self.node))?;
                    let fs = self.field_subject(base, agg, field)?;
                    subs.push(PTask::Test(el, fs, fail));
                }
                stack.extend(subs.into_iter().rev());
            }
            Pat::Record { fields, .. } => {
                let fields = hir.list(fields);
                if fields.is_empty() {
                    return Ok(());
                }
                let (base, agg) = self.subject_mem(s)?;
                let mut subs = Vec::with_capacity(fields.len());
                for fp in fields {
                    let (k, _) = self
                        .agg(agg)?
                        .field_index_by_name(fp.name.sym)
                        .ok_or_else(|| internal(self.node))?;
                    let fs = self.field_subject(base, agg, k)?;
                    subs.push(PTask::Test(fp.pat, fs, fail));
                }
                stack.extend(subs.into_iter().rev());
            }
            Pat::Or(alts) => {
                let alts = hir.list(alts);
                let Some((&last, firsts)) = alts.split_last() else {
                    // An empty or-pattern matches nothing.
                    let f = self.fail_block(fail, outer)?;
                    self.jump(f)?;
                    return self.go_dead();
                };
                let success = self.new_block()?;
                // Executed in order: alt 0 (failing to next 0), jump to
                // success, enter next 0, alt 1, ..., the last alt failing
                // to this pattern's own fail target, then enter success.
                let mut seq: Vec<PTask> = Vec::with_capacity(alts.len() * 3 + 1);
                for &alt in firsts {
                    let next = self.new_block()?;
                    seq.push(PTask::Test(alt, s, Fail::Block(next)));
                    seq.push(PTask::Jump(success));
                    seq.push(PTask::Enter(next));
                }
                seq.push(PTask::Test(last, s, fail));
                seq.push(PTask::Jump(success));
                seq.push(PTask::Enter(success));
                stack.extend(seq.into_iter().rev());
            }
            _ => return Err(internal(self.node)),
        }
        Ok(())
    }

    /// A pattern naming a constant (compare) or a unit record (always
    /// matches).
    fn def_test(
        &mut self,
        d: hir_lang::DefId,
        s: Subject,
        fail: Fail,
        outer: &mut Option<Block>,
    ) -> Result<(), LowerError> {
        let Def::Item(item) = d.def() else {
            return Err(internal(self.node));
        };
        match self.cx.hir.get_item(item).map(|i| i.kind) {
            Some(ItemKind::Const {
                value: Some(value), ..
            }) => {
                let Subject::Scalar(v, sc) = s else {
                    return Err(internal(self.node));
                };
                let bits = consteval::scalar_bits(self.cx, &self.t.types, self.budget, value, sc)?;
                let c = self.const_bits(sc, bits)?;
                let cond = self.b.compare(CmpOp::Eq, v, c)?;
                let f = self.fail_block(fail, outer)?;
                self.branch_cont(cond, f)
            }
            Some(ItemKind::Record(_)) => Ok(()),
            _ => Err(internal(self.node)),
        }
    }

    fn bound_bits(&mut self, bound: PatId, sc: Scalar) -> Result<u64, LowerError> {
        match *self.cx.hir.pat(bound) {
            Pat::Lit(lit) => consteval::lit_bits(lit, sc).ok_or_else(|| internal(self.node)),
            Pat::Path(path) => {
                let Res::Def(d) = self.cx.hir.path(path).res else {
                    return Err(internal(self.node));
                };
                let Def::Item(item) = d.def() else {
                    return Err(internal(self.node));
                };
                let Some(ItemKind::Const {
                    value: Some(value), ..
                }) = self.cx.hir.get_item(item).map(|i| i.kind)
                else {
                    return Err(internal(self.node));
                };
                consteval::scalar_bits(self.cx, &self.t.types, self.budget, value, sc)
            }
            _ => Err(internal(self.node)),
        }
    }

    fn subject_mem(&mut self, s: Subject) -> Result<(Value, u32), LowerError> {
        match s {
            Subject::Mem(a, agg) => Ok((a, agg)),
            Subject::Slot(slot, agg) => Ok((self.b.stack_addr(slot)?, agg)),
            _ => Err(internal(self.node)),
        }
    }

    /// Field `k` of the aggregate at `base`, as a subject (scalars loaded).
    fn field_subject(&mut self, base: Value, agg: u32, k: u32) -> Result<Subject, LowerError> {
        let (id, fr, is_array) = {
            let a = self.agg(agg)?;
            let fr = a.field(k).ok_or_else(|| internal(self.node))?;
            (a.id, fr, matches!(a.shape, Shape::Array { .. }))
        };
        let addr = if is_array {
            let idx = self.b.iconst(ir_lang::Type::U64, i128::from(k))?;
            let mt = self
                .t
                .types
                .elem_memtype(fr)
                .ok_or_else(|| internal(self.node))?;
            self.b.elem_addr(base, mt, idx)?
        } else {
            self.b.field_addr(base, id, k)?
        };
        Ok(match fr {
            Repr::Scalar(sc) => Subject::Scalar(self.load(sc, addr)?, sc),
            Repr::Agg(j) => Subject::Mem(addr, j),
            Repr::Unit | Repr::Never => Subject::Unit,
        })
    }

    /// Binds a binder to a subject (a copy for aggregates). Or-pattern
    /// alternatives bind the same binder: the same variable or slot is reused.
    pub(super) fn bind(&mut self, binder: BinderId, s: Subject) -> Result<(), LowerError> {
        let existing = self.get_local(binder);
        let local = match s {
            Subject::Scalar(v, sc) => {
                let var = match existing {
                    Some(Local::Var(var, _)) => var,
                    _ => self.b.declare_var(sc.ir())?,
                };
                self.b.def_var(var, v)?;
                Local::Var(var, sc)
            }
            Subject::Mem(_, agg) | Subject::Slot(_, agg) => {
                let slot = match existing {
                    Some(Local::Slot(slot, _)) => slot,
                    _ => self.new_slot(agg)?,
                };
                let (src, _) = self.subject_mem(s)?;
                let dst = self.b.stack_addr(slot)?;
                self.copy(dst, src, agg)?;
                Local::Slot(slot, agg)
            }
            Subject::Unit => Local::Unit,
        };
        self.set_local(binder, local);
        Ok(())
    }

    /// Zero-initializes the binders of a `let` without an initializer.
    pub(super) fn bind_zero(&mut self, pat: PatId) -> Result<(), LowerError> {
        let Pat::Bind { binder, .. } = *self.cx.hir.pat(pat) else {
            return Ok(());
        };
        let t = self
            .cx
            .typed
            .binder(binder)
            .ok_or(LowerError::MissingType {
                node: hir_lang::NodeRef::Pat(pat),
            })?;
        let r = self.t.types.known(t).ok_or(LowerError::MissingType {
            node: hir_lang::NodeRef::Pat(pat),
        })?;
        let local = match r {
            Repr::Scalar(sc) => {
                let var = self.b.declare_var(sc.ir())?;
                let z = self.zero(sc.ir())?;
                self.b.def_var(var, z)?;
                Local::Var(var, sc)
            }
            Repr::Agg(agg) => {
                let slot = self.new_slot(agg)?;
                let (size, align) = {
                    let a = self.agg(agg)?;
                    (a.size, a.align)
                };
                if size > 0 {
                    let dst = self.b.stack_addr(slot)?;
                    let byte = self.b.iconst(ir_lang::Type::U8, 0)?;
                    let len = self.b.iconst(ir_lang::Type::U64, i128::from(size))?;
                    let _ = self
                        .b
                        .memset(dst, byte, len, ir_lang::MemFlags::new(align.max(1)))?;
                }
                Local::Slot(slot, agg)
            }
            Repr::Unit | Repr::Never => Local::Unit,
        };
        self.set_local(binder, local);
        Ok(())
    }

    /// Whether a match arm's pattern is a literal (or literals) a `switch`
    /// can test, with the case values for subject scalar `sc`.
    pub(super) fn switch_cases(&self, p: PatId, sc: Scalar, out: &mut Vec<u64>) -> bool {
        let hir = self.cx.hir;
        let lit_case = |lit: Lit, out: &mut Vec<u64>| -> bool {
            match lit {
                Lit::Int(_) | Lit::Char(_) => match consteval::lit_bits(lit, sc) {
                    Some(bits) => {
                        out.push(bits);
                        true
                    }
                    None => false,
                },
                _ => false,
            }
        };
        match *hir.pat(p) {
            Pat::Lit(lit) => lit_case(lit, out),
            Pat::Or(alts) => {
                for &a in hir.list(alts) {
                    match *hir.pat(a) {
                        Pat::Lit(lit) if lit_case(lit, out) => {}
                        _ => return false,
                    }
                }
                true
            }
            _ => false,
        }
    }
}
