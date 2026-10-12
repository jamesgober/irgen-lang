//! Random well-typed programs of the subset, for differential tests.
//!
//! A seed drives a small deterministic generator that builds HIR directly:
//! functions calling earlier functions, scalar and aggregate locals and
//! parameters, every OPS operation with random policies (mixed-type shifts
//! included), conversions and casts, `if`, counted loops with labeled
//! `break`/`continue`, labeled blocks with break values, `match` over
//! integers, records, tuples, and arrays with field and bounds-checked index
//! access, compound assignment and `let_place`, `throw`, `try`/`catch`
//! /`finally`, `defer`, host calls with an observable trace, host functions
//! that unwind, and host GC references live across calls. Every loop is
//! counted, so every program terminates.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use hir_lang::{
    BinderId, DivZero, ExprId, FloatToInt, ItemId, Lit, Op, OpKind, Overflow, Pat, Policy, Prim,
    Shift, StmtId, TyId,
};

use super::{Hb, HostTy, Unit};

/// A deterministic random source (xorshift64*).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        // splitmix64, so nearby seeds start far apart.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Rng((z ^ (z >> 31)) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }

    pub fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

/// The types generated programs use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum T {
    I64,
    I32,
    U8,
    Bool,
    F64,
    /// `(i64, bool)`
    Tup,
    /// `Point { x: i64, y: i64 }`
    Point,
    /// `[i64; 4]`
    Arr,
    /// A host GC reference.
    Obj,
}

const INTS: [T; 3] = [T::I64, T::I32, T::U8];

impl T {
    fn prim(self) -> Option<Prim> {
        match self {
            T::I64 => Some(Prim::I64),
            T::I32 => Some(Prim::I32),
            T::U8 => Some(Prim::U8),
            T::Bool => Some(Prim::Bool),
            T::F64 => Some(Prim::F64),
            _ => None,
        }
    }

    fn is_int(self) -> bool {
        matches!(self, T::I64 | T::I32 | T::U8)
    }
}

fn width(t: T) -> u32 {
    match t {
        T::U8 => 8,
        T::I32 => 32,
        _ => 64,
    }
}

#[derive(Clone, Debug)]
struct FnSig {
    name: String,
    params: Vec<T>,
    ret: T,
}

#[derive(Clone, Copy, Debug)]
struct LoopCx {
    label: BinderId,
    /// The value type of `break` (`None` for a unit loop).
    val: Option<T>,
    defer_depth: u32,
}

#[derive(Clone, Copy, Debug)]
struct BlockCx {
    label: BinderId,
    val: T,
    defer_depth: u32,
}

pub struct Gen {
    pub h: Hb,
    r: Rng,
    scope: Vec<(BinderId, T)>,
    loops: Vec<LoopCx>,
    blocks: Vec<BlockCx>,
    in_defer: u32,
    fns: Vec<FnSig>,
    ret: T,
    /// Nodes left for this function (keeps programs small).
    budget: i64,
    pub with_objects: bool,
    /// Only `i64`, `bool`, and `f64`: the programs whose static semantics
    /// equal the dynamic profile's (for the bytecode-path differential).
    pub scalar: bool,
    /// Float values and operations. Off for the native end-to-end test:
    /// backend-lang 0.4 refuses float arithmetic, comparisons, and
    /// int/float conversions (isa-lang has no SSE yet, ISSUES P11). With it
    /// on, the generator draws exactly as it did in 0.2.
    pub floats: bool,
}

/// A generated program: the unit, and the entry's arguments.
pub struct Program {
    pub unit: Unit,
    pub entry: ItemId,
    pub args: Vec<i64>,
}

/// The program for `seed`; `size` scales the node budget per function.
pub fn program(seed: u64, size: i64) -> Program {
    program_with(seed, None, size)
}

/// A program with `fns` functions before `main` (random 1..=3 if `None`).
pub fn program_with(seed: u64, fns: Option<u64>, size: i64) -> Program {
    generate(seed, fns, size, false)
}

/// A program over `i64`, `bool`, and `f64` only, with no host calls but
/// `trace`: the same meaning under the static and the dynamic profile.
pub fn scalar_program(seed: u64, size: i64) -> Program {
    generate(seed, None, size, true)
}

/// A program the native backend accepts: no float values or operations,
/// and GC references only if `objects`.
pub fn native_program(seed: u64, size: i64, objects: bool) -> Program {
    generate_with(seed, None, size, false, objects, false)
}

fn generate(seed: u64, fns: Option<u64>, size: i64, scalar: bool) -> Program {
    generate_with(seed, fns, size, scalar, !scalar, true)
}

