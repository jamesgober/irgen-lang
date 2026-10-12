//! Hand-written programs for every construct of the subset: each is lowered,
//! validated, run in ir-lang's interpreter, and checked against the value
//! the program computes (and against the reference evaluator).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Hb, HostTy, Out, error, ret_i64, thrown_i64};
use hir_lang::{Lit, Op, OpKind, Overflow, Prim};
use ir_lang::interp::Val;
use irgen_lang::{ErrorMode, rt};

/// `fn add(a: i64, b: i64) -> i64 { a + b }` (overflow = error).
fn add_unit() -> common::Unit {
    let mut h = Hb::new();
    let i64t = h.prim(Prim::I64);
    let (pa, a) = h.param("a", i64t);
    let i64t = h.prim(Prim::I64);
    let (pb, b) = h.param("b", i64t);
    let (ua, ub) = (h.get(a), h.get(b));
    let sum = h.op(OpKind::Add, &[ua, ub]);
    let body = h.block(&[], Some(sum));
    let ret = h.prim(Prim::I64);
    let _ = h.func("add", &[pa, pb], Some(ret), body);
    h.finish()
}

#[test]
fn test_add_returns_sum_and_raises_overflow() {
    let u = add_unit();
    assert_eq!(u.run("add", &[Val::i64(2), Val::i64(3)]), ret_i64(5));
    assert_eq!(u.run("add", &[Val::i64(i64::MAX), Val::i64(1)]), error(1));
    assert_eq!(
        u.run_mode("add", &[Val::i64(i64::MAX), Val::i64(1)], ErrorMode::Trap),
        Out::Trap(1)
    );
    common::reference::agree(&u, "add", &[Val::i64(2), Val::i64(3)]);
    common::reference::agree(&u, "add", &[Val::i64(i64::MAX), Val::i64(1)]);
}

/// `fn sum(n: i64) -> i64 { let s = 0; let i = 0; loop { if i >= n { break }; s = s + i; i = i + 1 }; s }`
fn sum_unit() -> common::Unit {
    let mut h = Hb::new();
    let t = h.prim(Prim::I64);
    let (pn, n) = h.param("n", t);
    let s = h.local("s");
    let i = h.local("i");
    let z1 = h.i64(0);
    let z2 = h.i64(0);
    let ls = h.let_(s, None, Some(z1));
    let li = h.let_(i, None, Some(z2));
    let (gi, gn) = (h.get(i), h.get(n));
    let done = h.op(OpKind::Ge, &[gi, gn]);
    let brk = h.brk(None, None);
    let bb = h.block(&[], Some(brk));
    let check = h.if_(done, bb, None);
    let c1 = h.stmt(check);
    let (gs, gi2) = (h.get(s), h.get(i));
    let add = h.op(OpKind::Add, &[gs, gi2]);
    let set_s = h.set(s, add);
    let c2 = h.stmt(set_s);
    let gi3 = h.get(i);
    let one = h.i64(1);
    let inc = h.op(OpKind::Add, &[gi3, one]);
    let set_i = h.set(i, inc);
    let c3 = h.stmt(set_i);
    let body = h.block(&[c1, c2, c3], None);
    let lp = h.loop_(None, body, None);
    let lps = h.stmt(lp);
    let gs2 = h.get(s);
    let fb = h.block(&[ls, li, lps], Some(gs2));
    let ret = h.prim(Prim::I64);
    let _ = h.func("sum", &[pn], Some(ret), fb);
    h.finish()
}

#[test]
fn test_loop_with_mutable_locals_sums() {
    let u = sum_unit();
    assert_eq!(u.run("sum", &[Val::i64(10)]), ret_i64(45));
    assert_eq!(u.run("sum", &[Val::i64(0)]), ret_i64(0));
    common::reference::agree(&u, "sum", &[Val::i64(100)]);
}

