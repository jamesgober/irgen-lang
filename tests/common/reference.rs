//! An independent reference evaluator of the HIR subset irgen-lang lowers.
//!
//! It walks the HIR directly (recursively: test programs are shallow) and
//! implements OPS semantics on its own, with Rust integers in `i128` and
//! native IEEE floats, so a lowering bug and an evaluator bug would have to
//! coincide to hide each other. The one shared piece is float `pow`, whose
//! bits OPS v2 defines by the family routine `ls_pow`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::HashMap;

use hir_lang::{
    BinderId, Def, Expr, ExprId, Hir, ItemId, ItemKind, Lit, Member, Op, OpKind, Pat, PatId, Prim,
    Res, Stmt, Ty, TyId,
};
use intern_lang::Interner;
use ir_lang::interp::Val;
use irgen_lang::{ErrorMode, rt};
use typeck_lang::{Coercion, Type as TT, TypeId, Typed};

use super::{Out, Unit};

/// A value.
#[derive(Clone, Debug, PartialEq)]
pub enum V {
    Unit,
    Bool(bool),
    Int(i128, Prim),
    F32(f32),
    F64(f64),
    Char(u32),
    Agg(Vec<V>),
    Obj(usize),
}

/// Non-local control flow.
#[derive(Clone, Debug)]
pub enum Flow {
    Break(Option<BinderId>, V),
    Continue(Option<BinderId>),
    Return(V),
    Throw(u32, u64),
    Trap(u32),
}

type R<T> = Result<T, Flow>;

/// A place: a root and a path of field or element indices.
#[derive(Clone, Debug)]
enum PRef {
    Local(BinderId, Vec<usize>),
    Global(ItemId, Vec<usize>),
    Temp(V, Vec<usize>),
}

pub struct Eval<'a> {
    hir: &'a Hir,
    typed: &'a Typed,
    names: &'a Interner,
    mode: ErrorMode,
    frames: Vec<HashMap<BinderId, V>>,
    aliases: Vec<HashMap<BinderId, PRef>>,
    globals: HashMap<ItemId, V>,
    objects: Vec<i64>,
    pub trace: Vec<i64>,
    steps: u64,
}

const MAX_STEPS: u64 = 20_000_000;

fn width(p: Prim) -> u32 {
    match p {
        Prim::I8 | Prim::U8 => 8,
        Prim::I16 | Prim::U16 => 16,
        Prim::I32 | Prim::U32 => 32,
        _ => 64,
    }
}

fn signed(p: Prim) -> bool {
    matches!(
        p,
        Prim::I8 | Prim::I16 | Prim::I32 | Prim::I64 | Prim::Isize
    )
}

fn range(p: Prim) -> (i128, i128) {
    let w = width(p);
    if signed(p) {
        (-(1i128 << (w - 1)), (1i128 << (w - 1)) - 1)
    } else {
        (0, (1i128 << w) - 1)
    }
}

/// Wraps an exact integer into `p` (two's complement).
fn wrap(v: i128, p: Prim) -> i128 {
    let w = width(p);
    let m = v & ((1i128 << w) - 1);
    if signed(p) && m >= (1i128 << (w - 1)) {
        m - (1i128 << w)
    } else {
        m
    }
}

/// A value's bits, zero-extended to 64.
pub fn bits_of(v: &V) -> u64 {
    match v {
        V::Unit | V::Agg(_) | V::Obj(_) => 0,
        V::Bool(b) => u64::from(*b),
        V::Int(x, p) => {
            let w = width(*p);
            let m = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
            (*x as u64) & m
        }
        V::F32(f) => u64::from(f.to_bits()),
        V::F64(f) => f.to_bits(),
        V::Char(c) => u64::from(*c),
    }
}

fn prim_of(v: &V) -> Option<Prim> {
    match v {
        V::Bool(_) => Some(Prim::Bool),
        V::Int(_, p) => Some(*p),
        V::F32(_) => Some(Prim::F32),
        V::F64(_) => Some(Prim::F64),
        V::Char(_) => Some(Prim::Char),
        _ => None,
    }
}

/// The value of type `p` with these bits.
pub fn from_bits(bits: u64, p: Prim) -> V {
    match p {
        Prim::Bool => V::Bool(bits != 0),
        Prim::F32 => V::F32(f32::from_bits(bits as u32)),
        Prim::F64 => V::F64(f64::from_bits(bits)),
        Prim::Char => V::Char(bits as u32),
        _ => V::Int(wrap(i128::from(bits), p), p),
    }
}

fn fmin(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_negative() { a } else { b }
    } else if a < b {
        a
    } else {
        b
    }
}

fn fmax(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_positive() { a } else { b }
    } else if a > b {
        a
    } else {
        b
    }
}

impl<'a> Eval<'a> {
    pub fn new(hir: &'a Hir, typed: &'a Typed, names: &'a Interner, mode: ErrorMode) -> Self {
        let mut e = Eval {
            hir,
            typed,
            names,
            mode,
            frames: Vec::new(),
            aliases: Vec::new(),
            globals: HashMap::new(),
            objects: Vec::new(),
            trace: Vec::new(),
            steps: 0,
        };
        e.init_globals();
        e
    }

    fn init_globals(&mut self) {
        let hir = self.hir;
        let mut stack = vec![hir.root()];
        while let Some(i) = stack.pop() {
            match hir.item(i).kind {
                ItemKind::Module { items, .. } => stack.extend(hir.list(items)),
                ItemKind::Global { init, ty, .. } => {
                    self.frames.push(HashMap::new());
                    self.aliases.push(HashMap::new());
                    let v = match init {
                        Some(e) => self.eval(e).unwrap_or(V::Unit),
                        None => self.zero_ty(ty.expect("annotated")),
                    };
                    let _ = self.frames.pop();
                    let _ = self.aliases.pop();
                    let _ = self.globals.insert(i, v);
                }
                _ => {}
            }
        }
    }

    /// A run-time error: raised or trapped per the mode.
    fn err(&self, code: u32) -> Flow {
        match self.mode {
            ErrorMode::Trap => Flow::Trap(code),
            _ => Flow::Throw(rt::TAG_ERROR, u64::from(code)),
        }
    }