fn generate_with(
    seed: u64,
    fns: Option<u64>,
    size: i64,
    scalar: bool,
    objects: bool,
    floats: bool,
) -> Program {
    let mut g = Gen {
        h: Hb::new(),
        r: Rng::new(seed),
        scope: Vec::new(),
        loops: Vec::new(),
        blocks: Vec::new(),
        in_defer: 0,
        fns: Vec::new(),
        ret: T::I64,
        budget: 0,
        with_objects: objects,
        scalar,
        floats,
    };
    g.declarations();
    let n = fns.unwrap_or_else(|| 1 + g.r.below(3));
    for k in 0..n {
        let _ = g.function(&format!("f{k}"), false, size);
    }
    let main = g.function("main", true, size);
    let args = vec![g.int_value(T::I64), g.int_value(T::I64)];
    let mut unit = g.h.finish();
    unit.profile = typeck_lang::Profile::static_explicit().with_exhaustive_match(false);
    Program {
        unit,
        entry: main,
        args,
    }
}

impl Gen {
    fn declarations(&mut self) {
        let x = self.h.prim(Prim::I64);
        let y = self.h.prim(Prim::I64);
        let _ = self.h.record("Point", &[("x", x), ("y", y)]);
        // const K: i64 = <lit>;  global G: i64 = <lit> (mutable)
        let ty = self.h.prim(Prim::I64);
        let v = self.int_lit(T::I64);
        let k = self.h.name("K");
        let kid = self.h.item(hir_lang::Item::new(
            Some(k),
            hir_lang::ItemKind::Const {
                ty: Some(ty),
                value: Some(v),
            },
        ));
        self.h.named("K", kid);
        let ty = self.h.prim(Prim::I64);
        let v = self.int_lit(T::I64);
        let gname = self.h.name("G");
        let gid = self.h.item(hir_lang::Item::new(
            Some(gname),
            hir_lang::ItemKind::Global {
                ty: Some(ty),
                mutable: true,
                init: Some(v),
            },
        ));
        self.h.named("G", gid);
    }

    fn ty(&mut self, t: T) -> TyId {
        match t {
            T::Tup => {
                let a = self.h.prim(Prim::I64);
                let b = self.h.prim(Prim::Bool);
                self.h.tuple_ty(&[a, b])
            }
            T::Point => self.h.record_ty("Point"),
            T::Arr => {
                let e = self.h.prim(Prim::I64);
                self.h.array_ty(e, 4)
            }
            T::Obj => self.h.obj_ty(),
            other => self.h.prim(other.prim().unwrap_or(Prim::I64)),
        }
    }

