//! The pre-scan of a body: before a function is built (while the module can
//! still take declarations), check that every node is in the subset, map
//! every type the body uses (declaring its aggregates), and declare the host
//! functions it calls. The first unsupported node in canonical order is the
//! one reported.

use alloc::{collections::BTreeSet, vec::Vec};

use hir_lang::{ArgKind, Def, Expr, ExprId, ItemId, ItemKind, Lit, NodeRef, Pat, PatId, Res, Stmt};
use hir_lang::{BindMode, BinderKind, Control, Op, OpKind, Overflow, Shift};
use intern_lang::Lookup;
use ir_lang::Module;
use typeck_lang::{Coercion, Type as TType};

use crate::{
    error::{Construct, LowerError},
    options::ErrorMode,
    types::{Repr, Scalar},
    unit::{
        Budget, Ctx, FnEntry, Tables, declare_extern, preorder, refutable, runtime_exception,
        unsupported,
    },
};

struct Scanner<'a, 'c, L> {
    cx: &'a Ctx<'c, L>,
    module: &'a mut Module,
    tables: &'a mut Tables,
    budget: &'a mut Budget,
    err: Option<LowerError>,
    /// The callee the walk meets next, and its call's argument count.
    callee_next: Option<(ExprId, usize)>,
    catch_tops: BTreeSet<PatId>,
    queue: Vec<ExprId>,
    needs_runtime: bool,
}

/// Pre-scans a function's parameters and body, and every constant they use.
pub(crate) fn scan_function<L: Lookup>(
    cx: &Ctx<'_, L>,
    module: &mut Module,
    tables: &mut Tables,
    budget: &mut Budget,
    entry: &FnEntry,
    body: ExprId,
) -> Result<(), LowerError> {
    let mut s = Scanner {
        cx,
        module,
        tables,
        budget,
        err: None,
        callee_next: None,
        catch_tops: BTreeSet::new(),
        queue: Vec::new(),
        needs_runtime: false,
    };
    let hir = cx.hir;
    for &(pat, _) in &entry.params {
        if refutable(hir, pat) {
            return Err(unsupported(hir, Construct::Pattern, NodeRef::Pat(pat)));
        }
        preorder(hir, NodeRef::Pat(pat), |n| s.visit(n));
        if let Some(e) = s.err.take() {
            return Err(e);
        }
    }
    s.queue.push(body);
    while let Some(root) = s.queue.pop() {
        preorder(hir, NodeRef::Expr(root), |n| s.visit(n));
        if let Some(e) = s.err.take() {
            return Err(e);
        }
    }
    if s.needs_runtime {
        let _ = runtime_exception(s.module, s.tables)?;
    }
    Ok(())
}

impl<L: Lookup> Scanner<'_, '_, L> {
    fn visit(&mut self, node: NodeRef) -> Control {
        let r = match self.budget.tick() {
            Err(e) => Err(e),
            Ok(()) => match node {
                NodeRef::Expr(e) => self.expr(e),
                NodeRef::Stmt(s) => self.stmt(s),
                NodeRef::Pat(p) => self.pat(p).map(|()| Control::Continue),
                NodeRef::Path(_) => Ok(Control::Continue),
                _ => Ok(Control::Skip),
            },
        };
        match r {
            Ok(c) => c,
            Err(e) => {
                self.err = Some(e);
                Control::Stop
            }
        }
    }

    fn no(&self, c: Construct, node: NodeRef) -> LowerError {
        unsupported(self.cx.hir, c, node)
    }

    fn map_type(&mut self, t: typeck_lang::TypeId, at: NodeRef) -> Result<Repr, LowerError> {
        self.tables
            .types
            .of_type(self.cx.hir, self.cx.typed, self.module, t, at)
    }