    /// An error under a policy that may say `trap`.
    fn fail(&self, code: u32, trap: bool) -> Flow {
        if trap {
            Flow::Trap(code)
        } else {
            self.err(code)
        }
    }

    pub fn call(&mut self, item: ItemId, args: Vec<V>) -> R<V> {
        self.steps += 1;
        assert!(self.steps < MAX_STEPS, "reference fuel");
        assert!(self.frames.len() < 400, "reference depth");
        let ItemKind::Fn(def) = self.hir.item(item).kind else {
            panic!("not a function");
        };
        let mut frame = HashMap::new();
        for (&p, v) in self.hir.list(def.params).iter().zip(args) {
            let pat = self.hir.param(p).pat;
            let binds = self.matches(pat, &v).expect("irrefutable parameter");
            frame.extend(binds);
        }
        self.frames.push(frame);
        self.aliases.push(HashMap::new());
        let r = self.eval(def.body.expect("body"));
        let _ = self.frames.pop();
        let _ = self.aliases.pop();
        match r {
            Ok(v) => Ok(v),
            Err(Flow::Return(v)) => Ok(v),
            Err(f) => Err(f),
        }
    }

    fn ty_of(&self, e: ExprId) -> TypeId {
        self.typed.expr(e).expect("typed")
    }

    fn prim_type(&self, t: TypeId) -> Option<Prim> {
        match self.typed.types().get(t) {
            TT::Prim(p) => Some(p),
            _ => None,
        }
    }

    /// The zero value of a type (for `let x;`).
    fn zero_type(&self, t: TypeId) -> V {
        match self.typed.types().get(t) {
            TT::Prim(p) => from_bits(0, p),
            TT::Tuple([]) => V::Unit,
            TT::Tuple(es) => V::Agg(es.iter().map(|&e| self.zero_type(e)).collect()),
            TT::Array { elem, len } => V::Agg(vec![self.zero_type(elem); len as usize]),
            TT::Adt { item, .. } => self.zero_record(item),
            other => panic!("no zero for {other:?}"),
        }
    }

    fn zero_record(&self, item: ItemId) -> V {
        let ItemKind::Record(def) = self.hir.item(item).kind else {
            panic!("record");
        };
        V::Agg(
            self.hir
                .list(def.fields)
                .iter()
                .map(|&f| self.zero_ty(self.hir.field(f).ty.expect("annotated")))
                .collect(),
        )
    }

    fn zero_ty(&self, ty: TyId) -> V {
        match *self.hir.ty(ty) {
            Ty::Prim(p) => from_bits(0, p),
            Ty::Tuple(l) if l.is_empty() => V::Unit,
            Ty::Tuple(l) => V::Agg(self.hir.list(l).iter().map(|&t| self.zero_ty(t)).collect()),
            Ty::Array { elem, len } => {
                let Expr::Lit(Lit::Int(n)) = *self.hir.expr(len) else {
                    panic!("literal length");
                };
                V::Agg(vec![self.zero_ty(elem); n.value as usize])
            }
            Ty::Path(p) => match self.hir.path(p).res {
                Res::Prim(prim) => from_bits(0, prim),
                Res::Def(d) => match d.def() {
                    Def::Item(i) => self.zero_record(i),
                    Def::Variant(_) => panic!("sum"),
                },
                _ => panic!("type"),
            },
            _ => panic!("type"),
        }
    }

    fn ty_prim(&self, ty: TyId) -> Prim {
        match *self.hir.ty(ty) {
            Ty::Prim(p) => p,
            Ty::Path(p) => match self.hir.path(p).res {
                Res::Prim(prim) => prim,
                _ => panic!("prim type"),
            },
            _ => panic!("prim type"),
        }
    }

    fn coerce(&self, e: ExprId, v: V) -> V {
        match self.typed.coercion(e) {
            Some((Coercion::Widen, t)) => {
                let to = self.prim_type(t).expect("prim");
                match (v, to) {
                    (V::Int(x, _), Prim::F32) => V::F32(x as f32),
                    (V::Int(x, _), Prim::F64) => V::F64(x as f64),
                    (V::Int(x, _), p) => V::Int(x, p),
                    (V::Char(c), p) => V::Int(i128::from(c), p),
                    (V::F32(f), Prim::F64) => V::F64(f64::from(f)),
                    (other, _) => other,
                }
            }
            Some((c, _)) => panic!("coercion {c:?}"),
            None => v,
        }
    }

    pub fn eval(&mut self, e: ExprId) -> R<V> {
        self.steps += 1;
        assert!(self.steps < MAX_STEPS, "reference fuel");
        let v = self.eval_inner(e)?;
        Ok(self.coerce(e, v))
    }

    fn lit(&self, e: ExprId, lit: Lit) -> V {
        let p = self.prim_type(self.ty_of(e));
        match (lit, p) {
            (Lit::Bool(b), _) => V::Bool(b),
            (Lit::Char(c), _) => V::Char(u32::from(c)),
            (Lit::Int(i), Some(Prim::F32)) => V::F32(int(i) as f32),
            (Lit::Int(i), Some(Prim::F64)) => V::F64(int(i) as f64),
            (Lit::Int(i), Some(p)) => V::Int(int(i), p),
            (Lit::Float(f), Some(Prim::F32)) => V::F32(f.value() as f32),
            (Lit::Float(f), _) => V::F64(f.value()),
            other => panic!("literal {other:?}"),
        }
    }