    fn ints(&self) -> &'static [T] {
        if self.scalar { &[T::I64] } else { &INTS }
    }

    fn any_type(&mut self) -> T {
        if self.scalar {
            return self.r.pick(&[T::I64, T::I64, T::Bool, T::F64]);
        }
        let mut ts = vec![
            T::I64,
            T::I64,
            T::I32,
            T::U8,
            T::Bool,
            T::F64,
            T::Tup,
            T::Point,
            T::Arr,
        ];
        if !self.floats {
            ts.retain(|&t| t != T::F64);
        }
        if self.with_objects {
            ts.push(T::Obj);
        }
        self.r.pick(&ts)
    }

    fn function(&mut self, name: &str, entry: bool, size: i64) -> ItemId {
        self.scope.clear();
        self.loops.clear();
        self.blocks.clear();
        self.in_defer = 0;
        self.budget = size;
        let (params, ret) = if entry {
            (vec![T::I64, T::I64], T::I64)
        } else {
            let n = self.r.below(3) as usize;
            let ps: Vec<T> = (0..n).map(|_| self.any_type()).collect();
            let ret = self.any_type();
            (ps, ret)
        };
        self.ret = ret;
        let mut pids = Vec::new();
        for (i, &t) in params.iter().enumerate() {
            let ty = self.ty(t);
            let (p, b) = self.h.param(&format!("p{i}"), ty);
            pids.push(p);
            self.scope.push((b, t));
        }
        let body = self.block_of(ret, 4);
        let rty = self.ty(ret);
        let item = self.h.func(name, &pids, Some(rty), body);
        self.fns.push(FnSig {
            name: name.to_string(),
            params,
            ret,
        });
        item
    }

    // ------------------------------------------------------------ values

    fn int_value(&mut self, t: T) -> i64 {
        let (min, max): (i64, i64) = match t {
            T::I32 => (i64::from(i32::MIN), i64::from(i32::MAX)),
            T::U8 => (0, 255),
            _ => (i64::MIN, i64::MAX),
        };
        let small = self.r.below(21) as i64 - 10;
        let c = self
            .r
            .pick(&[0, 1, 2, 3, 7, 8, 10, 31, 32, 63, 64, 100, 127, 128, 255]);
        let v = match self.r.below(12) {
            0 => min,
            1 => max,
            2 => min.saturating_add(1),
            3 => max - 1,
            4..=7 => small,
            _ => c,
        };
        v.clamp(min, max)
    }

    fn int_lit(&mut self, t: T) -> ExprId {
        let v = self.int_value(t);
        let p = t.prim().unwrap_or(Prim::I64);
        if v < 0 {
            self.h.int(v, p)
        } else {
            self.h.uint(v as u64, p)
        }
    }

    fn float_lit(&mut self) -> ExprId {
        let v = self.r.pick(&[
            0.0,
            -0.0,
            0.5,
            1.0,
            -1.5,
            2.5,
            3.0,
            1e300,
            -1e300,
            1e-300,
            f64::INFINITY,
            f64::NEG_INFINITY,
            9.2e18,
            -9.3e18,
            255.9,
        ]);
        self.h.float(v, Prim::F64)
    }

    fn overflow(&mut self) -> Overflow {
        self.r.pick(&[
            Overflow::Error,
            Overflow::Error,
            Overflow::Wrap,
            Overflow::Wrap,
            Overflow::Trap,
        ])
    }

    fn locals_of(&self, t: T) -> Vec<BinderId> {
        self.scope
            .iter()
            .filter(|(_, tt)| *tt == t)
            .map(|(b, _)| *b)
            .collect()
    }

    fn policy_for(&mut self, k: OpKind) -> Op {
        let mut op = Op::new(k);
        if op.policy.overflow.is_some() {
            let o = self.overflow();
            op = op.with_overflow(o);
        }
        if op.policy.div_zero.is_some() {
            let z = self.r.pick(&[DivZero::Error, DivZero::Trap]);
            op = op.with_div_zero(z);
        }
        if op.policy.shift.is_some() {
            let s = self.r.pick(&[Shift::Error, Shift::Mask, Shift::Saturate]);
            op = op.with_shift(s);
        }
        if op.policy.float_to_int.is_some() {
            let f = self.r.pick(&[FloatToInt::Error, FloatToInt::Saturate]);
            op = op.with_float_to_int(f);
        }
        op
    }

    // ------------------------------------------------------- expressions

    /// An expression of type `t` with nesting at most `d`.
    fn expr(&mut self, t: T, d: u32) -> ExprId {
        self.budget -= 1;
        let d = if self.budget <= 0 { 0 } else { d };
        let locals = self.locals_of(t);
        if d == 0 || self.r.chance(1, 5) {
            if !locals.is_empty() && self.r.chance(2, 3) {
                let b = self.r.pick(&locals);
                return self.h.get(b);
            }
            return self.leaf(t);
        }
        match t {
            T::I64 | T::I32 | T::U8 => self.int_expr(t, d),
            T::Bool => self.bool_expr(d),
            T::F64 => self.float_expr(d),
            T::Tup => match self.r.below(4) {
                0 => self.call_of(t, d).unwrap_or_else(|| self.leaf(t)),
                1 => self.if_expr(t, d),
                _ => {
                    let a = self.expr(T::I64, d - 1);
                    let b = self.expr(T::Bool, d - 1);
                    self.h.tuple(&[a, b])
                }
            },
            T::Point => match self.r.below(4) {
                0 => self.call_of(t, d).unwrap_or_else(|| self.leaf(t)),
                1 => self.block_of(t, d - 1),
                _ => {
                    let x = self.expr(T::I64, d - 1);
                    let y = self.expr(T::I64, d - 1);
                    self.h.record_lit("Point", &[("x", x), ("y", y)])
                }
            },
            T::Arr => match self.r.below(4) {
                0 => {
                    let e = self.expr(T::I64, d - 1);
                    self.h.repeat(e, 4)
                }
                1 => self.call_of(t, d).unwrap_or_else(|| self.leaf(t)),
                _ => {
                    let es: Vec<ExprId> = (0..4).map(|_| self.expr(T::I64, d - 1)).collect();
                    self.h.array(&es)
                }
            },
            T::Obj => {
                if self.r.chance(1, 3) {
                    if let Some(c) = self.call_of(t, d) {
                        return c;
                    }
                }
                self.leaf(t)
            }
        }
    }

    fn leaf(&mut self, t: T) -> ExprId {
        match t {
            T::I64 => match self.r.below(8) {
                0 => self.h.item_path("K"),
                1 => self.h.item_path("G"),
                _ => self.int_lit(t),
            },
            T::I32 | T::U8 => self.int_lit(t),
            T::Bool => {
                let v = self.r.chance(1, 2);
                self.h.boolean(v)
            }
            T::F64 => self.float_lit(),
            T::Tup => {
                let a = self.int_lit(T::I64);
                let b = self.h.boolean(true);
                self.h.tuple(&[a, b])
            }
            T::Point => {
                let x = self.int_lit(T::I64);
                let y = self.int_lit(T::I64);
                self.h.record_lit("Point", &[("x", x), ("y", y)])
            }
            T::Arr => {
                let e = self.int_lit(T::I64);
                self.h.repeat(e, 4)
            }
            T::Obj => {
                let v = self.int_lit(T::I64);
                self.h
                    .ext_call("obj_new", &[HostTy::Prim(Prim::I64)], HostTy::Obj, &[v])
            }
        }
    }

    fn int_expr(&mut self, t: T, d: u32) -> ExprId {
        let p = t.prim().unwrap_or(Prim::I64);
        match self.r.below(22) {
            0..=5 => {
                let k = self.r.pick(&[
                    OpKind::Add,
                    OpKind::Sub,
                    OpKind::Mul,
                    OpKind::Add,
                    OpKind::Sub,
                    OpKind::Mul,
                    OpKind::Div,
                    OpKind::Rem,
                    OpKind::FloorDiv,
                    OpKind::FloorMod,
                    OpKind::Min,
                    OpKind::Max,
                    OpKind::And,
                    OpKind::Or,
                    OpKind::Xor,
                    OpKind::Pow,
                ]);
                let a = self.expr(t, d - 1);
                let b = self.expr(t, d - 1);
                let op = self.policy_for(k);
                self.h.opp(op, &[a, b])
            }
            6 => {
                // A shift by an amount of any integer type.
                let k = self.r.pick(&[OpKind::Shl, OpKind::Shr]);
                let at = self.r.pick(self.ints());
                let a = self.expr(t, d - 1);
                let n = if self.r.chance(1, 2) {
                    self.small_int(at)
                } else {
                    self.expr(at, d - 1)
                };
                let op = self.policy_for(k);
                self.h.opp(op, &[a, n])
            }
            7 => {
                let k = self.r.pick(&[OpKind::Neg, OpKind::Abs, OpKind::BitNot]);
                let a = self.expr(t, d - 1);
                let op = self.policy_for(k);
                self.h.opp(op, &[a])
            }
            8 if !self.scalar => {
                // A conversion from another integer type.
                let from = self.r.pick(&INTS);
                let a = self.expr(from, d - 1);
                let (fb, tb) = (width(from), width(t));
                let k = match self.r.below(3) {
                    0 if fb <= tb => OpKind::Zext(p),
                    1 if fb <= tb => OpKind::Sext(p),
                    2 if fb >= tb => OpKind::Narrow(p),
                    _ => OpKind::IntCast(p),
                };
                let op = self.policy_for(k);
                self.h.opp(op, &[a])
            }
            9 if !self.scalar => {
                // A cast from an integer, a float, or a bool.
                let src = if self.floats {
                    self.r.pick(&[T::I64, T::I32, T::U8, T::F64, T::Bool])
                } else {
                    self.r.pick(&[T::I64, T::I32, T::U8, T::Bool])
                };
                let a = self.expr(src, d - 1);
                let ty = self.ty(t);
                let o = self.overflow();
                let f = self.r.pick(&[FloatToInt::Error, FloatToInt::Saturate]);
                let pol = Policy::NONE.with_overflow(o).with_float_to_int(f);
                self.h.cast(a, ty, pol)
            }
            10 => self.if_expr(t, d),
            11 => self.block_of(t, d - 1),
            12 => self.match_expr(t, d),
            13 => self.call_of(t, d).unwrap_or_else(|| self.leaf(t)),
            14 if t == T::I64 && !self.scalar => self.field_read(d),
            15 => self.counted_loop(Some(t), d),
            16 => self.labeled_expr(t, d),
            17 => {
                // An assignment's value.
                let locals = self.locals_of(t);
                if locals.is_empty() {
                    return self.leaf(t);
                }
                let x = self.r.pick(&locals);
                let v = self.expr(t, d - 1);
                self.h.set(x, v)
            }
            18 if t == T::I64 => {
                let objs = self.locals_of(T::Obj);
                if objs.is_empty() {
                    return self.leaf(t);
                }
                let o = self.r.pick(&objs);
                let go = self.h.get(o);
                self.h
                    .ext_call("obj_get", &[HostTy::Obj], HostTy::Prim(Prim::I64), &[go])
            }
            19 if t == T::I64 && self.floats => {
                // f64 to i64 through a float-to-int conversion.
                let a = self.expr(T::F64, d - 1);
                let f = self.r.pick(&[FloatToInt::Saturate, FloatToInt::Error]);
                let op = Op::new(OpKind::FloatToInt(Prim::I64)).with_float_to_int(f);
                self.h.opp(op, &[a])
            }
            20 if t == T::I64 => {
                // let_place: a compound update through a place evaluated once.
                let arrs = self.locals_of(T::Arr);
                if arrs.is_empty() {
                    return self.leaf(t);
                }
                let a = self.r.pick(&arrs);
                let ga = self.h.get(a);
                let i = self.expr(T::I64, d - 1);
                let place = self.h.index(ga, i);
                let v = self.expr(T::I64, d - 1);
                let op = self.policy_for(OpKind::Add);
                self.h.let_place("pl", place, |h, pb| {
                    let t1 = h.get(pb);
                    let t2 = h.get(pb);
                    let s = h.opp(op, &[t2, v]);
                    h.assign(t1, s)
                })
            }
            _ => self.leaf(t),
        }
    }

    fn small_int(&mut self, t: T) -> ExprId {
        let v = self
            .r
            .pick(&[0i64, 1, 3, 7, 8, 15, 31, 32, 63, 64, 65, 100, -1, -64]);
        let v = if t == T::U8 && v < 0 { 255 } else { v };
        let p = t.prim().unwrap_or(Prim::I64);
        if v < 0 {
            self.h.int(v, p)
        } else {
            self.h.uint(v as u64, p)
        }
    }

    fn bool_expr(&mut self, d: u32) -> ExprId {
        match self.r.below(8) {
            0..=2 => {
                let t = self.r.pick(self.ints());
                let k = self.r.pick(&[
                    OpKind::Eq,
                    OpKind::Ne,
                    OpKind::Lt,
                    OpKind::Le,
                    OpKind::Gt,
                    OpKind::Ge,
                ]);
                let a = self.expr(t, d - 1);
                let b = self.expr(t, d - 1);
                self.h.op(k, &[a, b])
            }
            3 if self.floats => {
                let k = self
                    .r
                    .pick(&[OpKind::Eq, OpKind::Ne, OpKind::Lt, OpKind::Ge]);
                let a = self.expr(T::F64, d - 1);
                let b = self.expr(T::F64, d - 1);
                self.h.op(k, &[a, b])
            }
            4 => {
                let a = self.expr(T::Bool, d - 1);
                self.h.op(OpKind::Not, &[a])
            }
            5 => {
                let k = self
                    .r
                    .pick(&[OpKind::And, OpKind::Or, OpKind::Xor, OpKind::Eq]);
                let a = self.expr(T::Bool, d - 1);
                let b = self.expr(T::Bool, d - 1);
                self.h.op(k, &[a, b])
            }
            6 => self.if_expr(T::Bool, d),
            _ => self.leaf(T::Bool),
        }
    }

    fn float_expr(&mut self, d: u32) -> ExprId {
        match self.r.below(7) {
            0..=2 => {
                let k = self.r.pick(&[
                    OpKind::Add,
                    OpKind::Sub,
                    OpKind::Mul,
                    OpKind::Div,
                    OpKind::Min,
                    OpKind::Max,
                    OpKind::Rem,
                    OpKind::Pow,
                ]);
                let a = self.expr(T::F64, d - 1);
                let b = self.expr(T::F64, d - 1);
                let op = self.policy_for(k);
                self.h.opp(op, &[a, b])
            }
            3 => {
                let k = self.r.pick(&[
                    OpKind::Neg,
                    OpKind::Abs,
                    OpKind::Sqrt,
                    OpKind::Floor,
                    OpKind::Ceil,
                    OpKind::Trunc,
                    OpKind::Round,
                    OpKind::RoundEven,
                ]);
                let a = self.expr(T::F64, d - 1);
                let op = self.policy_for(k);
                self.h.opp(op, &[a])
            }
            4 => {
                let src = self.r.pick(self.ints());
                let a = self.expr(src, d - 1);
                self.h.op(OpKind::IntToFloat(Prim::F64), &[a])
            }
            5 => self.if_expr(T::F64, d),
            _ => self.leaf(T::F64),
        }
    }

    fn if_expr(&mut self, t: T, d: u32) -> ExprId {
        let c = self.expr(T::Bool, d - 1);
        let a = self.block_of(t, d - 1);
        let b = self.block_of(t, d - 1);
        self.h.if_(c, a, Some(b))
    }

    /// `{ stmts; tail }` of type `t`, with its own scope.
    fn block_of(&mut self, t: T, d: u32) -> ExprId {
        let mark = self.scope.len();
        let n = if d == 0 { 0 } else { self.r.below(4) };
        let mut stmts = Vec::new();
        for _ in 0..n {
            if self.budget <= 0 {
                break;
            }
            self.stmt(d, &mut stmts);
        }
        let tail = self.expr(t, d);
        self.scope.truncate(mark);
        self.h.block(&stmts, Some(tail))
    }

    fn match_expr(&mut self, t: T, d: u32) -> ExprId {
        let st = self.r.pick(self.ints());
        let s = self.expr(st, d - 1);
        let p = st.prim().unwrap_or(Prim::I64);
        let n = 1 + self.r.below(4);
        let mut arms = Vec::new();
        for _ in 0..n {
            let mark = self.scope.len();
            let (pat, guard) = match self.r.below(5) {
                0 => {
                    let a = self.lit_pat(st, p);
                    let b = self.lit_pat(st, p);
                    (self.h.pat_or(&[a, b]), None)
                }
                1 => {
                    let lo = self.lit_pat(st, p);
                    let hi = self.lit_pat(st, p);
                    let incl = self.r.chance(1, 2);
                    (self.h.pat_range(Some(lo), Some(hi), incl), None)
                }
                2 => {
                    let x = self.h.local("m");
                    let pat = self.h.pat_bind(x);
                    self.scope.push((x, st));
                    let g = self.expr(T::Bool, d - 1);
                    (pat, Some(g))
                }
                _ => (self.lit_pat(st, p), None),
            };
            let body = self.expr(t, d - 1);
            self.scope.truncate(mark);
            arms.push(self.h.arm(pat, guard, body));
        }
        if self.r.chance(2, 3) {
            let w = self.h.pat_wild();
            let body = self.expr(t, d - 1);
            arms.push(self.h.arm(w, None, body));
        }
        self.h.match_(s, &arms)
    }

    fn lit_pat(&mut self, t: T, p: Prim) -> hir_lang::PatId {
        let v = self.int_value(t);
        let lit = if v < 0 {
            Lit::Int(hir_lang::IntLit::signed(v).with_suffix(p))
        } else {
            Lit::Int(hir_lang::IntLit::new(v as u64).with_suffix(p))
        };
        self.h.b.pat(Pat::Lit(lit))
    }

    fn call_of(&mut self, t: T, d: u32) -> Option<ExprId> {
        let cands: Vec<FnSig> = self.fns.iter().filter(|f| f.ret == t).cloned().collect();
        if cands.is_empty() {
            return None;
        }
        let f = cands[self.r.below(cands.len() as u64) as usize].clone();
        let args: Vec<ExprId> = f
            .params
            .iter()
            .map(|&pt| self.expr(pt, d.saturating_sub(1)))
            .collect();
        Some(self.h.call(&f.name, &args))
    }

    fn field_read(&mut self, d: u32) -> ExprId {
        match self.r.below(3) {
            0 => {
                let ps = self.locals_of(T::Point);
                if ps.is_empty() {
                    return self.leaf(T::I64);
                }
                let p = self.r.pick(&ps);
                let gp = self.h.get(p);
                let f = self.r.pick(&["x", "y"]);
                self.h.field(gp, f)
            }
            1 => {
                let ts = self.locals_of(T::Tup);
                if ts.is_empty() {
                    return self.leaf(T::I64);
                }
                let tl = self.r.pick(&ts);
                let gt = self.h.get(tl);
                self.h.tfield(gt, 0)
            }
            _ => {
                let arrs = self.locals_of(T::Arr);
                if arrs.is_empty() {
                    return self.leaf(T::I64);
                }
                let a = self.r.pick(&arrs);
                let ga = self.h.get(a);
                let it = self.r.pick(&INTS);
                let i = if self.r.chance(2, 3) {
                    let v = self.r.below(6) as i64 - 1;
                    let v = if it == T::U8 && v < 0 { 5 } else { v };
                    let p = it.prim().unwrap_or(Prim::I64);
                    if v < 0 {
                        self.h.int(v, p)
                    } else {
                        self.h.uint(v as u64, p)
                    }
                } else {
                    self.expr(it, d - 1)
                };
                self.h.index(ga, i)
            }
        }
    }

    /// `{ let i = 0; 'l: loop [step: i = i + 1] { if i >= k { break 'l v }; .. } }`
    fn counted_loop(&mut self, val: Option<T>, d: u32) -> ExprId {
        let mark = self.scope.len();
        let i = self.h.local("i");
        let z = self.h.i64(0);
        let li = self.h.let_(i, None, Some(z));
        let label = self.h.label("l");
        let gi = self.h.get(i);
        let k = self.r.below(4) as i64;
        let kk = self.h.i64(k);
        let done = self.h.op(OpKind::Ge, &[gi, kk]);
        let v = val.map(|t| self.expr(t, d.saturating_sub(1)));
        let br = self.h.brk(Some(label), v);
        let brb = self.h.block(&[], Some(br));
        let check = self.h.if_(done, brb, None);
        let cs = self.h.stmt(check);
        self.loops.push(LoopCx {
            label,
            val,
            defer_depth: self.in_defer,
        });
        let mut stmts = vec![cs];
        let n = 1 + self.r.below(3);
        for _ in 0..n {
            if self.budget <= 0 {
                break;
            }
            self.stmt(d.saturating_sub(1), &mut stmts);
        }
        let _ = self.loops.pop();
        let body = self.h.block(&stmts, None);
        let gi2 = self.h.get(i);
        let one = self.h.i64(1);
        let inc = self.h.wrap(OpKind::Add, &[gi2, one]);
        let step = self.h.set(i, inc);
        let lp = self.h.loop_(Some(label), body, Some(step));
        self.scope.truncate(mark);
        match val {
            Some(_) => self.h.block(&[li], Some(lp)),
            None => {
                let s = self.h.stmt(lp);
                self.h.block(&[li, s], None)
            }
        }
    }

    /// `'b: { stmts; v }` with `break 'b v` possible inside.
    fn labeled_expr(&mut self, t: T, d: u32) -> ExprId {
        let mark = self.scope.len();
        let label = self.h.label("b");
        self.blocks.push(BlockCx {
            label,
            val: t,
            defer_depth: self.in_defer,
        });
        let mut stmts = Vec::new();
        let n = 1 + self.r.below(3);
        for _ in 0..n {
            if self.budget <= 0 {
                break;
            }
            self.stmt(d.saturating_sub(1), &mut stmts);
        }
        let tail = self.expr(t, d.saturating_sub(1));
        let _ = self.blocks.pop();
        self.scope.truncate(mark);
        self.h.labeled(label, &stmts, Some(tail))
    }

    // -------------------------------------------------------- statements

    fn stmt(&mut self, d: u32, out: &mut Vec<StmtId>) {
        self.budget -= 1;
        let d = d.max(1);
        match self.r.below(20) {
            0..=4 => {
                let t = self.any_type();
                let init = self.expr(t, d);
                let x = self.h.local("x");
                out.push(self.h.let_(x, None, Some(init)));
                self.scope.push((x, t));
            }
            5 | 6 => {
                if self.scope.is_empty() {
                    return;
                }
                let (x, t) = self.scope[self.r.below(self.scope.len() as u64) as usize];
                let v = self.expr(t, d);
                let a = self.h.set(x, v);
                out.push(self.h.stmt(a));
            }
            7 => {
                // Compound assignment to an integer local.
                let ints: Vec<(BinderId, T)> = self
                    .scope
                    .iter()
                    .copied()
                    .filter(|(_, t)| t.is_int())
                    .collect();
                if ints.is_empty() {
                    return;
                }
                let (x, t) = ints[self.r.below(ints.len() as u64) as usize];
                let k = self.r.pick(&[
                    OpKind::Add,
                    OpKind::Sub,
                    OpKind::Mul,
                    OpKind::Shl,
                    OpKind::Xor,
                ]);
                let op = self.policy_for(k);
                let v = if k == OpKind::Shl {
                    let at = self.r.pick(self.ints());
                    self.small_int(at)
                } else {
                    self.expr(t, d - 1)
                };
                let target = self.h.get(x);
                let a = self.h.assign_op(target, op, v);
                out.push(self.h.stmt(a));
            }
            8 => {
                // A store into a field or an element.
                let (t, target) = match self.r.below(3) {
                    0 => {
                        let ps = self.locals_of(T::Point);
                        if ps.is_empty() {
                            return;
                        }
                        let p = self.r.pick(&ps);
                        let gp = self.h.get(p);
                        (T::I64, self.h.field(gp, "y"))
                    }
                    1 => {
                        let ts = self.locals_of(T::Tup);
                        if ts.is_empty() {
                            return;
                        }
                        let tl = self.r.pick(&ts);
                        let gt = self.h.get(tl);
                        (T::Bool, self.h.tfield(gt, 1))
                    }
                    _ => {
                        let arrs = self.locals_of(T::Arr);
                        if arrs.is_empty() {
                            return;
                        }
                        let a = self.r.pick(&arrs);
                        let ga = self.h.get(a);
                        let i = self.expr(T::U8, d - 1);
                        (T::I64, self.h.index(ga, i))
                    }
                };
                let v = self.expr(t, d - 1);
                let a = self.h.assign(target, v);
                out.push(self.h.stmt(a));
            }
            9 => {
                let c = self.expr(T::Bool, d - 1);
                let a = self.unit_block(d - 1);
                let b = self.unit_block(d - 1);
                let e = self.h.if_(c, a, Some(b));
                out.push(self.h.stmt(e));
            }
            10 => {
                let l = self.counted_loop(None, d);
                out.push(self.h.stmt(l));
            }
            11 => {
                // break or continue to an enclosing loop, or break out of a
                // labeled block, without crossing a defer.
                let lps: Vec<LoopCx> = self
                    .loops
                    .iter()
                    .copied()
                    .filter(|l| l.defer_depth == self.in_defer)
                    .collect();
                let bls: Vec<BlockCx> = self
                    .blocks
                    .iter()
                    .copied()
                    .filter(|b| b.defer_depth == self.in_defer)
                    .collect();
                if !bls.is_empty() && (lps.is_empty() || self.r.chance(1, 3)) {
                    let b = bls[self.r.below(bls.len() as u64) as usize];
                    let v = self.expr(b.val, d - 1);
                    let e = self.h.brk(Some(b.label), Some(v));
                    out.push(self.h.stmt(e));
                } else if !lps.is_empty() {
                    let l = lps[self.r.below(lps.len() as u64) as usize];
                    let e = if self.r.chance(1, 2) {
                        self.h.cont(Some(l.label))
                    } else {
                        let v = l.val.map(|t| self.expr(t, d - 1));
                        self.h.brk(Some(l.label), v)
                    };
                    out.push(self.h.stmt(e));
                }
            }
            12 => {
                if self.in_defer == 0 {
                    let v = self.expr(self.ret, d - 1);
                    let e = self.h.ret(Some(v));
                    out.push(self.h.stmt(e));
                }
            }
            13 => {
                let v = self.expr(T::I64, d - 1);
                let e = self.h.throw(v);
                out.push(self.h.stmt(e));
            }
            14 | 15 => {
                let e = self.try_expr(d);
                out.push(self.h.stmt(e));
            }
            16 => {
                self.in_defer += 1;
                let b = self.unit_block(d - 1);
                self.in_defer -= 1;
                out.push(self.h.defer(b));
            }
            17 | 18 => {
                let v = self.expr(T::I64, d - 1);
                let e = self
                    .h
                    .ext_call("trace", &[HostTy::Prim(Prim::I64)], HostTy::Unit, &[v]);
                out.push(self.h.stmt(e));
            }
            _ => {
                if self.with_objects {
                    let e = if self.r.chance(1, 2) {
                        self.h.ext_call("gc_poll", &[], HostTy::Unit, &[])
                    } else {
                        let v = self.expr(T::I64, d - 1);
                        self.h.ext_call(
                            "host_check",
                            &[HostTy::Prim(Prim::I64)],
                            HostTy::Unit,
                            &[v],
                        )
                    };
                    out.push(self.h.stmt(e));
                }
            }
        }
    }

    fn unit_block(&mut self, d: u32) -> ExprId {
        let mark = self.scope.len();
        let mut stmts = Vec::new();
        let n = self.r.below(3);
        for _ in 0..n {
            if self.budget <= 0 {
                break;
            }
            self.stmt(d, &mut stmts);
        }
        self.scope.truncate(mark);
        self.h.block(&stmts, None)
    }

    /// `try { .. } catch (e: i64) if g { .. } catch _ { .. } finally { .. }`
    fn try_expr(&mut self, d: u32) -> ExprId {
        let body = self.unit_block(d - 1);
        let mut arms = Vec::new();
        if self.r.chance(3, 4) {
            let mark = self.scope.len();
            let e = self.h.local("e");
            self.scope.push((e, T::I64));
            let guard = if self.r.chance(1, 3) {
                Some(self.expr(T::Bool, d - 1))
            } else {
                None
            };
            let b = self.unit_block(d - 1);
            self.scope.truncate(mark);
            arms.push(self.h.catch_typed(Prim::I64, Some(e), guard, b));
        }
        if self.r.chance(1, 3) {
            let b = self.unit_block(d - 1);
            arms.push(self.h.catch_all(b));
        }
        let finally = if arms.is_empty() || self.r.chance(1, 2) {
            Some(self.unit_block(d - 1))
        } else {
            None
        };
        self.h.try_(body, &arms, finally)
    }
}