    fn expr(&mut self, e: ExprId) -> Result<Control, LowerError> {
        let hir = self.cx.hir;
        let typed = self.cx.typed;
        let at = NodeRef::Expr(e);
        let callee_args = match self.callee_next.take() {
            Some((c, n)) if c == e => Some(n),
            _ => None,
        };
        let expr = *hir.expr(e);
        let raise = self.cx.options.errors() == ErrorMode::Raise;
        match expr {
            Expr::Lit(Lit::Null) => return Err(self.no(Construct::Nullable, at)),
            Expr::Lit(Lit::Str(_) | Lit::Bytes(_) | Lit::BigInt(_)) => {
                return Err(self.no(Construct::String, at));
            }
            Expr::Lit(_) => {}
            Expr::Path(p) => {
                if let Some(n) = callee_args {
                    self.callee(e, p, n)?;
                    // A callee's type is a function type; it is not a value.
                    return Ok(Control::Continue);
                }
                self.value_path(p, at)?;
            }
            Expr::Tuple(_) | Expr::Array(_) | Expr::Repeat { .. } | Expr::Record { .. } => {}
            Expr::Map(_) => return Err(self.no(Construct::Collection, at)),
            Expr::Call { callee, args } => {
                for a in hir.list(args) {
                    if a.kind != ArgKind::Positional || a.place {
                        return Err(self.no(Construct::CallShape, NodeRef::Expr(a.value)));
                    }
                }
                if !matches!(hir.expr(callee), Expr::Path(_)) {
                    return Err(self.no(Construct::FunctionValue, NodeRef::Expr(callee)));
                }
                self.callee_next = Some((callee, args.len()));
            }
            Expr::MethodCall { .. } => return Err(self.no(Construct::MethodCall, at)),
            Expr::Field { .. } | Expr::Index { .. } => {
                if raise && matches!(expr, Expr::Index { .. }) {
                    self.needs_runtime = true;
                }
            }
            Expr::DynField { .. }
            | Expr::DynMethodCall { .. }
            | Expr::VarVar(_)
            | Expr::Append(_)
            | Expr::RefAssign { .. } => return Err(self.no(Construct::DynamicAccess, at)),
            Expr::Op { op, .. } => {
                self.op(op, at, raise)?;
            }
            Expr::Cast { policy, .. } => {
                if policy.overflow == Some(Overflow::Promote) {
                    return Err(self.no(Construct::DynamicValue, at));
                }
                if raise
                    && (policy.overflow == Some(Overflow::Error)
                        || policy.float_to_int == Some(hir_lang::FloatToInt::Error))
                {
                    self.needs_runtime = true;
                }
            }
            Expr::Assign { op, .. } => {
                if let Some(op) = op {
                    self.op(op, at, raise)?;
                }
            }
            Expr::LetPlace { .. } => {}
            Expr::Deref(_) | Expr::Borrow { .. } => return Err(self.no(Construct::Reference, at)),
            Expr::Block(_)
            | Expr::If { .. }
            | Expr::Loop { .. }
            | Expr::Break { .. }
            | Expr::Continue { .. }
            | Expr::Return(_) => {}
            Expr::Match { .. } => {
                if raise {
                    self.needs_runtime = true;
                }
            }
            Expr::Closure(_) => return Err(self.no(Construct::Closure, at)),
            Expr::Throw(v) => {
                self.needs_runtime = true;
                let t = typed.expr(v).ok_or(LowerError::MissingType {
                    node: NodeRef::Expr(v),
                })?;
                let t = typed.coercion(v).map_or(t, |(_, target)| target);
                match self.map_type(t, NodeRef::Expr(v)) {
                    Ok(Repr::Scalar(s)) if s != Scalar::Host => {}
                    Ok(Repr::Never) => {}
                    Ok(_) | Err(LowerError::Unsupported { .. }) => {
                        return Err(self.no(Construct::ThrowValue, NodeRef::Expr(v)));
                    }
                    Err(other) => return Err(other),
                }
            }
            Expr::Try { catches, .. } => {
                for arm in hir.list(catches) {
                    let _ = self.catch_tops.insert(arm.pat);
                }
            }
            Expr::Await(_) | Expr::Yield { .. } | Expr::YieldFrom(_) | Expr::Spawn(_) => {
                return Err(self.no(Construct::Coroutine, at));
            }
            Expr::Asm(_) => return Err(self.no(Construct::Asm, at)),
            Expr::Intrinsic { .. } => return Err(self.no(Construct::Intrinsic, at)),
            Expr::Err => return Err(self.no(Construct::ErrorNode, at)),
        }
        let t = typed.expr(e).ok_or(LowerError::MissingType { node: at })?;
        let _ = self.map_type(t, at)?;
        if let Some((c, target)) = typed.coercion(e) {
            if c != Coercion::Widen {
                return Err(self.no(Construct::Coercion, at));
            }
            let _ = self.map_type(target, at)?;
        }
        Ok(Control::Continue)
    }