    fn eval_inner(&mut self, e: ExprId) -> R<V> {
        let hir = self.hir;
        match *hir.expr(e) {
            Expr::Lit(l) => Ok(self.lit(e, l)),
            Expr::Path(p) => match hir.path(p).res {
                Res::Local(_) => {
                    let pr = self.place(e)?;
                    Ok(self.read(&pr))
                }
                Res::Def(d) => {
                    let Def::Item(item) = d.def() else {
                        panic!("variant")
                    };
                    match hir.item(item).kind {
                        ItemKind::Const { value: Some(v), .. } => self.eval(v),
                        ItemKind::Global { .. } => Ok(self.globals[&item].clone()),
                        ItemKind::Record(_) => Ok(V::Agg(Vec::new())),
                        _ => panic!("path"),
                    }
                }
                _ => panic!("path"),
            },
            Expr::Tuple(l) | Expr::Array(l) => {
                if l.is_empty() {
                    return Ok(V::Unit);
                }
                let mut out = Vec::new();
                for &x in hir.list(l) {
                    out.push(self.eval(x)?);
                }
                Ok(V::Agg(out))
            }
            Expr::Repeat { elem, .. } => {
                let v = self.eval(elem)?;
                let TT::Array { len, .. } = self.typed.types().get(self.ty_of(e)) else {
                    panic!("array");
                };
                Ok(V::Agg(vec![v; len as usize]))
            }
            Expr::Record { path, fields, base } => {
                let mut vals = Vec::new();
                for fi in hir.list(fields) {
                    vals.push((fi.name.sym, self.eval(fi.value)?));
                }
                let base = match base {
                    Some(b) => Some(self.eval(b)?),
                    None => None,
                };
                let item = self.record_item(path.expect("named record"));
                let names = self.field_names(item);
                let mut out = match base {
                    Some(V::Agg(v)) => v,
                    _ => vec![V::Unit; names.len()],
                };
                for (sym, v) in vals {
                    let k = names.iter().position(|&n| n == sym).expect("field");
                    out[k] = v;
                }
                Ok(V::Agg(out))
            }
            Expr::Call { callee, args } => {
                let mut vals = Vec::new();
                for a in hir.list(args) {
                    vals.push(self.eval(a.value)?);
                }
                let Expr::Path(p) = *hir.expr(callee) else {
                    panic!("callee")
                };
                match hir.path(p).res {
                    Res::Def(d) => {
                        let Def::Item(item) = d.def() else {
                            panic!("callee")
                        };
                        match hir.item(item).kind {
                            ItemKind::Fn(_) => self.call(item, vals),
                            ItemKind::Record(_) => Ok(V::Agg(vals)),
                            _ => panic!("callee"),
                        }
                    }
                    Res::Extern(sym) => {
                        let name = self
                            .names
                            .resolve_with(sym, |s| s.to_string())
                            .expect("name");
                        self.host(&name, vals)
                    }
                    _ => panic!("callee"),
                }
            }
            Expr::Field { .. } | Expr::Index { .. } => {
                let pr = self.place(e)?;
                Ok(self.read(&pr))
            }
            Expr::Op { op, args } => {
                let mut vals = Vec::new();
                for &a in hir.list(args) {
                    vals.push(self.eval(a)?);
                }
                self.op(op, vals)
            }
            Expr::Cast { expr, policy, .. } => {
                let v = self.eval(expr)?;
                let to = self.ty_of(e);
                self.cast(v, to, policy)
            }
            Expr::Assign { target, op, value } => {
                let pr = self.place(target)?;
                let v = match op {
                    Some(op) => {
                        let old = self.read(&pr);
                        let rhs = self.eval(value)?;
                        self.op(op, vec![old, rhs])?
                    }
                    None => self.eval(value)?,
                };
                self.write(&pr, v.clone());
                Ok(v)
            }
            Expr::LetPlace {
                binder,
                place,
                body,
            } => {
                let pr = self.place(place)?;
                let _ = self.aliases.last_mut().unwrap().insert(binder, pr);
                self.eval(body)
            }
            Expr::Block(b) => {
                let r = self.block(b.stmts, b.tail);
                match (r, b.label) {
                    (Err(Flow::Break(Some(l), v)), Some(label)) if l == label => Ok(v),
                    (r, _) => r,
                }
            }
            Expr::If { cond, then, else_ } => {
                let c = self.eval(cond)?;
                if c == V::Bool(true) {
                    self.eval(then)
                } else if let Some(o) = else_ {
                    self.eval(o)
                } else {
                    Ok(V::Unit)
                }
            }
            Expr::Match { scrutinee, arms } => {
                let s = self.eval(scrutinee)?;
                for arm in hir.list(arms) {
                    let Some(binds) = self.matches(arm.pat, &s) else {
                        continue;
                    };
                    self.bind_all(binds);
                    if let Some(g) = arm.guard {
                        if self.eval(g)? != V::Bool(true) {
                            continue;
                        }
                    }
                    return self.eval(arm.body);
                }
                Err(self.err(rt::E_NO_MATCH))
            }
            Expr::Loop { label, body, step } => loop {
                match self.eval(body) {
                    Ok(_) => {}
                    Err(Flow::Break(l, v)) if l.is_none() || l == label => return Ok(v),
                    Err(Flow::Continue(l)) if l.is_none() || l == label => {}
                    Err(f) => return Err(f),
                }
                if let Some(s) = step {
                    match self.eval(s) {
                        Ok(_) => {}
                        Err(Flow::Break(l, v)) if l.is_none() || l == label => return Ok(v),
                        Err(f) => return Err(f),
                    }
                }
                self.steps += 1;
                assert!(self.steps < MAX_STEPS, "reference fuel");
            },
            Expr::Break { label, value } => {
                let v = match value {
                    Some(v) => self.eval(v)?,
                    None => V::Unit,
                };
                Err(Flow::Break(label, v))
            }
            Expr::Continue { label } => Err(Flow::Continue(label)),
            Expr::Return(v) => {
                let v = match v {
                    Some(v) => self.eval(v)?,
                    None => V::Unit,
                };
                Err(Flow::Return(v))
            }
            Expr::Throw(v) => {
                let v = self.eval(v)?;
                let p = prim_of(&v).expect("scalar throw");
                Err(Flow::Throw(rt::tag(p).unwrap(), bits_of(&v)))
            }
            Expr::Try {
                body,
                catches,
                finally,
            } => {
                let mut r = self.eval(body);
                if let Err(Flow::Throw(tag, bits)) = r {
                    for arm in hir.list(catches) {
                        let binds = match *hir.pat(arm.pat) {
                            Pat::Wild => Some(Vec::new()),
                            Pat::TypeTest { ty, pat } => {
                                let p = self.ty_prim(ty);
                                if rt::tag(p) == Some(tag) {
                                    let v = from_bits(bits, p);
                                    match pat {
                                        Some(sub) => self.matches(sub, &v),
                                        None => Some(Vec::new()),
                                    }
                                } else {
                                    None
                                }
                            }
                            _ => panic!("catch pattern"),
                        };
                        let Some(binds) = binds else { continue };
                        self.bind_all(binds);
                        if let Some(g) = arm.guard {
                            match self.eval(g) {
                                Ok(V::Bool(true)) => {}
                                Ok(_) => continue,
                                Err(f) => {
                                    r = Err(f);
                                    break;
                                }
                            }
                        }
                        r = self.eval(arm.body);
                        break;
                    }
                }
                if let Some(f) = finally {
                    if matches!(r, Err(Flow::Trap(_))) {
                        return r;
                    }
                    self.eval(f)?;
                }
                r
            }
            other => panic!("not in the subset: {other:?}"),
        }
    }