#[test]
fn test_records_tuples_and_arrays_live_in_memory() {
    let mut h = Hb::new();
    let i64t = h.prim(Prim::I64);
    let i64t2 = h.prim(Prim::I64);
    let _ = h.record("Point", &[("x", i64t), ("y", i64t2)]);
    // fn f(k: i64) -> i64 {
    //   let p = Point { x: 3, y: 4 };
    //   p.y = p.y + 10;
    //   let a = [1, 2, 3, 4];
    //   a[1] = 20;
    //   let t = (p, a[k]);
    //   t.0.x + t.0.y + t.1
    // }
    let kt = h.prim(Prim::I64);
    let (pk, k) = h.param("k", kt);
    let p = h.local("p");
    let (three, four) = (h.i64(3), h.i64(4));
    let lit = h.record_lit("Point", &[("x", three), ("y", four)]);
    let lp = h.let_(p, None, Some(lit));
    let gp = h.get(p);
    let py = h.field(gp, "y");
    let gp2 = h.get(p);
    let py2 = h.field(gp2, "y");
    let ten = h.i64(10);
    let add = h.op(OpKind::Add, &[py2, ten]);
    let asg = h.assign(py, add);
    let s1 = h.stmt(asg);
    let a = h.local("a");
    let elems: Vec<_> = (1..=4).map(|v| h.i64(v)).collect();
    let arr = h.array(&elems);
    let la = h.let_(a, None, Some(arr));
    let ga = h.get(a);
    let one = h.int(1, Prim::I64);
    let a1 = h.index(ga, one);
    let twenty = h.i64(20);
    let asg2 = h.assign(a1, twenty);
    let s2 = h.stmt(asg2);
    let t = h.local("t");
    let (gp3, ga2, gk) = (h.get(p), h.get(a), h.get(k));
    let ak = h.index(ga2, gk);
    let tup = h.tuple(&[gp3, ak]);
    let lt = h.let_(t, None, Some(tup));
    let gt = h.get(t);
    let t0 = h.tfield(gt, 0);
    let t0x = h.field(t0, "x");
    let gt2 = h.get(t);
    let t02 = h.tfield(gt2, 0);
    let t0y = h.field(t02, "y");
    let gt3 = h.get(t);
    let t1 = h.tfield(gt3, 1);
    let s = h.op(OpKind::Add, &[t0x, t0y]);
    let s = h.op(OpKind::Add, &[s, t1]);
    let body = h.block(&[lp, s1, la, s2, lt], Some(s));
    let ret = h.prim(Prim::I64);
    let _ = h.func("f", &[pk], Some(ret), body);
    let u = h.finish();
    assert_eq!(u.run("f", &[Val::i64(1)]), ret_i64(3 + 14 + 20));
    assert_eq!(u.run("f", &[Val::i64(3)]), ret_i64(3 + 14 + 4));
    assert_eq!(u.run("f", &[Val::i64(4)]), error(rt::E_INDEX_OUT_OF_BOUNDS));
    assert_eq!(
        u.run("f", &[Val::i64(-1)]),
        error(rt::E_INDEX_OUT_OF_BOUNDS)
    );
    for k in [-1, 0, 1, 2, 3, 4] {
        common::reference::agree(&u, "f", &[Val::i64(k)]);
    }
}

#[test]
fn test_throw_try_catch_finally() {
    let mut h = Hb::new();
    // fn g(x: i64) -> i64 { if x > 5 { throw x } ; x }
    let t = h.prim(Prim::I64);
    let (px, x) = h.param("x", t);
    let (gx, five) = (h.get(x), h.i64(5));
    let gt = h.op(OpKind::Gt, &[gx, five]);
    let gx2 = h.get(x);
    let th = h.throw(gx2);
    let thb = h.block(&[], Some(th));
    let iff = h.if_(gt, thb, None);
    let s = h.stmt(iff);
    let gx3 = h.get(x);
    let body = h.block(&[s], Some(gx3));
    let ret = h.prim(Prim::I64);
    let _ = h.func("g", &[px], Some(ret), body);
    // fn f(x: i64) -> i64 {
    //   let log = 0;
    //   let r = try { g(x) } catch (e: i64) { e * 100 } finally { log = 7 };
    //   r + log
    // }
    let t = h.prim(Prim::I64);
    let (px, x) = h.param("x", t);
    let log = h.local("log");
    let z = h.i64(0);
    let ll = h.let_(log, None, Some(z));
    let gx = h.get(x);
    let call = h.call("g", &[gx]);
    let body = h.block(&[], Some(call));
    let e = h.local("e");
    let ge = h.get(e);
    let hundred = h.i64(100);
    let mul = h.op(OpKind::Mul, &[ge, hundred]);
    let arm = h.catch_typed(Prim::I64, Some(e), None, mul);
    let seven = h.i64(7);
    let setlog = h.set(log, seven);
    let st = h.stmt(setlog);
    let fin = h.block(&[st], None);
    let tr = h.try_(body, &[arm], Some(fin));
    let r = h.local("r");
    let lr = h.let_(r, None, Some(tr));
    let (gr, gl) = (h.get(r), h.get(log));
    let sum = h.op(OpKind::Add, &[gr, gl]);
    let fbody = h.block(&[ll, lr], Some(sum));
    let ret = h.prim(Prim::I64);
    let _ = h.func("f", &[px], Some(ret), fbody);
    let u = h.finish();
    assert_eq!(u.run("g", &[Val::i64(9)]), thrown_i64(9));
    assert_eq!(u.run("f", &[Val::i64(3)]), ret_i64(10));
    assert_eq!(u.run("f", &[Val::i64(9)]), ret_i64(907));
    for x in [3, 9] {
        common::reference::agree(&u, "f", &[Val::i64(x)]);
    }
}

