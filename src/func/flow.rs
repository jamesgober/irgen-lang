//! Control flow: blocks, `if`, loops, `match`, jumps, and exceptions.
//!
//! Jumps out of a `finally` or `defer` region are routed through the
//! region's body: the jump records which exit it wants in the region's
//! `dest` variable and enters the body; after the body, a `switch` on `dest`
//! continues each exit (possibly into the next enclosing cleanup). Each
//! cleanup body is lowered once, so nested cleanups stay linear in size.
//!
//! Exceptions: inside a `try` or cleanup region, calls are `invoke`s whose
//! landing pad forwards the payload to the region; `throw` and run-time
//! errors jump there directly. Outside every region, `resume` unwinds to
//! the caller.

use alloc::{boxed::Box, collections::BTreeMap, vec::Vec};

use hir_lang::{Arm, BinderId, Block as HBlock, Expr, ExprId, List, Pat};
use intern_lang::Lookup;
use ir_lang::{Block, BlockCall, Call, CmpOp, ConvOp, Policy, SwitchCase, Type, Value};

use super::{Cleanup, Exit, FnCx, Region, ResSlot, Ret, Subject, Task, Val, internal};
use crate::{
    error::LowerError,
    options::ErrorMode,
    rt,
    types::{Repr, Scalar},
};

pub(crate) struct IfSt {
    then: ExprId,
    other: Option<ExprId>,
    res: ResSlot,
    /// The `else` block and the join, made once the condition is lowered.
    blocks: Option<(Block, Block)>,
}

pub(crate) struct LoopSt {
    region: usize,
    header: Block,
    step: Option<(Block, ExprId)>,
    brk: Block,
    res: ResSlot,
}

pub(crate) struct MatchSt {
    arms: List<Arm>,
    k: u32,
    subject: Subject,
    res: ResSlot,
    exit: Block,
    next: Option<Block>,
    /// For a run of literal arms lowered as one `switch`: the arms' body
    /// blocks, the first arm of the run, and the default block.
    run: Vec<Block>,
    run_start: u32,
    run_pos: u32,
}

pub(crate) struct TrySt {
    catches: List<Arm>,
    k: u32,
    res: ResSlot,
    exit: Block,
    /// Region index of the `finally` cleanup, if any.
    cleanup: Option<usize>,
    /// The caught exception's address, tag, and bits.
    exn: Option<(Value, Value, Value)>,
    next: Option<Block>,
}

pub(crate) struct BlockSt {
    base: usize,
    label: bool,
    res: ResSlot,
    /// The tail's value while defers run (a scalar is moved to `tmp`).
    val: Val,
    tmp: ResSlot,
}

pub(crate) struct LetSt {
    pub(super) saved: (Block, bool),
}

/// The last (innermost) index in the sorted `stack` below `level`.
fn innermost_below(stack: &[usize], level: usize) -> Option<usize> {
    let n = stack.partition_point(|&x| x < level);
    n.checked_sub(1).and_then(|k| stack.get(k).copied())
}

fn idx<T>(v: &[T]) -> Result<u32, LowerError> {
    u32::try_from(v.len()).map_err(|_| internal(None))
}