    fn bind_all(&mut self, binds: Vec<(BinderId, V)>) {
        let f = self.frames.last_mut().unwrap();
        for (b, v) in binds {
            let _ = f.insert(b, v);
        }
    }

    fn block(&mut self, stmts: hir_lang::List<hir_lang::StmtId>, tail: Option<ExprId>) -> R<V> {
        let hir = self.hir;
        let mut defers: Vec<ExprId> = Vec::new();
        let mut result: R<V> = Ok(V::Unit);
        let mut done = false;
        for &s in hir.list(stmts) {
            let r = match *hir.stmt(s) {
                Stmt::Let {
                    pat, init, else_, ..
                } => match init {
                    None => {
                        let Pat::Bind { binder, .. } = *hir.pat(pat) else {
                            panic!("let")
                        };
                        let t = self.typed.binder(binder).expect("typed");
                        let z = self.zero_type(t);
                        self.bind_all(vec![(binder, z)]);
                        Ok(())
                    }
                    Some(init) => match self.eval(init) {
                        Err(f) => Err(f),
                        Ok(v) => match self.matches(pat, &v) {
                            Some(binds) => {
                                self.bind_all(binds);
                                Ok(())
                            }
                            None => match else_ {
                                Some(other) => match self.eval(other) {
                                    Err(f) => Err(f),
                                    Ok(_) => panic!("let-else fell through"),
                                },
                                None => Err(self.err(rt::E_NO_MATCH)),
                            },
                        },
                    },
                },
                Stmt::Expr(x) => self.eval(x).map(|_| ()),
                Stmt::Item(_) => Ok(()),
                Stmt::Defer(x) => {
                    defers.push(x);
                    Ok(())
                }
                _ => panic!("statement"),
            };
            if let Err(f) = r {
                result = Err(f);
                done = true;
                break;
            }
        }
        if !done {
            result = match tail {
                Some(t) => self.eval(t),
                None => Ok(V::Unit),
            };
        }
        // Deferred bodies run last-registered first on every exit but a trap;
        // a body's own jump or exception replaces the pending one.
        if !matches!(result, Err(Flow::Trap(_))) {
            while let Some(d) = defers.pop() {
                if let Err(f) = self.eval(d) {
                    let trap = matches!(f, Flow::Trap(_));
                    result = Err(f);
                    if trap {
                        break;
                    }
                }
            }
        }
        result
    }

    fn record_item(&self, path: hir_lang::PathId) -> ItemId {
        match self.hir.path(path).res {
            Res::Def(d) => match d.def() {
                Def::Item(i) => i,
                Def::Variant(_) => panic!("variant"),
            },
            _ => panic!("record path"),
        }
    }

    fn field_names(&self, item: ItemId) -> Vec<intern_lang::Symbol> {
        let ItemKind::Record(def) = self.hir.item(item).kind else {
            panic!("record");
        };
        self.hir
            .list(def.fields)
            .iter()
            .map(|&f| self.hir.field(f).name.expect("named").sym)
            .collect()
    }

    /// The field index a member names in the aggregate type `t`.
    fn member_index(&self, t: TypeId, member: Member) -> usize {
        match member {
            Member::Index(i) => i as usize,
            Member::Named(sym) => {
                let TT::Adt { item, .. } = self.typed.types().get(t) else {
                    panic!("record type");
                };
                self.field_names(item)
                    .iter()
                    .position(|&n| n == sym)
                    .expect("field")
            }
        }
    }

    fn place(&mut self, e: ExprId) -> R<PRef> {
        let hir = self.hir;
        match *hir.expr(e) {
            Expr::Path(p) => match hir.path(p).res {
                Res::Local(b) => {
                    if let Some(a) = self.aliases.last().unwrap().get(&b) {
                        return Ok(a.clone());
                    }
                    Ok(PRef::Local(b, Vec::new()))
                }
                Res::Def(d) => {
                    let Def::Item(item) = d.def() else {
                        panic!("variant")
                    };
                    match hir.item(item).kind {
                        ItemKind::Global { .. } => Ok(PRef::Global(item, Vec::new())),
                        _ => {
                            let v = self.eval(e)?;
                            Ok(PRef::Temp(v, Vec::new()))
                        }
                    }
                }
                _ => panic!("place"),
            },
            Expr::Field { base, member, .. } => {
                let bt = self.ty_of(base);
                let k = self.member_index(bt, member);
                let mut pr = self.place(base)?;
                path_of(&mut pr).push(k);
                Ok(pr)
            }
            Expr::Index { base, index } => {
                let mut pr = self.place(base)?;
                let i = self.eval(index)?;
                let V::Int(i, _) = i else { panic!("index") };
                let len = match self.read(&pr) {
                    V::Agg(v) => v.len() as i128,
                    other => panic!("array {other:?}"),
                };
                if i < 0 || i >= len {
                    return Err(self.err(rt::E_INDEX_OUT_OF_BOUNDS));
                }
                path_of(&mut pr).push(i as usize);
                Ok(pr)
            }
            _ => {
                let v = self.eval(e)?;
                Ok(PRef::Temp(v, Vec::new()))
            }
        }
    }

    fn read(&self, pr: &PRef) -> V {
        let (root, path) = match pr {
            PRef::Local(b, p) => (
                self.frames
                    .last()
                    .unwrap()
                    .get(b)
                    .cloned()
                    .unwrap_or(V::Unit),
                p,
            ),
            PRef::Global(i, p) => (self.globals[i].clone(), p),
            PRef::Temp(v, p) => (v.clone(), p),
        };
        let mut v = root;
        for &k in path {
            v = match v {
                V::Agg(mut xs) => xs.swap_remove(k),
                other => other,
            };
        }
        v
    }