#[test]
fn test_host_references_survive_calls_through_statepoints() {
    let mut h = Hb::new();
    // fn f(v: i64) -> i64 { let o = obj_new(v); gc_poll(); trace(1); obj_get(o) + obj_get(o) }
    let t = h.prim(Prim::I64);
    let (pv, v) = h.param("v", t);
    let o = h.local("o");
    let gv = h.get(v);
    let mk = h.ext_call("obj_new", &[HostTy::Prim(Prim::I64)], HostTy::Obj, &[gv]);
    let lo = h.let_(o, None, Some(mk));
    let poll = h.ext_call("gc_poll", &[], HostTy::Unit, &[]);
    let sp = h.stmt(poll);
    let one = h.i64(1);
    let tr = h.ext_call("trace", &[HostTy::Prim(Prim::I64)], HostTy::Unit, &[one]);
    let st = h.stmt(tr);
    let go = h.get(o);
    let g1 = h.ext_call("obj_get", &[HostTy::Obj], HostTy::Prim(Prim::I64), &[go]);
    let go2 = h.get(o);
    let g2 = h.ext_call("obj_get", &[HostTy::Obj], HostTy::Prim(Prim::I64), &[go2]);
    let sum = h.op(OpKind::Add, &[g1, g2]);
    let body = h.block(&[lo, sp, st], Some(sum));
    let ret = h.prim(Prim::I64);
    let _ = h.func("f", &[pv], Some(ret), body);
    let u = h.finish();
    let typed = u.check();
    let out = u.lower(&typed);
    assert!(out.stats().relocations >= 2, "{:?}", out.stats());
    let text = out.module().to_string();
    assert!(text.contains("gc("), "{text}");
    assert_eq!(
        common::run_lowered(&out, u.item("f"), &[Val::i64(21)]),
        ret_i64(42)
    );
}

#[test]
fn test_match_on_integers_with_switch_ranges_guards_and_no_match() {
    let mut h = Hb::new();
    // fn m(x: i64) -> i64 { match x { 1 | 2 => 10, 3 => 30, 4..=9 => 49, y if y < 0 => -1, 100 => 100 } }
    let t = h.prim(Prim::I64);
    let (px, x) = h.param("x", t);
    let gx = h.get(x);
    let (p1, p2) = (h.pat_int(1, Prim::I64), h.pat_int(2, Prim::I64));
    let p12 = h.pat_or(&[p1, p2]);
    let b10 = h.i64(10);
    let a1 = h.arm(p12, None, b10);
    let p3 = h.pat_int(3, Prim::I64);
    let b30 = h.i64(30);
    let a2 = h.arm(p3, None, b30);
    let lo = h.pat_int(4, Prim::I64);
    let hi = h.pat_int(9, Prim::I64);
    let rng = h.pat_range(Some(lo), Some(hi), true);
    let b49 = h.i64(49);
    let a3 = h.arm(rng, None, b49);
    let y = h.local("y");
    let py = h.pat_bind(y);
    let (gy, z) = (h.get(y), h.i64(0));
    let neg = h.op(OpKind::Lt, &[gy, z]);
    let m1 = h.i64(-1);
    let a4 = h.arm(py, Some(neg), m1);
    let p100 = h.pat_int(100, Prim::I64);
    let b100 = h.i64(100);
    let a5 = h.arm(p100, None, b100);
    let m = h.match_(gx, &[a1, a2, a3, a4, a5]);
    let body = h.block(&[], Some(m));
    let ret = h.prim(Prim::I64);
    let _ = h.func("m", &[px], Some(ret), body);
    let mut u = h.finish();
    u.profile = typeck_lang::Profile::static_explicit().with_exhaustive_match(false);
    let cases = [
        (1, ret_i64(10)),
        (2, ret_i64(10)),
        (3, ret_i64(30)),
        (7, ret_i64(49)),
        (-5, ret_i64(-1)),
        (100, ret_i64(100)),
        (50, error(rt::E_NO_MATCH)),
    ];
    for (x, want) in cases {
        assert_eq!(u.run("m", &[Val::i64(x)]), want, "x = {x}");
        common::reference::agree(&u, "m", &[Val::i64(x)]);
    }
    let _ = (Lit::Bool(true), Op::new(OpKind::Add), Overflow::Wrap);
}