impl<L: Lookup> FnCx<'_, '_, '_, L> {
    // --------------------------------------------------------------- blocks

    pub(super) fn block_start(&mut self, e: ExprId, b: HBlock) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        let mut res = ResSlot::None;
        if let Some(label) = b.label {
            let r = self.repr_of(e)?;
            res = self.res_for(r)?;
            let brk = self.new_block()?;
            let _ = self.push_region(Region::Label { label, brk, res });
        }
        let i = idx(&self.st.blocks)?;
        self.st.blocks.push(BlockSt {
            base: self.regions.len(),
            label: b.label.is_some(),
            res,
            val: Val::None,
            tmp: ResSlot::None,
        });
        self.tasks.push(Task::BlockEnd(i));
        match b.tail {
            Some(t) => self.tasks.push(Task::Expr(t)),
            None => self.tasks.push(Task::PushUnit),
        }
        for &s in hir.list(b.stmts).iter().rev() {
            self.tasks.push(Task::Stmt(s));
        }
        Ok(())
    }

    pub(super) fn block_end(&mut self, i: u32) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        let (base, label, res) = {
            let st = self
                .st
                .blocks
                .get(i as usize)
                .ok_or_else(|| internal(self.node))?;
            (st.base, st.label, st.res)
        };
        let has_defers = self.regions.len() > base;
        let mut tmp = ResSlot::None;
        if label {
            if !self.dead {
                self.store_res(res, v)?;
            }
        } else if has_defers {
            if let Val::Scalar(x, sc) = v {
                let var = self.b.declare_var(sc.ir())?;
                self.b.def_var(var, x)?;
                tmp = ResSlot::Var(var, sc);
            }
        }
        if let Some(st) = self.st.blocks.get_mut(i as usize) {
            st.val = v;
            st.tmp = tmp;
        }
        self.tasks.push(Task::BlockDefers(i));
        Ok(())
    }

    pub(super) fn block_defers(&mut self, i: u32) -> Result<(), LowerError> {
        let base = self
            .st
            .blocks
            .get(i as usize)
            .map(|s| s.base)
            .ok_or_else(|| internal(self.node))?;
        if self.regions.len() > base {
            // Close the innermost defer, then come back for the next.
            self.tasks.push(Task::BlockDefers(i));
            let c = self.regions.len() - 1;
            if !self.dead {
                self.fallthrough(c)?;
            }
            return self.close_cleanup();
        }
        self.tasks.push(Task::BlockFinish(i));
        Ok(())
    }

    pub(super) fn block_finish(&mut self, i: u32) -> Result<(), LowerError> {
        let (label, res, val, tmp) = {
            let st = self
                .st
                .blocks
                .get(i as usize)
                .ok_or_else(|| internal(self.node))?;
            (st.label, st.res, st.val, st.tmp)
        };
        let v = if label {
            let Some(Region::Label { brk, .. }) = self.pop_region() else {
                return Err(internal(self.node));
            };
            self.jump(brk)?;
            self.enter_sealed(brk)?;
            self.load_res(res)?
        } else {
            match tmp {
                ResSlot::None => val,
                other => self.load_res(other)?,
            }
        };
        self.vals.push(v);
        Ok(())
    }

    // ------------------------------------------------------------------- if

    pub(super) fn if_start(&mut self, e: ExprId) -> Result<u32, LowerError> {
        let Expr::If { then, else_, .. } = *self.cx.hir.expr(e) else {
            return Err(internal(self.node));
        };
        let res = match else_ {
            Some(_) => {
                let r = self.repr_of(e)?;
                self.res_for(r)?
            }
            None => ResSlot::None,
        };
        let i = idx(&self.st.ifs)?;
        self.st.ifs.push(IfSt {
            then,
            other: else_,
            res,
            blocks: None,
        });
        Ok(i)
    }

    /// After the condition: branch, then lower `then`.
    pub(super) fn if_then(&mut self, i: u32) -> Result<(), LowerError> {
        let c = self.pop_val()?;
        let then_b = self.new_block()?;
        let else_b = self.new_block()?;
        let join = self.new_block()?;
        match c {
            Val::Scalar(c, _) => self.branch(c, then_b, else_b)?,
            _ => self.jump(then_b)?,
        }
        let then = {
            let st = self
                .st
                .ifs
                .get_mut(i as usize)
                .ok_or_else(|| internal(None))?;
            st.blocks = Some((else_b, join));
            st.then
        };
        self.enter_sealed(then_b)?;
        self.tasks.push(Task::IfElse(i));
        self.tasks.push(Task::Expr(then));
        Ok(())
    }

    pub(super) fn if_else(&mut self, i: u32) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        let (res, (else_b, join), other) = {
            let st = self
                .st
                .ifs
                .get(i as usize)
                .ok_or_else(|| internal(self.node))?;
            (st.res, st.blocks.ok_or_else(|| internal(None))?, st.other)
        };
        if !self.dead {
            self.store_res(res, v)?;
        }
        self.jump(join)?;
        self.enter_sealed(else_b)?;
        self.tasks.push(Task::IfJoin(i));
        match other {
            Some(o) => self.tasks.push(Task::Expr(o)),
            None => self.tasks.push(Task::PushUnit),
        }
        Ok(())
    }

    pub(super) fn if_join(&mut self, i: u32) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        let (res, join) = {
            let st = self
                .st
                .ifs
                .get(i as usize)
                .ok_or_else(|| internal(self.node))?;
            (st.res, st.blocks.ok_or_else(|| internal(None))?.1)
        };
        if !self.dead {
            self.store_res(res, v)?;
        }
        self.jump(join)?;
        self.enter_sealed(join)?;
        let out = self.load_res(res)?;
        self.vals.push(out);
        Ok(())
    }

    // ---------------------------------------------------------------- loops

    pub(super) fn loop_start(&mut self, e: ExprId) -> Result<(), LowerError> {
        let Expr::Loop { label, body, step } = *self.cx.hir.expr(e) else {
            return Err(internal(self.node));
        };
        let r = self.repr_of(e)?;
        let res = self.res_for(r)?;
        let header = self.new_block()?;
        let step_b = match step {
            Some(s) => Some((self.new_block()?, s)),
            None => None,
        };
        let brk = self.new_block()?;
        self.jump(header)?;
        self.enter(header)?;
        let cont = step_b.map_or(header, |(b, _)| b);
        let region = self.push_region(Region::Loop {
            label,
            cont,
            brk,
            res,
        });
        let i = idx(&self.st.loops)?;
        self.st.loops.push(LoopSt {
            region,
            header,
            step: step_b,
            brk,
            res,
        });
        self.tasks.push(Task::LoopBody(i));
        self.tasks.push(Task::Expr(body));
        Ok(())
    }

    pub(super) fn loop_body(&mut self, i: u32) -> Result<(), LowerError> {
        let _ = self.pop_val()?;
        let (header, step) = {
            let st = self
                .st
                .loops
                .get(i as usize)
                .ok_or_else(|| internal(self.node))?;
            (st.header, st.step)
        };
        match step {
            Some((sb, s)) => {
                self.jump(sb)?;
                self.enter_sealed(sb)?;
                self.tasks.push(Task::LoopStep(i));
                self.tasks.push(Task::Expr(s));
                Ok(())
            }
            None => {
                self.jump(header)?;
                self.loop_finish(i)
            }
        }
    }

    pub(super) fn loop_step(&mut self, i: u32) -> Result<(), LowerError> {
        let _ = self.pop_val()?;
        let header = self
            .st
            .loops
            .get(i as usize)
            .map(|s| s.header)
            .ok_or_else(|| internal(self.node))?;
        self.jump(header)?;
        self.loop_finish(i)
    }

    fn loop_finish(&mut self, i: u32) -> Result<(), LowerError> {
        let (region, header, brk, res) = {
            let st = self
                .st
                .loops
                .get(i as usize)
                .ok_or_else(|| internal(self.node))?;
            (st.region, st.header, st.brk, st.res)
        };
        self.b.seal_block(header)?;
        if self.regions.len() != region + 1 {
            return Err(internal(self.node));
        }
        let _ = self.pop_region();
        self.enter_sealed(brk)?;
        let v = self.load_res(res)?;
        self.vals.push(v);
        Ok(())
    }

    // ---------------------------------------------------------------- match

    pub(super) fn match_scrut(&mut self, e: ExprId) -> Result<(), LowerError> {
        let Expr::Match { arms, .. } = *self.cx.hir.expr(e) else {
            return Err(internal(self.node));
        };
        let v = self.pop_val()?;
        let subject = match v {
            Val::Scalar(x, sc) => Subject::Scalar(x, sc),
            Val::Mem(slot, agg) => Subject::Mem(self.b.stack_addr(slot)?, agg),
            Val::None => Subject::Unit,
        };
        let r = self.repr_of(e)?;
        let res = self.res_for(r)?;
        let exit = self.new_block()?;
        let i = idx(&self.st.matches)?;
        self.st.matches.push(MatchSt {
            arms,
            k: 0,
            subject,
            res,
            exit,
            next: None,
            run: Vec::new(),
            run_start: 0,
            run_pos: 0,
        });
        self.tasks.push(Task::MatchArm(i));
        Ok(())
    }

    fn match_st(&self, i: u32) -> Result<&MatchSt, LowerError> {
        self.st
            .matches
            .get(i as usize)
            .ok_or_else(|| internal(self.node))
    }

    pub(super) fn match_arm(&mut self, i: u32) -> Result<(), LowerError> {
        let (arms, k, subject, res, exit) = {
            let st = self.match_st(i)?;
            (st.arms, st.k, st.subject, st.res, st.exit)
        };
        let arms_list = self.cx.hir.list(arms);
        let Some(arm) = arms_list.get(k as usize).copied() else {
            // No arm matched.
            self.raise_const(rt::E_NO_MATCH)?;
            self.enter_sealed(exit)?;
            let v = self.load_res(res)?;
            self.vals.push(v);
            return Ok(());
        };
        if let Subject::Scalar(v, sc) = subject {
            if sc.is_int_like() && self.try_switch(i, v, sc, arms_list, k)? {
                return Ok(());
            }
        }
        let next = self.new_block()?;
        if let Some(st) = self.st.matches.get_mut(i as usize) {
            st.next = Some(next);
        }
        let _ = self.test_pat(arm.pat, subject, Some(next))?;
        self.tasks.push(Task::MatchArmEnd(i));
        self.tasks.push(Task::Expr(arm.body));
        if let Some(g) = arm.guard {
            self.tasks.push(Task::Guard(next.as_u32()));
            self.tasks.push(Task::Expr(g));
        }
        Ok(())
    }

    /// A guard: continue if true, else to its fail block.
    pub(super) fn guard(&mut self, fail: u32) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        let fail = Block::from_u32(fail);
        if let Val::Scalar(c, _) = v {
            self.branch_cont(c, fail)?;
        }
        Ok(())
    }

    pub(super) fn match_arm_end(&mut self, i: u32) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        let (res, exit, next) = {
            let st = self.match_st(i)?;
            (st.res, st.exit, st.next)
        };
        if !self.dead {
            self.store_res(res, v)?;
        }
        self.jump(exit)?;
        let next = next.ok_or_else(|| internal(self.node))?;
        self.enter_sealed(next)?;
        if let Some(st) = self.st.matches.get_mut(i as usize) {
            st.k += 1;
        }
        self.tasks.push(Task::MatchArm(i));
        Ok(())
    }

    /// Lowers a run of at least two guard-free literal arms as one `switch`.
    fn try_switch(
        &mut self,
        i: u32,
        v: Value,
        sc: Scalar,
        arms: &[Arm],
        k: u32,
    ) -> Result<bool, LowerError> {
        let mut values: Vec<Vec<u64>> = Vec::new();
        for arm in arms.iter().skip(k as usize) {
            if arm.guard.is_some() {
                break;
            }
            let mut vs = Vec::new();
            if !self.switch_cases(arm.pat, sc, &mut vs) {
                break;
            }
            values.push(vs);
        }
        if values.len() < 2 {
            return Ok(false);
        }
        let mut seen: BTreeMap<u64, ()> = BTreeMap::new();
        let mut cases: Vec<SwitchCase> = Vec::new();
        let mut bodies: Vec<Block> = Vec::with_capacity(values.len());
        for vs in &values {
            let body = self.new_block()?;
            bodies.push(body);
            for &bits in vs {
                if seen.insert(bits, ()).is_none() {
                    cases.push(SwitchCase {
                        value: bits,
                        dest: BlockCall::new(body, &[]),
                    });
                }
            }
        }
        let default = self.new_block()?;
        if self.dead {
            self.b.unreachable()?;
        } else {
            for c in &cases {
                self.mark(c.dest.block);
            }
            self.b.switch(v, cases, BlockCall::new(default, &[]))?;
            self.mark(default);
        }
        let n = u32::try_from(bodies.len()).map_err(|_| internal(self.node))?;
        if let Some(st) = self.st.matches.get_mut(i as usize) {
            st.run = bodies;
            st.run_start = k;
            st.run_pos = 0;
            st.next = Some(default);
            st.k = k + n;
        }
        self.tasks.push(Task::SwitchArm(i));
        Ok(true)
    }

    /// Lowers the next body of a switch run.
    pub(super) fn switch_arm(&mut self, i: u32) -> Result<(), LowerError> {
        let (arms, arm_index, body) = {
            let st = self.match_st(i)?;
            let body = st.run.get(st.run_pos as usize).copied();
            (st.arms, st.run_start + st.run_pos, body)
        };
        let body = body.ok_or_else(|| internal(self.node))?;
        let arm = self
            .cx
            .hir
            .list(arms)
            .get(arm_index as usize)
            .copied()
            .ok_or_else(|| internal(self.node))?;
        if let Some(st) = self.st.matches.get_mut(i as usize) {
            st.run_pos += 1;
        }
        self.enter_sealed(body)?;
        self.tasks.push(Task::SwitchArmEnd(i));
        self.tasks.push(Task::Expr(arm.body));
        Ok(())
    }

    pub(super) fn switch_arm_end(&mut self, i: u32) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        let (res, exit, more, next) = {
            let st = self.match_st(i)?;
            (
                st.res,
                st.exit,
                (st.run_pos as usize) < st.run.len(),
                st.next,
            )
        };
        if !self.dead {
            self.store_res(res, v)?;
        }
        self.jump(exit)?;
        if more {
            self.tasks.push(Task::SwitchArm(i));
        } else {
            let next = next.ok_or_else(|| internal(self.node))?;
            self.enter_sealed(next)?;
            self.tasks.push(Task::MatchArm(i));
        }
        Ok(())
    }

    // ---------------------------------------------------------------- jumps

    /// The region index a `break`/`continue` targets: the labeled loop or
    /// block, or the innermost loop.
    fn jump_target(&self, label: Option<BinderId>, is_continue: bool) -> Result<usize, LowerError> {
        let i = match label {
            Some(l) => self.labels.get(&l).copied(),
            None => self.loop_idx.last().copied(),
        }
        .ok_or_else(|| internal(self.node))?;
        match self.regions.get(i) {
            Some(Region::Loop { .. }) => Ok(i),
            Some(Region::Label { .. }) if !is_continue => Ok(i),
            _ => Err(internal(self.node)),
        }
    }

    pub(super) fn break_task(&mut self, e: ExprId) -> Result<(), LowerError> {
        let Expr::Break { label, .. } = *self.cx.hir.expr(e) else {
            return Err(internal(self.node));
        };
        let v = self.pop_val()?;
        let t = self.jump_target(label, false)?;
        let res = match self.regions.get(t) {
            Some(Region::Loop { res, .. } | Region::Label { res, .. }) => *res,
            _ => return Err(internal(self.node)),
        };
        if !self.dead {
            self.store_res(res, v)?;
        }
        let level = self.regions.len();
        self.route(Exit::Break(t), level)?;
        self.go_dead()?;
        self.vals.push(Val::None);
        Ok(())
    }

    pub(super) fn continue_now(&mut self, label: Option<BinderId>) -> Result<(), LowerError> {
        let t = self.jump_target(label, true)?;
        let level = self.regions.len();
        self.route(Exit::Continue(t), level)?;
        self.go_dead()?;
        self.vals.push(Val::None);
        Ok(())
    }

    pub(super) fn return_task(&mut self) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        if !self.dead {
            match (self.ret, v) {
                (Ret::Scalar(var), Val::Scalar(x, _)) => self.b.def_var(var, x)?,
                (Ret::Sret(dst, agg), Val::Mem(slot, _)) => {
                    let src = self.b.stack_addr(slot)?;
                    self.copy(dst, src, agg)?;
                }
                _ => {}
            }
        }
        let level = self.regions.len();
        self.route(Exit::Return, level)?;
        self.go_dead()?;
        self.vals.push(Val::None);
        Ok(())
    }

    /// Emits the terminator taking `exit` from inside the first `level`
    /// regions: through the innermost cleanup it crosses, else straight to
    /// the target.
    fn route(&mut self, exit: Exit, level: usize) -> Result<(), LowerError> {
        let target = match exit {
            Exit::Break(t) | Exit::Continue(t) => Some(t),
            _ => None,
        };
        // The innermost cleanup inside `level`, if the exit crosses it.
        if let Some(c) = innermost_below(&self.cleanup_idx, level) {
            if target.is_none_or(|t| c > t) {
                return self.enter_cleanup(c, exit);
            }
        }
        match (target, exit) {
            (Some(t), _) => {
                let dest = match (self.regions.get(t), exit) {
                    (Some(Region::Loop { brk, .. }), Exit::Break(_)) => *brk,
                    (Some(Region::Loop { cont, .. }), Exit::Continue(_)) => *cont,
                    (Some(Region::Label { brk, .. }), Exit::Break(_)) => *brk,
                    _ => return Err(internal(self.node)),
                };
                self.jump(dest)
            }
            (None, Exit::Return) => self.ret_from_var(),
            _ => Err(internal(self.node)),
        }
    }

    fn ret_from_var(&mut self) -> Result<(), LowerError> {
        if self.dead {
            self.b.unreachable()?;
            return Ok(());
        }
        match self.ret {
            Ret::Scalar(var) => {
                let x = self.b.use_var(var)?;
                self.b.ret(&[x])?;
            }
            Ret::Unit | Ret::Sret(..) => self.b.ret(&[])?,
            Ret::Never => self.b.unreachable()?,
        }
        Ok(())
    }

    /// Records `exit` in cleanup `i` and jumps into its body.
    fn enter_cleanup(&mut self, i: usize, exit: Exit) -> Result<(), LowerError> {
        let (code, dest, entry) = {
            let Some(Region::Cleanup(c)) = self.regions.get_mut(i) else {
                return Err(internal(None));
            };
            let next = u32::try_from(c.exits.len()).map_err(|_| internal(None))?;
            let code = *c.keys.entry(exit).or_insert(next);
            if code == next {
                c.exits.push(exit);
            }
            (code, c.dest, c.entry)
        };
        // Each exit is a dispatch case later: charge it, so nested cleanups
        // with many targets stay within the step budget.
        self.tick()?;
        if !self.dead {
            let d = self.b.iconst(Type::U32, i128::from(code))?;
            self.b.def_var(dest, d)?;
        }
        self.jump(entry)
    }

    /// Normal completion into cleanup `c`.
    pub(super) fn fallthrough(&mut self, c: usize) -> Result<(), LowerError> {
        self.enter_cleanup(c, Exit::Fallthrough)
    }

    /// Opens a cleanup region whose body is `body` (a `defer`, or a
    /// `finally`).
    pub(super) fn open_cleanup(&mut self, body: ExprId) -> Result<usize, LowerError> {
        let entry = self.new_block()?;
        let after = self.new_block()?;
        let dest = self.b.declare_var(Type::U32)?;
        let exn = self.b.declare_var(Type::Ptr)?;
        let i = self.push_region(Region::Cleanup(Box::new(Cleanup {
            entry,
            dest,
            exn,
            exits: Vec::new(),
            keys: BTreeMap::new(),
            pad: None,
            err: None,
            after,
            body,
        })));
        Ok(i)
    }

    /// Closes the innermost region, a cleanup: lowers its body once (if
    /// anything enters it), then dispatches its exits.
    pub(super) fn close_cleanup(&mut self) -> Result<(), LowerError> {
        let Some(Region::Cleanup(c)) = self.pop_region() else {
            return Err(internal(self.node));
        };
        let c = *c;
        let level = self.regions.len();
        for b in [c.pad, c.err].into_iter().flatten() {
            self.b.seal_block(b)?;
        }
        if !self.has_pred(c.entry) {
            let after = c.after;
            self.enter_sealed(after)?;
            return Ok(());
        }
        self.enter_sealed(c.entry)?;
        let body = c.body;
        let i = idx(&self.closings)?;
        self.closings.push((c, level));
        self.tasks.push(Task::CleanupDispatch(i));
        self.tasks.push(Task::Discard);
        self.tasks.push(Task::Expr(body));
        Ok(())
    }

    pub(super) fn cleanup_dispatch(&mut self, i: u32) -> Result<(), LowerError> {
        let (exits, dest, exn, after, level) = {
            let (c, level) = self
                .closings
                .get(i as usize)
                .ok_or_else(|| internal(self.node))?;
            (c.exits.clone(), c.dest, c.exn, c.after, *level)
        };
        if !self.dead {
            match exits.as_slice() {
                [] => self.b.unreachable()?,
                [only] => self.perform(*only, exn, after, level)?,
                [first, rest @ ..] => {
                    let d = self.b.use_var(dest)?;
                    let mut cases = Vec::with_capacity(rest.len());
                    let mut blocks = Vec::with_capacity(exits.len());
                    let b0 = self.new_block()?;
                    blocks.push((b0, *first));
                    for (k, &x) in rest.iter().enumerate() {
                        let b = self.new_block()?;
                        cases.push(SwitchCase {
                            value: (k + 1) as u64,
                            dest: BlockCall::new(b, &[]),
                        });
                        blocks.push((b, x));
                    }
                    for c in &cases {
                        self.mark(c.dest.block);
                    }
                    self.mark(b0);
                    self.b.switch(d, cases, BlockCall::new(b0, &[]))?;
                    for (b, x) in blocks {
                        self.tick()?;
                        self.enter_sealed(b)?;
                        self.perform(x, exn, after, level)?;
                    }
                }
            }
        }
        self.enter_sealed(after)
    }

    /// Continues an exit after a cleanup body ran.
    fn perform(
        &mut self,
        x: Exit,
        exn: ir_lang::Variable,
        after: Block,
        level: usize,
    ) -> Result<(), LowerError> {
        match x {
            Exit::Fallthrough => self.jump(after),
            Exit::Rethrow => {
                let p = self.b.use_var(exn)?;
                self.raise_terminate(p, level)
            }
            other => self.route(other, level),
        }
    }

    // ---------------------------------------------------------- exceptions

    /// The innermost region exceptions go to (a handler or a cleanup).
    pub(super) fn exception_region(&self) -> Option<usize> {
        self.exn_idx.last().copied()
    }

    /// Emits the terminator that raises payload `p` from inside the first
    /// `level` regions.
    pub(super) fn raise_terminate(&mut self, p: Value, level: usize) -> Result<(), LowerError> {
        if let Some(i) = innermost_below(&self.exn_idx, level) {
            return match self.regions.get(i) {
                Some(Region::Handler { dispatch, exn, .. }) => {
                    let (dispatch, exn) = (*dispatch, *exn);
                    if !self.dead {
                        self.b.def_var(exn, p)?;
                    }
                    self.jump(dispatch)
                }
                Some(Region::Cleanup(c)) => {
                    let exn = c.exn;
                    if !self.dead {
                        self.b.def_var(exn, p)?;
                    }
                    self.enter_cleanup(i, Exit::Rethrow)
                }
                _ => Err(internal(self.node)),
            };
        }
        if self.dead {
            self.b.unreachable()?;
        } else {
            self.b.resume(p)?;
        }
        Ok(())
    }

    /// The landing pad of exception region `i` (made on first use): it
    /// forwards the payload to the region.
    pub(super) fn pad(&mut self, i: usize) -> Result<Block, LowerError> {
        let existing = match self.regions.get(i) {
            Some(Region::Handler { pad, .. }) => *pad,
            Some(Region::Cleanup(c)) => c.pad,
            _ => return Err(internal(self.node)),
        };
        if let Some(p) = existing {
            return Ok(p);
        }
        let pad = self.new_block_with(&[Type::Ptr])?;
        match self.regions.get_mut(i) {
            Some(Region::Handler { pad: slot, .. }) => *slot = Some(pad),
            Some(Region::Cleanup(c)) => c.pad = Some(pad),
            _ => return Err(internal(self.node)),
        }
        let saved = self.save()?;
        self.b.switch_to(pad)?;
        self.dead = false;
        let p = self.b.block_param(pad, 0)?;
        self.raise_terminate(p, i + 1)?;
        self.restore(saved)?;
        Ok(pad)
    }

    /// Allocates an exception record through the runtime.
    fn exception(&mut self, tag: u32, bits: Value) -> Result<Value, LowerError> {
        let f = self.t.runtime.ok_or_else(|| internal(self.node))?;
        let t = self.b.iconst(Type::U32, i128::from(tag))?;
        let inst = self.b.call(Call::direct(f, &[t, bits]))?;
        self.b
            .results(inst)
            .first()
            .ok_or_else(|| internal(self.node))
    }

    /// Raises a run-time error with a constant code here, then continues in
    /// dead code.
    pub(super) fn raise_const(&mut self, code: u32) -> Result<(), LowerError> {
        if self.dead {
            return Ok(());
        }
        match self.cx.options.errors() {
            ErrorMode::Trap => self.b.trap(code)?,
            _ => {
                let bits = self.b.iconst(Type::U64, i128::from(code))?;
                let p = self.exception(rt::TAG_ERROR, bits)?;
                let level = self.regions.len();
                self.raise_terminate(p, level)?;
            }
        }
        self.go_dead()
    }

    /// The error target of `check` terminators here (made on first use): a
    /// block taking the OPS code that raises it (or traps with it).
    pub(super) fn check_error_block(&mut self) -> Result<Block, LowerError> {
        let trap = self.cx.options.errors() == ErrorMode::Trap;
        let region = if trap { None } else { self.exception_region() };
        let existing = match region {
            None => self.fn_err,
            Some(i) => match self.regions.get(i) {
                Some(Region::Handler { err, .. }) => *err,
                Some(Region::Cleanup(c)) => c.err,
                _ => None,
            },
        };
        if let Some(b) = existing {
            return Ok(b);
        }
        let blk = self.new_block_with(&[Type::U32])?;
        match region {
            None => self.fn_err = Some(blk),
            Some(i) => match self.regions.get_mut(i) {
                Some(Region::Handler { err, .. }) => *err = Some(blk),
                Some(Region::Cleanup(c)) => c.err = Some(blk),
                _ => return Err(internal(self.node)),
            },
        }
        let saved = self.save()?;
        self.b.switch_to(blk)?;
        self.dead = false;
        let err = self.b.block_param(blk, 0)?;
        if trap {
            // The OPS codes are 1..=6 (IR.md §7: `err` is an OPS §6 code).
            let mut cases = Vec::with_capacity(6);
            let mut traps = Vec::with_capacity(6);
            for code in 1..=6u32 {
                let t = self.new_block()?;
                cases.push(SwitchCase {
                    value: u64::from(code),
                    dest: BlockCall::new(t, &[]),
                });
                traps.push((t, code));
            }
            let other = self.new_block()?;
            for c in &cases {
                self.mark(c.dest.block);
            }
            self.mark(other);
            self.b.switch(err, cases, BlockCall::new(other, &[]))?;
            for (t, code) in traps {
                self.enter_sealed(t)?;
                self.b.trap(code)?;
            }
            self.enter_sealed(other)?;
            self.b.unreachable()?;
        } else {
            let bits = self.b.convert(ConvOp::Zext, Policy::NONE, err, Type::U64)?;
            let p = self.exception(rt::TAG_ERROR, bits)?;
            let level = region.map_or(0, |i| i + 1);
            self.raise_terminate(p, level)?;
        }
        self.restore(saved)?;
        Ok(blk)
    }

    pub(super) fn throw_task(&mut self) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        if let Val::Scalar(x, sc) = v {
            if !self.dead {
                let tag = sc
                    .prim()
                    .and_then(rt::tag)
                    .ok_or_else(|| internal(self.node))?;
                let bits = self.bits64_of(x, sc)?;
                let p = self.exception(tag, bits)?;
                let level = self.regions.len();
                self.raise_terminate(p, level)?;
                self.go_dead()?;
            }
        }
        self.vals.push(Val::None);
        Ok(())
    }

    /// A scalar's bits zero-extended to `u64`.
    pub(super) fn bits64_of(&mut self, x: Value, sc: Scalar) -> Result<Value, LowerError> {
        let ty = sc.ir();
        Ok(match ty {
            Type::Bool => self
                .b
                .convert(ConvOp::BoolToInt, Policy::NONE, x, Type::U64)?,
            Type::F64 => self
                .b
                .convert(ConvOp::Bitcast, Policy::NONE, x, Type::U64)?,
            Type::F32 => {
                let u = self
                    .b
                    .convert(ConvOp::Bitcast, Policy::NONE, x, Type::U32)?;
                self.b.convert(ConvOp::Zext, Policy::NONE, u, Type::U64)?
            }
            _ => {
                let w = ty.int_bits().unwrap_or(64);
                let unsigned = Type::int(w, false).ok_or_else(|| internal(self.node))?;
                let u = if unsigned == ty {
                    x
                } else {
                    self.b.convert(ConvOp::Bitcast, Policy::NONE, x, unsigned)?
                };
                if w == 64 {
                    u
                } else {
                    self.b.convert(ConvOp::Zext, Policy::NONE, u, Type::U64)?
                }
            }
        })
    }

    /// The scalar whose bits (zero-extended) are `bits`.
    fn scalar_of_bits64(&mut self, bits: Value, sc: Scalar) -> Result<Value, LowerError> {
        let ty = sc.ir();
        Ok(match ty {
            Type::Bool => {
                let z = self.b.iconst(Type::U64, 0)?;
                self.b.compare(CmpOp::Ne, bits, z)?
            }
            Type::F64 => self
                .b
                .convert(ConvOp::Bitcast, Policy::NONE, bits, Type::F64)?,
            Type::F32 => {
                let u = self
                    .b
                    .convert(ConvOp::Narrow, Policy::NONE, bits, Type::U32)?;
                self.b
                    .convert(ConvOp::Bitcast, Policy::NONE, u, Type::F32)?
            }
            _ => {
                let w = ty.int_bits().unwrap_or(64);
                let unsigned = Type::int(w, false).ok_or_else(|| internal(self.node))?;
                let u = if w == 64 {
                    bits
                } else {
                    self.b
                        .convert(ConvOp::Narrow, Policy::NONE, bits, unsigned)?
                };
                if unsigned == ty {
                    u
                } else {
                    self.b.convert(ConvOp::Bitcast, Policy::NONE, u, ty)?
                }
            }
        })
    }

    // ------------------------------------------------------------------ try

    pub(super) fn try_start(&mut self, e: ExprId) -> Result<(), LowerError> {
        let Expr::Try {
            body,
            catches,
            finally,
        } = *self.cx.hir.expr(e)
        else {
            return Err(internal(self.node));
        };
        let r = self.repr_of(e)?;
        let res = self.res_for(r)?;
        let cleanup = match finally {
            Some(f) => Some(self.open_cleanup(f)?),
            None => None,
        };
        // The try's exit is the cleanup's `after` (or its own block).
        let exit = match cleanup.and_then(|c| self.regions.get(c)) {
            Some(Region::Cleanup(c)) => c.after,
            _ => self.new_block()?,
        };
        if !catches.is_empty() {
            let dispatch = self.new_block()?;
            let exn = self.b.declare_var(Type::Ptr)?;
            let _ = self.push_region(Region::Handler {
                dispatch,
                exn,
                pad: None,
                err: None,
            });
        }
        let i = idx(&self.st.tries)?;
        self.st.tries.push(TrySt {
            catches,
            k: 0,
            res,
            exit,
            cleanup,
            exn: None,
            next: None,
        });
        self.tasks.push(Task::TryBody(i));
        self.tasks.push(Task::Expr(body));
        Ok(())
    }

    fn try_st(&self, i: u32) -> Result<&TrySt, LowerError> {
        self.st
            .tries
            .get(i as usize)
            .ok_or_else(|| internal(self.node))
    }

    /// Normal completion of the body or a catch arm: on to the `finally`, or
    /// the exit.
    fn try_complete(&mut self, i: u32, v: Val) -> Result<(), LowerError> {
        let (res, exit, cleanup) = {
            let st = self.try_st(i)?;
            (st.res, st.exit, st.cleanup)
        };
        if !self.dead {
            self.store_res(res, v)?;
        }
        match cleanup {
            Some(c) => {
                if !self.dead {
                    self.fallthrough(c)?;
                }
                Ok(())
            }
            None => self.jump(exit),
        }
    }

    pub(super) fn try_body(&mut self, i: u32) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        self.try_complete(i, v)?;
        let catches = self.try_st(i)?.catches;
        if catches.is_empty() {
            self.tasks.push(Task::TryEnd(i));
            return Ok(());
        }
        let Some(Region::Handler {
            dispatch,
            exn,
            pad,
            err,
        }) = self.pop_region()
        else {
            return Err(internal(self.node));
        };
        for b in [pad, err].into_iter().flatten() {
            self.b.seal_block(b)?;
        }
        if !self.has_pred(dispatch) {
            self.b.seal_block(dispatch)?;
            self.tasks.push(Task::TryEnd(i));
            return Ok(());
        }
        self.enter_sealed(dispatch)?;
        let p = self.b.use_var(exn)?;
        let tag = self.b.load(Type::U32, p, ir_lang::MemFlags::new(4))?;
        let off = self.b.iconst(Type::U64, i128::from(rt::BITS_OFFSET))?;
        let bp = self.b.ptr_offset(p, off)?;
        let bits = self.b.load(Type::U64, bp, ir_lang::MemFlags::new(8))?;
        if let Some(st) = self.st.tries.get_mut(i as usize) {
            st.exn = Some((p, tag, bits));
        }
        self.tasks.push(Task::CatchArm(i));
        Ok(())
    }

    pub(super) fn catch_arm(&mut self, i: u32) -> Result<(), LowerError> {
        let (catches, k, exn) = {
            let st = self.try_st(i)?;
            (st.catches, st.k, st.exn)
        };
        let (p, tag, bits) = exn.ok_or_else(|| internal(self.node))?;
        let Some(arm) = self.cx.hir.list(catches).get(k as usize).copied() else {
            // No arm caught it: rethrow outward.
            let level = self.regions.len();
            self.raise_terminate(p, level)?;
            self.go_dead()?;
            self.tasks.push(Task::TryEnd(i));
            return Ok(());
        };
        let next = self.new_block()?;
        if let Some(st) = self.st.tries.get_mut(i as usize) {
            st.next = Some(next);
        }
        match *self.cx.hir.pat(arm.pat) {
            Pat::Wild => {}
            Pat::TypeTest { ty, pat } => {
                let sc = match self.t.types.known_ty(ty) {
                    Some(Repr::Scalar(sc)) => sc,
                    _ => return Err(internal(self.node)),
                };
                let want = sc
                    .prim()
                    .and_then(rt::tag)
                    .ok_or_else(|| internal(self.node))?;
                let w = self.b.iconst(Type::U32, i128::from(want))?;
                let is = self.b.compare(CmpOp::Eq, tag, w)?;
                self.branch_cont(is, next)?;
                if let Some(sub) = pat {
                    let v = self.scalar_of_bits64(bits, sc)?;
                    let _ = self.test_pat(sub, Subject::Scalar(v, sc), Some(next))?;
                }
            }
            _ => return Err(internal(self.node)),
        }
        self.tasks.push(Task::CatchArmEnd(i));
        self.tasks.push(Task::Expr(arm.body));
        if let Some(g) = arm.guard {
            self.tasks.push(Task::Guard(next.as_u32()));
            self.tasks.push(Task::Expr(g));
        }
        Ok(())
    }

    pub(super) fn catch_arm_end(&mut self, i: u32) -> Result<(), LowerError> {
        let v = self.pop_val()?;
        self.try_complete(i, v)?;
        let next = self.try_st(i)?.next.ok_or_else(|| internal(self.node))?;
        self.enter_sealed(next)?;
        if let Some(st) = self.st.tries.get_mut(i as usize) {
            st.k += 1;
        }
        self.tasks.push(Task::CatchArm(i));
        Ok(())
    }

    pub(super) fn try_end(&mut self, i: u32) -> Result<(), LowerError> {
        let (cleanup, exit) = {
            let st = self.try_st(i)?;
            (st.cleanup, st.exit)
        };
        self.tasks.push(Task::TryResult(i));
        match cleanup {
            Some(c) => {
                if self.regions.len() != c + 1 {
                    return Err(internal(self.node));
                }
                self.close_cleanup()
            }
            None => self.enter_sealed(exit),
        }
    }

    pub(super) fn try_result(&mut self, i: u32) -> Result<(), LowerError> {
        let res = self.try_st(i)?.res;
        let v = self.load_res(res)?;
        self.vals.push(v);
        Ok(())
    }
}