    fn write(&mut self, pr: &PRef, value: V) {
        let (root, path): (Option<&mut V>, &Vec<usize>) = match pr {
            PRef::Local(b, p) => {
                if p.is_empty() {
                    let _ = self.frames.last_mut().unwrap().insert(*b, value);
                    return;
                }
                (self.frames.last_mut().unwrap().get_mut(b), p)
            }
            PRef::Global(i, p) => (self.globals.get_mut(i), p),
            // Writing into a temporary changes nothing observable.
            PRef::Temp(..) => return,
        };
        let Some(mut v) = root else { return };
        for &k in path {
            v = match v {
                V::Agg(xs) => &mut xs[k],
                other => other,
            };
        }
        *v = value;
    }

    /// Matches a pattern, returning its bindings.
    fn matches(&mut self, p: PatId, v: &V) -> Option<Vec<(BinderId, V)>> {
        let hir = self.hir;
        match *hir.pat(p) {
            Pat::Wild => Some(Vec::new()),
            Pat::Bind { binder, sub, .. } => {
                let mut out = match sub {
                    Some(s) => self.matches(s, v)?,
                    None => Vec::new(),
                };
                out.push((binder, v.clone()));
                Some(out)
            }
            Pat::Ident { binder, path } => match hir.path(path).res {
                Res::Def(d) => self.def_matches(d, v).then(Vec::new),
                _ => Some(vec![(binder, v.clone())]),
            },
            Pat::Path(path) => match hir.path(path).res {
                Res::Def(d) => self.def_matches(d, v).then(Vec::new),
                _ => panic!("path pattern"),
            },
            Pat::Lit(l) => {
                let c = lit_like(l, v);
                eq(v, &c).then(Vec::new)
            }
            Pat::Range { lo, hi, inclusive } => {
                use std::cmp::Ordering::{Equal, Less};
                if let Some(lo) = lo {
                    let b = self.bound(lo, v);
                    if cmp(v, &b).is_none_or(|o| o == Less) {
                        return None;
                    }
                }
                if let Some(hi) = hi {
                    let b = self.bound(hi, v);
                    if !cmp(v, &b).is_some_and(|o| o == Less || (inclusive && o == Equal)) {
                        return None;
                    }
                }
                Some(Vec::new())
            }
            Pat::Tuple { elems, rest } | Pat::Ctor { elems, rest, .. } => {
                let elems = hir.list(elems);
                let xs = match v {
                    V::Agg(xs) => xs.clone(),
                    V::Unit => Vec::new(),
                    _ => panic!("tuple subject"),
                };
                let (n, m) = (xs.len(), elems.len());
                let mut out = Vec::new();
                for (k, &el) in elems.iter().enumerate() {
                    let f = match rest {
                        Some(r) if k >= r as usize => n - m + k,
                        _ => k,
                    };
                    out.extend(self.matches(el, &xs[f])?);
                }
                Some(out)
            }
            Pat::Record { path, fields, .. } => {
                let V::Agg(xs) = v else {
                    panic!("record subject")
                };
                let item = self.record_item(path.expect("named"));
                let names = self.field_names(item);
                let mut out = Vec::new();
                for fp in hir.list(fields) {
                    let k = names.iter().position(|&n| n == fp.name.sym).expect("field");
                    out.extend(self.matches(fp.pat, &xs[k].clone())?);
                }
                Some(out)
            }
            Pat::Or(alts) => {
                for &a in hir.list(alts) {
                    if let Some(b) = self.matches(a, v) {
                        return Some(b);
                    }
                }
                None
            }
            other => panic!("pattern {other:?}"),
        }
    }

    fn def_matches(&mut self, d: hir_lang::DefId, v: &V) -> bool {
        let Def::Item(item) = d.def() else {
            panic!("variant")
        };
        match self.hir.item(item).kind {
            ItemKind::Const { value: Some(c), .. } => {
                let c = self.eval(c).expect("constant");
                eq(v, &c)
            }
            ItemKind::Record(_) => true,
            _ => panic!("pattern def"),
        }
    }

    fn bound(&mut self, p: PatId, like: &V) -> V {
        match *self.hir.pat(p) {
            Pat::Lit(l) => lit_like(l, like),
            Pat::Path(path) => match self.hir.path(path).res {
                Res::Def(d) => {
                    let Def::Item(item) = d.def() else {
                        panic!("variant")
                    };
                    let ItemKind::Const { value: Some(c), .. } = self.hir.item(item).kind else {
                        panic!("const");
                    };
                    self.eval(c).expect("constant")
                }
                _ => panic!("bound"),
            },
            _ => panic!("bound"),
        }
    }

    fn host(&mut self, name: &str, args: Vec<V>) -> R<V> {
        match name {
            "obj_new" => {
                let V::Int(x, _) = args[0] else { panic!("arg") };
                self.objects.push(x as i64);
                Ok(V::Obj(self.objects.len() - 1))
            }
            "obj_get" => {
                let V::Obj(i) = args[0] else { panic!("arg") };
                Ok(V::Int(i128::from(self.objects[i]), Prim::I64))
            }
            "obj_set" => {
                let (V::Obj(i), V::Int(x, _)) = (&args[0], &args[1]) else {
                    panic!("arg")
                };
                self.objects[*i] = *x as i64;
                Ok(V::Unit)
            }
            "gc_poll" => Ok(V::Unit),
            "trace" => {
                let V::Int(x, _) = args[0] else { panic!("arg") };
                self.trace.push(x as i64);
                Ok(V::Unit)
            }
            "host_throw" => {
                let V::Int(x, _) = args[0] else { panic!("arg") };
                Err(Flow::Throw(rt::tag(Prim::I64).unwrap(), x as u64))
            }
            "host_check" => {
                let V::Int(x, _) = args[0] else { panic!("arg") };
                if (x as i64) % 3 == 0 {
                    return Err(Flow::Throw(rt::tag(Prim::I64).unwrap(), x as i64 as u64));
                }
                Ok(V::Unit)
            }
            other => panic!("host function {other}"),
        }
    }

    // ------------------------------------------------------------------ OPS

    /// An exact integer result in `p`, or the overflow policy's outcome.
    fn int_result(&self, exact: i128, p: Prim, o: hir_lang::Overflow) -> R<V> {
        let (lo, hi) = range(p);
        if (lo..=hi).contains(&exact) {
            return Ok(V::Int(exact, p));
        }
        match o {
            hir_lang::Overflow::Wrap => Ok(V::Int(wrap(exact, p), p)),
            hir_lang::Overflow::Trap => Err(Flow::Trap(1)),
            _ => Err(self.err(1)),
        }
    }