    fn op(&mut self, op: Op, at: NodeRef, raise: bool) -> Result<(), LowerError> {
        if op.promotes() {
            return Err(self.no(Construct::DynamicValue, at));
        }
        let p = op.policy;
        let fallible = p.overflow == Some(Overflow::Error)
            || p.div_zero == Some(hir_lang::DivZero::Error)
            || p.shift == Some(Shift::Error)
            || p.shift == Some(Shift::Saturate)
            || p.float_to_int == Some(hir_lang::FloatToInt::Error)
            || matches!(op.kind, OpKind::Pow | OpKind::CharFromU32);
        if raise && fallible {
            self.needs_runtime = true;
        }
        Ok(())
    }

    /// A path in value position.
    fn value_path(&mut self, p: hir_lang::PathId, at: NodeRef) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        let path = hir.path(p);
        if path.unresolved > 0 {
            return Err(self.no(Construct::MethodCall, at));
        }
        if hir.list(path.segments).iter().any(|s| !s.args.is_empty()) {
            return Err(self.no(Construct::Generic, at));
        }
        match path.res {
            Res::Local(b) => match hir.binder(b).map(|b| b.kind) {
                Some(BinderKind::Local | BinderKind::Param | BinderKind::Place) => Ok(()),
                Some(BinderKind::Capture) => Err(self.no(Construct::Closure, at)),
                Some(BinderKind::ConstParam | BinderKind::TypeParam) => {
                    Err(self.no(Construct::Generic, at))
                }
                _ => Err(self.no(Construct::Unresolved, at)),
            },
            Res::Def(d) => {
                if d.unit() != hir.unit() {
                    return Err(self.no(Construct::ForeignUnit, at));
                }
                match d.def() {
                    Def::Variant(_) => Err(self.no(Construct::Sum, at)),
                    Def::Item(item) => match hir.get_item(item).map(|i| i.kind) {
                        Some(ItemKind::Const { value: Some(v), .. }) => {
                            self.queue_const(item, v);
                            Ok(())
                        }
                        Some(ItemKind::Global { .. }) => Ok(()),
                        Some(ItemKind::Record(r)) if r.shape == hir_lang::Shape::Unit => Ok(()),
                        Some(ItemKind::Fn(_) | ItemKind::Record(_)) => {
                            Err(self.no(self.skipped_or(item, Construct::FunctionValue), at))
                        }
                        Some(ItemKind::Sum(_)) => Err(self.no(Construct::Sum, at)),
                        _ => Err(self.no(Construct::Class, at)),
                    },
                }
            }
            Res::Extern(_) => Err(self.no(Construct::HostBoundary, at)),
            Res::Prim(_) => Err(self.no(Construct::OtherType, at)),
            Res::Unresolved => Err(self.no(Construct::Unresolved, at)),
            Res::Err => Err(self.no(Construct::ErrorNode, at)),
        }
    }

    fn skipped_or(&self, item: ItemId, default: Construct) -> Construct {
        self.tables
            .fn_skipped
            .get(&item)
            .copied()
            .unwrap_or(default)
    }

    fn queue_const(&mut self, item: ItemId, value: ExprId) {
        if self.tables.consts_scanned.insert(item) {
            self.queue.push(value);
        }
    }

    /// A callee: a function item of this unit, a tuple record's
    /// constructor, or a host function.
    fn callee(
        &mut self,
        e: ExprId,
        p: hir_lang::PathId,
        call_args: usize,
    ) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        let at = NodeRef::Expr(e);
        let path = hir.path(p);
        if path.unresolved > 0 {
            return Err(self.no(Construct::MethodCall, at));
        }
        if hir.list(path.segments).iter().any(|s| !s.args.is_empty()) {
            return Err(self.no(Construct::Generic, at));
        }
        match path.res {
            Res::Def(d) => {
                if d.unit() != hir.unit() {
                    return Err(self.no(Construct::ForeignUnit, at));
                }
                let Def::Item(item) = d.def() else {
                    return Err(self.no(Construct::Sum, at));
                };
                if let Some(f) = self.tables.function(item) {
                    if f.params.len() != call_args {
                        return Err(self.no(Construct::CallShape, at));
                    }
                    return Ok(());
                }
                match hir.get_item(item).map(|i| i.kind) {
                    Some(ItemKind::Record(r)) if r.shape == hir_lang::Shape::Tuple => {
                        if r.fields.len() != call_args {
                            return Err(self.no(Construct::CallShape, at));
                        }
                        Ok(())
                    }
                    Some(ItemKind::Fn(_)) => {
                        Err(self.no(self.skipped_or(item, Construct::Class), at))
                    }
                    _ => Err(self.no(Construct::FunctionValue, at)),
                }
            }
            Res::Extern(sym) => {
                let t = self
                    .cx
                    .typed
                    .expr(e)
                    .ok_or(LowerError::MissingType { node: at })?;
                let (params, ret, effects) = match self.cx.typed.types().get(t) {
                    TType::Fn {
                        params,
                        ret,
                        effects,
                    } => (params.to_vec(), ret, effects),
                    _ => return Err(self.no(Construct::HostBoundary, at)),
                };
                if effects.contains(hir_lang::Effects::ASYNC)
                    || effects.contains(hir_lang::Effects::YIELD)
                {
                    return Err(self.no(Construct::Coroutine, at));
                }
                if params.len() != call_args {
                    return Err(self.no(Construct::CallShape, at));
                }
                let mut reprs = Vec::with_capacity(params.len());
                for t in params {
                    match self.map_type(t, at)? {
                        r @ (Repr::Scalar(_) | Repr::Unit) => reprs.push(r),
                        _ => return Err(self.no(Construct::HostBoundary, at)),
                    }
                }
                let ret = match self.map_type(ret, at)? {
                    r @ (Repr::Scalar(_) | Repr::Unit | Repr::Never) => r,
                    Repr::Agg(_) => return Err(self.no(Construct::HostBoundary, at)),
                };
                declare_extern(self.cx, self.module, self.tables, sym, reprs, ret)
            }
            Res::Local(_) => Err(self.no(Construct::FunctionValue, at)),
            Res::Prim(_) => Err(self.no(Construct::OtherType, at)),
            Res::Unresolved => Err(self.no(Construct::Unresolved, at)),
            Res::Err => Err(self.no(Construct::ErrorNode, at)),
        }
    }

    fn stmt(&mut self, s: hir_lang::StmtId) -> Result<Control, LowerError> {
        let hir = self.cx.hir;
        let at = NodeRef::Stmt(s);
        match *hir.stmt(s) {
            Stmt::Let {
                pat, init, else_, ..
            } => {
                // The pattern comes first in canonical order, but a binder of
                // a closure's type is a closure, not a function value.
                if let Some(i) = init {
                    if matches!(hir.expr(i), Expr::Closure(_)) {
                        return Err(self.no(Construct::Closure, NodeRef::Expr(i)));
                    }
                }
                if init.is_none()
                    && !matches!(hir.pat(pat), Pat::Bind { sub: None, .. } | Pat::Wild)
                {
                    return Err(self.no(Construct::Pattern, NodeRef::Pat(pat)));
                }
                if self.cx.options.errors() == ErrorMode::Raise
                    && (else_.is_some() || refutable(hir, pat))
                {
                    self.needs_runtime = true;
                }
                Ok(Control::Continue)
            }
            Stmt::Expr(_) | Stmt::Defer(_) => Ok(Control::Continue),
            Stmt::Item(_) => Ok(Control::Skip),
            Stmt::Static { .. } | Stmt::Global { .. } => Err(self.no(Construct::Static, at)),
            Stmt::Err => Err(self.no(Construct::ErrorNode, at)),
        }
    }

    fn pat(&mut self, p: PatId) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        let typed = self.cx.typed;
        let at = NodeRef::Pat(p);
        let pat = *hir.pat(p);
        if self.catch_tops.contains(&p) {
            return match pat {
                Pat::Wild => Ok(()),
                Pat::TypeTest { ty, .. } => {
                    match self.tables.types.of_ty(hir, typed, self.module, ty, at) {
                        Ok(Repr::Scalar(s)) if s != Scalar::Host => Ok(()),
                        Ok(_) | Err(LowerError::Unsupported { .. }) => {
                            Err(self.no(Construct::ThrowValue, at))
                        }
                        Err(other) => Err(other),
                    }
                }
                _ => Err(self.no(Construct::DynamicCatch, at)),
            };
        }
        match pat {
            Pat::Wild | Pat::Range { .. } | Pat::Tuple { .. } | Pat::Or(_) => {}
            Pat::Bind { binder, mode, .. } => {
                if mode != BindMode::Value {
                    return Err(self.no(Construct::Pattern, at));
                }
                let t = typed
                    .binder(binder)
                    .ok_or(LowerError::MissingType { node: at })?;
                let _ = self.map_type(t, at)?;
            }
            Pat::Ident { binder, path } => match hir.path(path).res {
                Res::Def(d) => self.pattern_def(d, at)?,
                _ => {
                    let t = typed
                        .binder(binder)
                        .ok_or(LowerError::MissingType { node: at })?;
                    let _ = self.map_type(t, at)?;
                }
            },
            Pat::Path(path) => match hir.path(path).res {
                Res::Def(d) => self.pattern_def(d, at)?,
                _ => return Err(self.no(Construct::Unresolved, at)),
            },
            Pat::Lit(Lit::Null) => return Err(self.no(Construct::Nullable, at)),
            Pat::Lit(Lit::Str(_) | Lit::Bytes(_) | Lit::BigInt(_)) => {
                return Err(self.no(Construct::String, at));
            }
            Pat::Lit(_) => {}
            Pat::Ctor { path, .. } => {
                self.pattern_record(path, hir_lang::Shape::Tuple, at)?;
            }
            Pat::Record { path, .. } => {
                if let Some(path) = path {
                    self.pattern_record(path, hir_lang::Shape::Named, at)?;
                }
            }
            Pat::Slice { .. } | Pat::Ref { .. } | Pat::TypeTest { .. } => {
                return Err(self.no(Construct::Pattern, at));
            }
            Pat::Err => return Err(self.no(Construct::ErrorNode, at)),
        }
        let t = typed.pat(p).ok_or(LowerError::MissingType { node: at })?;
        let _ = self.map_type(t, at)?;
        Ok(())
    }

    /// A pattern naming a definition: a scalar constant or a unit record.
    fn pattern_def(&mut self, d: hir_lang::DefId, at: NodeRef) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        if d.unit() != hir.unit() {
            return Err(self.no(Construct::ForeignUnit, at));
        }
        match d.def() {
            Def::Variant(_) => Err(self.no(Construct::Sum, at)),
            Def::Item(item) => match hir.get_item(item).map(|i| i.kind) {
                Some(ItemKind::Const { value: Some(v), .. }) => {
                    self.queue_const(item, v);
                    Ok(())
                }
                Some(ItemKind::Record(r)) if r.shape == hir_lang::Shape::Unit => Ok(()),
                _ => Err(self.no(Construct::Pattern, at)),
            },
        }
    }

    fn pattern_record(
        &mut self,
        path: hir_lang::PathId,
        shape: hir_lang::Shape,
        at: NodeRef,
    ) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        let p = hir.path(path);
        match p.res {
            Res::Def(d) if d.unit() == hir.unit() => match d.def() {
                Def::Variant(_) => Err(self.no(Construct::Sum, at)),
                Def::Item(item) => match hir.get_item(item).map(|i| i.kind) {
                    Some(ItemKind::Record(r)) if r.shape == shape => Ok(()),
                    Some(ItemKind::Sum(_)) => Err(self.no(Construct::Sum, at)),
                    _ => Err(self.no(Construct::Pattern, at)),
                },
            },
            Res::Def(_) => Err(self.no(Construct::ForeignUnit, at)),
            _ => Err(self.no(Construct::Unresolved, at)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overflow_promote_is_dynamic() {
        let op = Op::new(OpKind::Add).with_overflow(Overflow::Promote);
        assert!(op.promotes());
    }
}