    fn op(&mut self, op: Op, args: Vec<V>) -> R<V> {
        use OpKind as K;
        let pol = op.policy;
        let ov = pol.overflow.unwrap_or(hir_lang::Overflow::Error);
        match (op.kind, args.as_slice()) {
            (K::Add, [V::Int(a, p), V::Int(b, _)]) => self.int_result(a + b, *p, ov),
            (K::Sub, [V::Int(a, p), V::Int(b, _)]) => self.int_result(a - b, *p, ov),
            (K::Mul, [V::Int(a, p), V::Int(b, _)]) => match a.checked_mul(*b) {
                Some(x) => self.int_result(x, *p, ov),
                // Beyond i128 (u64 operands): out of range; wrap keeps the
                // low bits, which wrapping multiplication preserves.
                None => match ov {
                    hir_lang::Overflow::Wrap => Ok(V::Int(wrap(a.wrapping_mul(*b), *p), *p)),
                    hir_lang::Overflow::Trap => Err(Flow::Trap(1)),
                    _ => Err(self.err(1)),
                },
            },
            (K::Div | K::FloorDiv, [V::Int(a, p), V::Int(b, _)]) => {
                if *b == 0 {
                    let trap = pol.div_zero == Some(hir_lang::DivZero::Trap);
                    return Err(self.fail(2, trap));
                }
                let q = if op.kind == K::Div {
                    a / b
                } else {
                    floor_div(*a, *b)
                };
                self.int_result(q, *p, ov)
            }
            (K::Rem | K::FloorMod, [V::Int(a, p), V::Int(b, _)]) => {
                if *b == 0 {
                    let trap = pol.div_zero == Some(hir_lang::DivZero::Trap);
                    return Err(self.fail(2, trap));
                }
                let r = if op.kind == K::Rem {
                    a % b
                } else {
                    a - b * floor_div(*a, *b)
                };
                Ok(V::Int(r, *p))
            }
            (K::Shl | K::Shr, [V::Int(a, p), V::Int(n, _)]) => {
                let w = i128::from(width(*p));
                let sh = pol.shift.unwrap_or(hir_lang::Shift::Error);
                let mut n = *n;
                if n < 0 || n >= w {
                    match sh {
                        hir_lang::Shift::Error => return Err(self.err(3)),
                        hir_lang::Shift::Mask => n = n.rem_euclid(w),
                        hir_lang::Shift::Saturate => {
                            if n < 0 {
                                return Err(self.err(3));
                            }
                            let v = if op.kind == K::Shl || *a >= 0 { 0 } else { -1 };
                            return Ok(V::Int(v, *p));
                        }
                    }
                }
                let v = if op.kind == K::Shl {
                    wrap(a << n, *p)
                } else {
                    a >> n
                };
                Ok(V::Int(v, *p))
            }
            (K::Pow, [V::Int(a, p), V::Int(e, _)]) => {
                if *e < 0 {
                    // OPS v2: NegativeExponent under every policy but
                    // `promote`, `trap` included.
                    return Err(self.err(6));
                }
                let (lo, hi) = range(*p);
                match exact_pow(*a, *e) {
                    Some(x) if (lo..=hi).contains(&x) => Ok(V::Int(x, *p)),
                    _ => match ov {
                        hir_lang::Overflow::Wrap => Ok(V::Int(wrap_pow(*a, *e, *p), *p)),
                        hir_lang::Overflow::Trap => Err(Flow::Trap(1)),
                        _ => Err(self.err(1)),
                    },
                }
            }
            (K::Neg, [V::Int(a, p)]) => self.int_result(-a, *p, ov),
            (K::Abs, [V::Int(a, p)]) => self.int_result(a.abs(), *p, ov),
            (K::Min, [V::Int(a, p), V::Int(b, _)]) => Ok(V::Int(*a.min(b), *p)),
            (K::Max, [V::Int(a, p), V::Int(b, _)]) => Ok(V::Int(*a.max(b), *p)),
            (K::And, [V::Int(a, p), V::Int(b, _)]) => Ok(V::Int(wrap(a & b, *p), *p)),
            (K::Or, [V::Int(a, p), V::Int(b, _)]) => Ok(V::Int(wrap(a | b, *p), *p)),
            (K::Xor, [V::Int(a, p), V::Int(b, _)]) => Ok(V::Int(wrap(a ^ b, *p), *p)),
            (K::BitNot, [V::Int(a, p)]) => Ok(V::Int(wrap(!a, *p), *p)),
            (K::And, [V::Bool(a), V::Bool(b)]) => Ok(V::Bool(*a && *b)),
            (K::Or, [V::Bool(a), V::Bool(b)]) => Ok(V::Bool(*a || *b)),
            (K::Xor, [V::Bool(a), V::Bool(b)]) => Ok(V::Bool(a != b)),
            (K::Not, [V::Bool(a)]) => Ok(V::Bool(!a)),
            (K::Eq | K::Ne | K::Lt | K::Le | K::Gt | K::Ge, [a, b]) => {
                use std::cmp::Ordering::{Equal, Greater, Less};
                let o = cmp(a, b);
                Ok(V::Bool(match op.kind {
                    K::Eq => o == Some(Equal),
                    K::Ne => o != Some(Equal),
                    K::Lt => o == Some(Less),
                    K::Le => matches!(o, Some(Less | Equal)),
                    K::Gt => o == Some(Greater),
                    _ => matches!(o, Some(Greater | Equal)),
                }))
            }
            (K::TotalCmp, [V::F64(a), V::F64(b)]) => Ok(V::Int(a.total_cmp(b) as i128, Prim::I8)),
            (K::TotalCmp, [V::F32(a), V::F32(b)]) => Ok(V::Int(a.total_cmp(b) as i128, Prim::I8)),
            (K::Fma, [V::F64(a), V::F64(b), V::F64(c)]) => Ok(V::F64(a.mul_add(*b, *c))),
            (K::Fma, [V::F32(a), V::F32(b), V::F32(c)]) => Ok(V::F32(a.mul_add(*b, *c))),
            (k, [V::F64(a), V::F64(b)]) => Ok(V::F64(fbin(k, *a, *b))),
            (k, [V::F32(a), V::F32(b)]) => Ok(V::F32(fbin32(k, *a, *b))),
            (K::IntCast(t), [v]) => self.int_result(int_of(v), t, ov),
            (K::Zext(t), [v]) => Ok(V::Int(wrap(i128::from(bits_of(v)), t), t)),
            (K::Sext(t), [V::Int(x, p)]) => {
                // Sign-extend the source's bit pattern from its width.
                let w = width(*p);
                let b = i128::from(bits_of(&V::Int(*x, *p)));
                let sv = if b >= (1i128 << (w - 1)) {
                    b - (1i128 << w)
                } else {
                    b
                };
                Ok(V::Int(wrap(sv, t), t))
            }
            (K::Narrow(t), [V::Int(x, _)]) => Ok(V::Int(wrap(*x, t), t)),
            (K::IntToFloat(t), [V::Int(x, _)]) => Ok(if t == Prim::F32 {
                V::F32(*x as f32)
            } else {
                V::F64(*x as f64)
            }),
            (K::FloatToInt(t), [v]) => {
                let f = match v {
                    V::F32(f) => f64::from(*f),
                    V::F64(f) => *f,
                    _ => panic!("float"),
                };
                self.float_to_int(
                    f,
                    t,
                    pol.float_to_int.unwrap_or(hir_lang::FloatToInt::Error),
                )
            }
            (K::Bitcast(t), [v]) => Ok(from_bits(bits_of(v), t)),
            (K::BoolToInt(t), [V::Bool(b)]) => Ok(V::Int(i128::from(*b), t)),
            (K::F32ToF64, [V::F32(f)]) => Ok(V::F64(f64::from(*f))),
            (K::F64ToF32, [V::F64(f)]) => Ok(V::F32(*f as f32)),
            (K::CharFromU32, [V::Int(x, _)]) => {
                if char::from_u32(*x as u32).is_some() {
                    Ok(V::Char(*x as u32))
                } else {
                    Err(self.err(5))
                }
            }
            (k, [V::F64(a)]) => Ok(V::F64(fun(k, *a))),
            (k, [V::F32(a)]) => Ok(V::F32(fun32(k, *a))),
            (k, a) => panic!("op {k:?} on {a:?}"),
        }
    }

    fn float_to_int(&self, f: f64, t: Prim, p: hir_lang::FloatToInt) -> R<V> {
        let (lo, hi) = range(t);
        let tr = f.trunc();
        // Every bound here is a power of two or one less; compare in i128
        // after checking the magnitude is below 2^65 so the cast is exact.
        let inside = !f.is_nan() && tr.abs() < 3.7e19 && {
            let x = tr as i128;
            (lo..=hi).contains(&x)
        };
        if inside {
            return Ok(V::Int(tr as i128, t));
        }
        match p {
            hir_lang::FloatToInt::Saturate => Ok(V::Int(
                if f.is_nan() {
                    0
                } else if tr < 0.0 {
                    lo
                } else {
                    hi
                },
                t,
            )),
            hir_lang::FloatToInt::Error => Err(self.err(4)),
        }
    }

    fn cast(&self, v: V, to: TypeId, pol: hir_lang::Policy) -> R<V> {
        let Some(t) = self.prim_type(to) else {
            return Ok(v);
        };
        let o = pol.overflow.unwrap_or(hir_lang::Overflow::Error);
        let fti = pol.float_to_int.unwrap_or(hir_lang::FloatToInt::Error);
        match (v, t) {
            (V::Int(x, Prim::U8), Prim::Char) => Ok(V::Char(x as u32)),
            (V::Int(x, _), Prim::F32) => Ok(V::F32(x as f32)),
            (V::Int(x, _), Prim::F64) => Ok(V::F64(x as f64)),
            (V::Int(x, _), t) => self.int_result(x, t, o),
            (V::F32(f), Prim::F64) => Ok(V::F64(f64::from(f))),
            (V::F64(f), Prim::F32) => Ok(V::F32(f as f32)),
            (V::F32(f), Prim::F32) => Ok(V::F32(f)),
            (V::F64(f), Prim::F64) => Ok(V::F64(f)),
            (V::F32(f), t) => self.float_to_int(f64::from(f), t, fti),
            (V::F64(f), t) => self.float_to_int(f, t, fti),
            (V::Bool(b), Prim::Bool) => Ok(V::Bool(b)),
            (V::Bool(b), t) => Ok(V::Int(i128::from(b), t)),
            (V::Char(c), Prim::Char) => Ok(V::Char(c)),
            (V::Char(c), t) => self.int_result(i128::from(c), t, o),
            (other, _) => Ok(other),
        }
    }
}

fn int(i: hir_lang::IntLit) -> i128 {
    if i.negative {
        -i128::from(i.value)
    } else {
        i128::from(i.value)
    }
}

fn int_of(v: &V) -> i128 {
    match v {
        V::Int(x, _) => *x,
        V::Char(c) => i128::from(*c),
        V::Bool(b) => i128::from(*b),
        _ => panic!("int"),
    }
}

fn floor_div(a: i128, b: i128) -> i128 {
    let q = a / b;
    if (a % b != 0) && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// The exact power, or `None` when it leaves `i128` (then it is out of every
/// type's range too).
fn exact_pow(a: i128, e: i128) -> Option<i128> {
    match a {
        0 => return Some(i128::from(e == 0)),
        1 => return Some(1),
        -1 => return Some(if e % 2 == 0 { 1 } else { -1 }),
        _ => {}
    }
    if e > 127 {
        return None;
    }
    a.checked_pow(e as u32)
}

/// The low bits of the exact power `a^e` in `p` (squaring modulo 2^width).
fn wrap_pow(a: i128, e: i128, p: Prim) -> i128 {
    let w = width(p);
    let m: u128 = if w == 64 {
        u128::from(u64::MAX)
    } else {
        (1u128 << w) - 1
    };
    let mut base = (a as u128) & m;
    let mut e = e as u128;
    let mut acc: u128 = 1;
    while e > 0 {
        if e & 1 == 1 {
            acc = acc.wrapping_mul(base) & m;
        }
        base = base.wrapping_mul(base) & m;
        e >>= 1;
    }
    wrap(acc as i128, p)
}

fn cmp(a: &V, b: &V) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (V::Int(x, _), V::Int(y, _)) => Some(x.cmp(y)),
        (V::Bool(x), V::Bool(y)) => Some(x.cmp(y)),
        (V::Char(x), V::Char(y)) => Some(x.cmp(y)),
        (V::F64(x), V::F64(y)) => x.partial_cmp(y),
        (V::F32(x), V::F32(y)) => x.partial_cmp(y),
        _ => None,
    }
}

fn eq(a: &V, b: &V) -> bool {
    cmp(a, b) == Some(std::cmp::Ordering::Equal)
}

/// A literal pattern as a value of the subject's type.
fn lit_like(l: Lit, like: &V) -> V {
    match (l, like) {
        (Lit::Int(i), V::Int(_, p)) => V::Int(int(i), *p),
        (Lit::Int(i), V::F64(_)) => V::F64(int(i) as f64),
        (Lit::Int(i), V::F32(_)) => V::F32(int(i) as f32),
        (Lit::Float(f), V::F32(_)) => V::F32(f.value() as f32),
        (Lit::Float(f), _) => V::F64(f.value()),
        (Lit::Bool(b), _) => V::Bool(b),
        (Lit::Char(c), _) => V::Char(u32::from(c)),
        other => panic!("literal pattern {other:?}"),
    }
}

fn fbin(k: OpKind, a: f64, b: f64) -> f64 {
    match k {
        OpKind::Add => a + b,
        OpKind::Sub => a - b,
        OpKind::Mul => a * b,
        OpKind::Div => a / b,
        OpKind::Rem => a % b,
        OpKind::Min => fmin(a, b),
        OpKind::Max => fmax(a, b),
        OpKind::Pow => ir_lang::ls_pow(a, b),
        other => panic!("float op {other:?}"),
    }
}

fn fbin32(k: OpKind, a: f32, b: f32) -> f32 {
    match k {
        OpKind::Add => a + b,
        OpKind::Sub => a - b,
        OpKind::Mul => a * b,
        OpKind::Div => a / b,
        OpKind::Rem => a % b,
        OpKind::Min => fmin(f64::from(a), f64::from(b)) as f32,
        OpKind::Max => fmax(f64::from(a), f64::from(b)) as f32,
        OpKind::Pow => ir_lang::ls_pow(f64::from(a), f64::from(b)) as f32,
        other => panic!("float op {other:?}"),
    }
}

fn fun(k: OpKind, a: f64) -> f64 {
    match k {
        OpKind::Neg => -a,
        OpKind::Abs => a.abs(),
        OpKind::Sqrt => a.sqrt(),
        OpKind::Floor => a.floor(),
        OpKind::Ceil => a.ceil(),
        OpKind::Trunc => a.trunc(),
        OpKind::Round => a.round(),
        OpKind::RoundEven => a.round_ties_even(),
        other => panic!("float op {other:?}"),
    }
}

fn fun32(k: OpKind, a: f32) -> f32 {
    match k {
        OpKind::Neg => -a,
        OpKind::Abs => a.abs(),
        OpKind::Sqrt => a.sqrt(),
        OpKind::Floor => a.floor(),
        OpKind::Ceil => a.ceil(),
        OpKind::Trunc => a.trunc(),
        OpKind::Round => a.round(),
        OpKind::RoundEven => a.round_ties_even(),
        other => panic!("float op {other:?}"),
    }
}

fn path_of(pr: &mut PRef) -> &mut Vec<usize> {
    match pr {
        PRef::Local(_, p) | PRef::Global(_, p) | PRef::Temp(_, p) => p,
    }
}

/// The reference outcome of calling `item` with scalar `args`, and the
/// host trace.
pub fn run(
    u: &Unit,
    typed: &Typed,
    mode: ErrorMode,
    item: ItemId,
    args: &[Val],
) -> (Out, Vec<i64>) {
    let mut ev = Eval::new(&u.hir, typed, &u.names, mode);
    let ItemKind::Fn(def) = u.hir.item(item).kind else {
        panic!("fn")
    };
    let mut vals = Vec::new();
    for (&p, a) in u.hir.list(def.params).iter().zip(args) {
        let pat = u.hir.param(p).pat;
        let t = typed.pat(pat).expect("typed");
        let prim = match typed.types().get(t) {
            TT::Prim(p) => p,
            other => panic!("scalar parameters only: {other:?}"),
        };
        vals.push(from_bits(a.bits(), prim));
    }
    let r = ev.call(item, vals);
    let out = match r {
        Ok(V::Unit) => Out::Ret(None),
        Ok(other) => Out::Ret(Some(bits_of(&other))),
        Err(Flow::Throw(tag, bits)) => Out::Thrown { tag, bits },
        Err(Flow::Trap(c)) => Out::Trap(c),
        Err(other) => panic!("escaped flow {other:?}"),
    };
    (out, ev.trace)
}

/// The float type a function returns, for NaN-tolerant comparison.
pub fn float_ret(u: &Unit, item: ItemId) -> Option<Prim> {
    let ItemKind::Fn(def) = u.hir.item(item).kind else {
        return None;
    };
    let ty = def.ret?;
    match *u.hir.ty(ty) {
        Ty::Prim(p @ (Prim::F32 | Prim::F64)) => Some(p),
        _ => None,
    }
}

/// Checks the lowered run against the reference evaluator, in both error
/// modes, including the host-visible trace.
pub fn agree(u: &Unit, name: &str, args: &[Val]) {
    let typed = u.check();
    agree_typed(u, &typed, u.item(name), args);
}

pub fn agree_typed(u: &Unit, typed: &Typed, item: ItemId, args: &[Val]) {
    for mode in [ErrorMode::Raise, ErrorMode::Trap] {
        let out = u.lower_with(typed, irgen_lang::Options::new().with_errors(mode));
        let (got, trace) = super::run_lowered_traced(&out, item, args);
        let (want, want_trace) = run(u, typed, mode, item, args);
        assert!(
            got.same(&want, float_ret(u, item)),
            "{item:?}{args:?} in {mode:?}: lowered {got:?}, reference {want:?}\n{}",
            out.module()
        );
        assert_eq!(trace, want_trace, "{item:?}{args:?}: host trace");
    }
}
