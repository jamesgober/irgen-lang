//! One test per construct of the subset: lowered, validated, run in ir-lang's
//! interpreter against the expected value, and checked against the
//! reference evaluator in both error modes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use common::{Hb, HostTy, Out, Unit, error, reference, ret_i64, run_lowered, thrown_i64};
use hir_lang::{
    BinderKind, DivZero, Expr, FloatToInt, Item, ItemKind, Lit, Op, OpKind, Overflow, Pat, Policy,
    Prim, Shift,
};
use ir_lang::interp::Val;
use irgen_lang::{ErrorMode, Options, rt};

/// `fn name(params..) -> ret { body }` where `body` is built by `f` from
/// the parameter binders.
fn func1(
    h: &mut Hb,
    name: &str,
    params: &[(&str, Prim)],
    ret: Option<Prim>,
    f: impl FnOnce(&mut Hb, &[hir_lang::BinderId]) -> hir_lang::ExprId,
) -> hir_lang::ItemId {
    let mut ps = Vec::new();
    let mut bs = Vec::new();
    for &(n, p) in params {
        let t = h.prim(p);
        let (pid, b) = h.param(n, t);
        ps.push(pid);
        bs.push(b);
    }
    let body = f(h, &bs);
    let ret = ret.map(|p| h.prim(p));
    h.func(name, &ps, ret, body)
}

fn agree(u: &Unit, name: &str, args: &[Val]) {
    reference::agree(u, name, args);
}

#[test]
fn test_labeled_blocks_and_nested_loops_with_labeled_continue() {
    let mut h = Hb::new();
    // fn f(n: i64) -> i64 {
    //   let acc = 0;
    //   'outer: loop [step i += 1] (i from 0) {
    //     if i >= n { break 'outer }
    //     let j = 0;
    //     'inner: loop [step j += 1] {
    //       if j >= 3 { break 'inner }
    //       if j == 1 { continue 'outer }      // skips the rest of outer body
    //       acc = acc + 10;
    //     }
    //     acc = acc + 1;
    //   }
    //   let v = 'b: { if acc > 25 { break 'b acc * 2 }; acc };
    //   v
    // }
    let _ = func1(
        &mut h,
        "f",
        &[("n", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let n = ps[0];
            let acc = h.local("acc");
            let z = h.i64(0);
            let lacc = h.let_(acc, None, Some(z));
            let i = h.local("i");
            let z = h.i64(0);
            let li = h.let_(i, None, Some(z));
            let outer = h.label("outer");
            let inner = h.label("inner");
            let (gi, gn) = (h.get(i), h.get(n));
            let ge = h.op(OpKind::Ge, &[gi, gn]);
            let bo = h.brk(Some(outer), None);
            let bob = h.block(&[], Some(bo));
            let c1 = h.if_(ge, bob, None);
            let s1 = h.stmt(c1);
            let j = h.local("j");
            let z = h.i64(0);
            let lj = h.let_(j, None, Some(z));
            let (gj, three) = (h.get(j), h.i64(3));
            let ge3 = h.op(OpKind::Ge, &[gj, three]);
            let bi = h.brk(Some(inner), None);
            let bib = h.block(&[], Some(bi));
            let c2 = h.if_(ge3, bib, None);
            let t1 = h.stmt(c2);
            let (gj, one) = (h.get(j), h.i64(1));
            let eq1 = h.op(OpKind::Eq, &[gj, one]);
            let co = h.cont(Some(outer));
            let cob = h.block(&[], Some(co));
            let c3 = h.if_(eq1, cob, None);
            let t2 = h.stmt(c3);
            let (ga, ten) = (h.get(acc), h.i64(10));
            let add = h.op(OpKind::Add, &[ga, ten]);
            let sa = h.set(acc, add);
            let t3 = h.stmt(sa);
            let ib = h.block(&[t1, t2, t3], None);
            let (gj, one) = (h.get(j), h.i64(1));
            let incj = h.op(OpKind::Add, &[gj, one]);
            let stepj = h.set(j, incj);
            let il = h.loop_(Some(inner), ib, Some(stepj));
            let s2 = h.stmt(il);
            let (ga, one) = (h.get(acc), h.i64(1));
            let add1 = h.op(OpKind::Add, &[ga, one]);
            let sa1 = h.set(acc, add1);
            let s3 = h.stmt(sa1);
            let ob = h.block(&[s1, lj, s2, s3], None);
            let (gi, one) = (h.get(i), h.i64(1));
            let inci = h.op(OpKind::Add, &[gi, one]);
            let stepi = h.set(i, inci);
            let ol = h.loop_(Some(outer), ob, Some(stepi));
            let s_ol = h.stmt(ol);
            let b = h.label("b");
            let (ga, tf) = (h.get(acc), h.i64(25));
            let gt = h.op(OpKind::Gt, &[ga, tf]);
            let (ga, two) = (h.get(acc), h.i64(2));
            let dbl = h.op(OpKind::Mul, &[ga, two]);
            let bb = h.brk(Some(b), Some(dbl));
            let bbb = h.block(&[], Some(bb));
            let c4 = h.if_(gt, bbb, None);
            let s4 = h.stmt(c4);
            let ga = h.get(acc);
            let lb = h.labeled(b, &[s4], Some(ga));
            let v = h.local("v");
            let lv = h.let_(v, None, Some(lb));
            let gv = h.get(v);
            h.block(&[lacc, li, s_ol, lv], Some(gv))
        },
    );
    let u = h.finish();
    // Each outer iteration adds 10 (j = 0) then continues at j = 1.
    assert_eq!(u.run("f", &[Val::i64(2)]), ret_i64(20));
    assert_eq!(u.run("f", &[Val::i64(3)]), ret_i64(60));
    for n in 0..5 {
        agree(&u, "f", &[Val::i64(n)]);
    }
}

#[test]
fn test_defer_runs_in_reverse_on_every_exit() {
    let mut h = Hb::new();
    // fn f(x: i64) -> i64 {
    //   defer { trace(1) }
    //   defer { trace(2) }
    //   if x == 0 { return 10 }
    //   if x == 1 { throw 11 }
    //   trace(3);
    //   12
    // }
    let _ = func1(
        &mut h,
        "f",
        &[("x", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let x = ps[0];
            let tr = |h: &mut Hb, v: i64| {
                let a = h.i64(v);
                let c = h.ext_call("trace", &[HostTy::Prim(Prim::I64)], HostTy::Unit, &[a]);
                h.stmt(c)
            };
            let t1 = tr(h, 1);
            let b1 = h.block(&[t1], None);
            let d1 = h.defer(b1);
            let t2 = tr(h, 2);
            let b2 = h.block(&[t2], None);
            let d2 = h.defer(b2);
            let (gx, z) = (h.get(x), h.i64(0));
            let e0 = h.op(OpKind::Eq, &[gx, z]);
            let ten = h.i64(10);
            let r = h.ret(Some(ten));
            let rb = h.block(&[], Some(r));
            let i0 = h.if_(e0, rb, None);
            let s0 = h.stmt(i0);
            let (gx, one) = (h.get(x), h.i64(1));
            let e1 = h.op(OpKind::Eq, &[gx, one]);
            let el = h.i64(11);
            let th = h.throw(el);
            let tb = h.block(&[], Some(th));
            let i1 = h.if_(e1, tb, None);
            let s1 = h.stmt(i1);
            let t3 = tr(h, 3);
            let tw = h.i64(12);
            h.block(&[d1, d2, s0, s1, t3], Some(tw))
        },
    );
    let u = h.finish();
    let typed = u.check();
    let out = u.lower(&typed);
    let f = u.item("f");
    assert_eq!(
        common::run_lowered_traced(&out, f, &[Val::i64(0)]),
        (ret_i64(10), vec![2, 1])
    );
    assert_eq!(
        common::run_lowered_traced(&out, f, &[Val::i64(1)]),
        (thrown_i64(11), vec![2, 1])
    );
    assert_eq!(
        common::run_lowered_traced(&out, f, &[Val::i64(2)]),
        (ret_i64(12), vec![3, 2, 1])
    );
    for x in 0..3 {
        agree(&u, "f", &[Val::i64(x)]);
    }
}

#[test]
fn test_finally_overrides_with_its_own_jump() {
    let mut h = Hb::new();
    // fn f(x: i64) -> i64 {
    //   let r = 0;
    //   loop { try { if x > 0 { throw x } ; r = 1; break } finally { r = r + 100; break } }
    //   r
    // }
    let _ = func1(
        &mut h,
        "f",
        &[("x", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let x = ps[0];
            let r = h.local("r");
            let z = h.i64(0);
            let lr = h.let_(r, None, Some(z));
            let (gx, z) = (h.get(x), h.i64(0));
            let pos = h.op(OpKind::Gt, &[gx, z]);
            let gx = h.get(x);
            let th = h.throw(gx);
            let thb = h.block(&[], Some(th));
            let ifp = h.if_(pos, thb, None);
            let s1 = h.stmt(ifp);
            let one = h.i64(1);
            let sr = h.set(r, one);
            let s2 = h.stmt(sr);
            let br = h.brk(None, None);
            let s3 = h.stmt(br);
            let body = h.block(&[s1, s2, s3], None);
            let (gr, hundred) = (h.get(r), h.i64(100));
            let add = h.op(OpKind::Add, &[gr, hundred]);
            let sr2 = h.set(r, add);
            let f1 = h.stmt(sr2);
            let br2 = h.brk(None, None);
            let f2 = h.stmt(br2);
            let fin = h.block(&[f1, f2], None);
            let tr = h.try_(body, &[], Some(fin));
            let ts = h.stmt(tr);
            let lb = h.block(&[ts], None);
            let lp = h.loop_(None, lb, None);
            let ls = h.stmt(lp);
            let gr = h.get(r);
            h.block(&[lr, ls], Some(gr))
        },
    );
    let u = h.finish();
    // The finally's break discards the pending exception.
    assert_eq!(u.run("f", &[Val::i64(5)]), ret_i64(100));
    assert_eq!(u.run("f", &[Val::i64(0)]), ret_i64(101));
    for x in [0, 5] {
        agree(&u, "f", &[Val::i64(x)]);
    }
}

#[test]
fn test_host_exceptions_reach_catch_through_landing_pads_with_live_references() {
    let mut h = Hb::new();
    // fn f(x: i64) -> i64 {
    //   let o = obj_new(x);
    //   let r = try { host_throw(x + 1); 0 } catch (e: i64) { e + obj_get(o) };
    //   r + obj_get(o)
    // }
    let _ = func1(
        &mut h,
        "f",
        &[("x", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let x = ps[0];
            let o = h.local("o");
            let gx = h.get(x);
            let mk = h.ext_call("obj_new", &[HostTy::Prim(Prim::I64)], HostTy::Obj, &[gx]);
            let lo = h.let_(o, None, Some(mk));
            let (gx, one) = (h.get(x), h.i64(1));
            let x1 = h.op(OpKind::Add, &[gx, one]);
            let ht = h.ext_call(
                "host_throw",
                &[HostTy::Prim(Prim::I64)],
                HostTy::Unit,
                &[x1],
            );
            let s = h.stmt(ht);
            let z = h.i64(0);
            let body = h.block(&[s], Some(z));
            let e = h.local("e");
            let (ge, go) = (h.get(e), h.get(o));
            let og = h.ext_call("obj_get", &[HostTy::Obj], HostTy::Prim(Prim::I64), &[go]);
            let sum = h.op(OpKind::Add, &[ge, og]);
            let arm = h.catch_typed(Prim::I64, Some(e), None, sum);
            let tr = h.try_(body, &[arm], None);
            let r = h.local("r");
            let lr = h.let_(r, None, Some(tr));
            let (gr, go) = (h.get(r), h.get(o));
            let og = h.ext_call("obj_get", &[HostTy::Obj], HostTy::Prim(Prim::I64), &[go]);
            let total = h.op(OpKind::Add, &[gr, og]);
            h.block(&[lo, lr], Some(total))
        },
    );
    let u = h.finish();
    let typed = u.check();
    let out = u.lower(&typed);
    let text = out.module().to_string();
    assert!(text.contains("invoke @host_throw"), "{text}");
    assert!(text.contains("gc("), "{text}");
    assert_eq!(
        run_lowered(&out, u.item("f"), &[Val::i64(5)]),
        ret_i64(6 + 5 + 5)
    );
    agree(&u, "f", &[Val::i64(5)]);
}

#[test]
fn test_patterns_destructure_tuples_and_records_in_let_and_match() {
    let mut h = Hb::new();
    let (xt, yt) = (h.prim(Prim::I64), h.prim(Prim::I64));
    let _ = h.record("P", &[("x", xt), ("y", yt)]);
    // fn f(a: i64, b: i64) -> i64 {
    //   let (u, _, w) = (a, true, b);
    //   let P { x, y: yy } = P { x: u, y: w };
    //   match (x, yy) { (0, q) => q, (p, 0) => p * 2, (p, q) if p == q => 7, _ => x + yy }
    // }
    let _ = func1(
        &mut h,
        "f",
        &[("a", Prim::I64), ("b", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let (a, b) = (ps[0], ps[1]);
            let (u, w) = (h.local("u"), h.local("w"));
            let pu = h.pat_bind(u);
            let pw0 = h.pat_wild();
            let pw = h.pat_bind(w);
            let tp = h.pat_tuple(&[pu, pw0, pw], None);
            let (ga, tt, gb) = (h.get(a), h.boolean(true), h.get(b));
            let tup = h.tuple(&[ga, tt, gb]);
            let l1 = h.let_pat(tp, tup, None);
            let (x, yy) = (h.local("x"), h.local("yy"));
            let px = h.pat_bind(x);
            let pyy = h.pat_bind(yy);
            let (ix, iy) = (h.ident("x"), h.ident("y"));
            let fields = h.b.list(&[
                hir_lang::FieldPat { name: ix, pat: px },
                hir_lang::FieldPat { name: iy, pat: pyy },
            ]);
            let ppath = h.type_path("P");
            let rp = h.b.pat(Pat::Record {
                path: Some(ppath),
                fields,
                rest: false,
            });
            let (gu, gw) = (h.get(u), h.get(w));
            let lit = h.record_lit("P", &[("x", gu), ("y", gw)]);
            let l2 = h.let_pat(rp, lit, None);
            let (gx, gyy) = (h.get(x), h.get(yy));
            let scrut = h.tuple(&[gx, gyy]);
            // arm 1: (0, q) => q
            let q = h.local("q");
            let z = h.pat_int(0, Prim::I64);
            let pq = h.pat_bind(q);
            let p1 = h.pat_tuple(&[z, pq], None);
            let gq = h.get(q);
            let a1 = h.arm(p1, None, gq);
            // arm 2: (p, 0) => p * 2
            let p = h.local("p");
            let pp = h.pat_bind(p);
            let z = h.pat_int(0, Prim::I64);
            let p2 = h.pat_tuple(&[pp, z], None);
            let (gp, two) = (h.get(p), h.i64(2));
            let m = h.op(OpKind::Mul, &[gp, two]);
            let a2 = h.arm(p2, None, m);
            // arm 3: (p, q) if p == q => 7
            let (p3b, q3b) = (h.local("p"), h.local("q"));
            let pp3 = h.pat_bind(p3b);
            let pq3 = h.pat_bind(q3b);
            let p3 = h.pat_tuple(&[pp3, pq3], None);
            let (g1, g2) = (h.get(p3b), h.get(q3b));
            let eq = h.op(OpKind::Eq, &[g1, g2]);
            let seven = h.i64(7);
            let a3 = h.arm(p3, Some(eq), seven);
            // _ => x + yy
            let w4 = h.pat_wild();
            let (gx, gyy) = (h.get(x), h.get(yy));
            let s = h.op(OpKind::Add, &[gx, gyy]);
            let a4 = h.arm(w4, None, s);
            let m = h.match_(scrut, &[a1, a2, a3, a4]);
            h.block(&[l1, l2], Some(m))
        },
    );
    let u = h.finish();
    assert_eq!(u.run("f", &[Val::i64(0), Val::i64(9)]), ret_i64(9));
    assert_eq!(u.run("f", &[Val::i64(4), Val::i64(0)]), ret_i64(8));
    assert_eq!(u.run("f", &[Val::i64(3), Val::i64(3)]), ret_i64(7));
    assert_eq!(u.run("f", &[Val::i64(3), Val::i64(4)]), ret_i64(7));
    for (a, b) in [(0, 9), (4, 0), (3, 3), (3, 4), (0, 0)] {
        agree(&u, "f", &[Val::i64(a), Val::i64(b)]);
    }
}

#[test]
fn test_let_else_and_refutable_let_raise_no_match() {
    let mut h = Hb::new();
    // fn f(x: i64) -> i64 { let 5 = x else { return -1 }; let (1 | 2) = x - 4; 100 }
    let _ = func1(
        &mut h,
        "f",
        &[("x", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let x = ps[0];
            let five = h.pat_int(5, Prim::I64);
            let gx = h.get(x);
            let m1 = h.i64(-1);
            let r = h.ret(Some(m1));
            let rb = h.block(&[], Some(r));
            let l1 = h.let_pat(five, gx, Some(rb));
            let (a, b) = (h.pat_int(1, Prim::I64), h.pat_int(2, Prim::I64));
            let or = h.pat_or(&[a, b]);
            let (gx, four) = (h.get(x), h.i64(4));
            let sub = h.op(OpKind::Sub, &[gx, four]);
            let l2 = h.let_pat(or, sub, None);
            let hundred = h.i64(100);
            h.block(&[l1, l2], Some(hundred))
        },
    );
    let u = h.finish();
    assert_eq!(u.run("f", &[Val::i64(7)]), ret_i64(-1));
    // 5 - 4 = 1 matches `1 | 2`.
    assert_eq!(u.run("f", &[Val::i64(5)]), ret_i64(100));
    agree(&u, "f", &[Val::i64(5)]);
    agree(&u, "f", &[Val::i64(7)]);
}

#[test]
fn test_globals_constants_and_constant_patterns() {
    let mut h = Hb::new();
    // const LIMIT: i64 = 2;  global COUNT: i64 = 40;  global TABLE: [i64; 3] = [1, LIMIT, 9];
    let ty = h.prim(Prim::I64);
    let v = h.i64(2);
    let n = h.name("LIMIT");
    let limit = h.item(Item::new(
        Some(n),
        ItemKind::Const {
            ty: Some(ty),
            value: Some(v),
        },
    ));
    h.named("LIMIT", limit);
    let ty = h.prim(Prim::I64);
    let v = h.i64(40);
    let n = h.name("COUNT");
    let count = h.item(Item::new(
        Some(n),
        ItemKind::Global {
            ty: Some(ty),
            mutable: true,
            init: Some(v),
        },
    ));
    h.named("COUNT", count);
    let e = h.prim(Prim::I64);
    let ty = h.array_ty(e, 3);
    let one = h.i64(1);
    let lim = h.item_path("LIMIT");
    let nine = h.i64(9);
    let arr = h.array(&[one, lim, nine]);
    let n = h.name("TABLE");
    let table = h.item(Item::new(
        Some(n),
        ItemKind::Global {
            ty: Some(ty),
            mutable: true,
            init: Some(arr),
        },
    ));
    h.named("TABLE", table);
    // fn f(i: i64) -> i64 {
    //   COUNT = COUNT + 1;
    //   TABLE[i] = TABLE[i] * 10;
    //   match i { LIMIT => 1000, _ => COUNT + TABLE[i] }
    // }
    let _ = func1(
        &mut h,
        "f",
        &[("i", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let i = ps[0];
            let c1 = h.item_path("COUNT");
            let c2 = h.item_path("COUNT");
            let one = h.i64(1);
            let add = h.op(OpKind::Add, &[c2, one]);
            let a1 = h.assign(c1, add);
            let s1 = h.stmt(a1);
            let (t1, gi) = (h.item_path("TABLE"), h.get(i));
            let lhs = h.index(t1, gi);
            let (t2, gi) = (h.item_path("TABLE"), h.get(i));
            let rhs0 = h.index(t2, gi);
            let ten = h.i64(10);
            let rhs = h.op(OpKind::Mul, &[rhs0, ten]);
            let a2 = h.assign(lhs, rhs);
            let s2 = h.stmt(a2);
            let n = h.name("LIMIT");
            let p = h.b.name_path(n, hir_lang::Ns::Pattern);
            let cp = h.b.pat(Pat::Path(p));
            h.fixup_path(p, "LIMIT");
            let k = h.i64(1000);
            let arm1 = h.arm(cp, None, k);
            let w = h.pat_wild();
            let c = h.item_path("COUNT");
            let (t3, gi) = (h.item_path("TABLE"), h.get(i));
            let ti = h.index(t3, gi);
            let s = h.op(OpKind::Add, &[c, ti]);
            let arm2 = h.arm(w, None, s);
            let scr = h.get(i);
            let m = h.match_(scr, &[arm1, arm2]);
            h.block(&[s1, s2], Some(m))
        },
    );
    let mut u = h.finish();
    u.profile = typeck_lang::Profile::static_explicit().with_exhaustive_match(false);
    let typed = u.check();
    let out = u.lower(&typed);
    assert!(out.global(table).is_some());
    assert_eq!(
        out.module()
            .global(out.global(table).unwrap())
            .unwrap()
            .size,
        24
    );
    // Globals persist across calls in one interpreter.
    let mut it =
        ir_lang::interp::Interpreter::new(out.module(), common::TestHost::default()).unwrap();
    let f = out.function(u.item("f")).unwrap();
    let r = it.call(f, &[Val::i64(0)]);
    assert_eq!(common::decode(&it, r), ret_i64(41 + 10));
    let r = it.call(f, &[Val::i64(0)]);
    assert_eq!(common::decode(&it, r), ret_i64(42 + 100));
    let r = it.call(f, &[Val::i64(2)]);
    assert_eq!(common::decode(&it, r), ret_i64(1000));
    let r = it.call(f, &[Val::i64(3)]);
    assert_eq!(common::decode(&it, r), error(rt::E_INDEX_OUT_OF_BOUNDS));
    for i in [0, 1, 2, 5, -1] {
        agree(&u, "f", &[Val::i64(i)]);
    }
}

#[test]
fn test_aggregate_parameters_and_results_travel_by_address() {
    let mut h = Hb::new();
    let (xt, yt) = (h.prim(Prim::I64), h.prim(Prim::I64));
    let _ = h.record("P", &[("x", xt), ("y", yt)]);
    // fn swap(p: P) -> P { let t = p.x; p.x = p.y; p.y = t; p }
    let pty = h.record_ty("P");
    let (pp, p) = h.param("p", pty);
    let t = h.local("t");
    let gp = h.get(p);
    let px = h.field(gp, "x");
    let lt = h.let_(t, None, Some(px));
    let (gp1, gp2) = (h.get(p), h.get(p));
    let (px, py) = (h.field(gp1, "x"), h.field(gp2, "y"));
    let a1 = h.assign(px, py);
    let s1 = h.stmt(a1);
    let gp = h.get(p);
    let py = h.field(gp, "y");
    let gt = h.get(t);
    let a2 = h.assign(py, gt);
    let s2 = h.stmt(a2);
    let gp = h.get(p);
    let body = h.block(&[lt, s1, s2], Some(gp));
    let rty = h.record_ty("P");
    let _ = h.func("swap", &[pp], Some(rty), body);
    // fn f(a: i64, b: i64) -> i64 { let q = P { x: a, y: b }; let r = swap(q); q.x * 1000 + r.x * 10 + r.y }
    let _ = func1(
        &mut h,
        "f",
        &[("a", Prim::I64), ("b", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let (a, b) = (ps[0], ps[1]);
            let q = h.local("q");
            let (ga, gb) = (h.get(a), h.get(b));
            let lit = h.record_lit("P", &[("x", ga), ("y", gb)]);
            let lq = h.let_(q, None, Some(lit));
            let r = h.local("r");
            let gq = h.get(q);
            let c = h.call("swap", &[gq]);
            let lr = h.let_(r, None, Some(c));
            let gq = h.get(q);
            let qx = h.field(gq, "x");
            let k = h.i64(1000);
            let m1 = h.op(OpKind::Mul, &[qx, k]);
            let gr = h.get(r);
            let rx = h.field(gr, "x");
            let ten = h.i64(10);
            let m2 = h.op(OpKind::Mul, &[rx, ten]);
            let gr = h.get(r);
            let ry = h.field(gr, "y");
            let s = h.op(OpKind::Add, &[m1, m2]);
            let s = h.op(OpKind::Add, &[s, ry]);
            h.block(&[lq, lr], Some(s))
        },
    );
    let u = h.finish();
    // The callee's writes to its parameter do not reach the caller's `q`.
    assert_eq!(
        u.run("f", &[Val::i64(1), Val::i64(2)]),
        ret_i64(1000 + 20 + 1)
    );
    agree(&u, "f", &[Val::i64(1), Val::i64(2)]);
}

#[test]
fn test_repeat_unrolled_and_looped() {
    let mut h = Hb::new();
    // fn f(v: i64, i: i64) -> i64 { let a = [v; 3]; let b = [v + 1; 40]; a[i] + b[i * 10] }
    let _ = func1(
        &mut h,
        "f",
        &[("v", Prim::I64), ("i", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let (v, i) = (ps[0], ps[1]);
            let a = h.local("a");
            let gv = h.get(v);
            let r1 = h.repeat(gv, 3);
            let la = h.let_(a, None, Some(r1));
            let b = h.local("b");
            let (gv, one) = (h.get(v), h.i64(1));
            let v1 = h.op(OpKind::Add, &[gv, one]);
            let r2 = h.repeat(v1, 40);
            let lb = h.let_(b, None, Some(r2));
            let (ga, gi) = (h.get(a), h.get(i));
            let ai = h.index(ga, gi);
            let (gb, gi, ten) = (h.get(b), h.get(i), h.i64(10));
            let i10 = h.op(OpKind::Mul, &[gi, ten]);
            let bi = h.index(gb, i10);
            let s = h.op(OpKind::Add, &[ai, bi]);
            h.block(&[la, lb], Some(s))
        },
    );
    let u = h.finish();
    assert_eq!(u.run("f", &[Val::i64(5), Val::i64(2)]), ret_i64(11));
    assert_eq!(
        u.run("f", &[Val::i64(5), Val::i64(3)]),
        error(rt::E_INDEX_OUT_OF_BOUNDS)
    );
    for i in [0, 2, 3, 4] {
        agree(&u, "f", &[Val::i64(5), Val::i64(i)]);
    }
}

#[test]
fn test_let_place_evaluates_the_index_once() {
    let mut h = Hb::new();
    // fn f(i: i64) -> i64 {
    //   let a = [10, 20, 30];
    //   let_place p = a[{ trace(i); i }] in p = p + 1;     (the index traced once)
    //   a[1] += 5;                                         (compound: place once)
    //   a[0] + a[1] + a[2]
    // }
    let _ = func1(
        &mut h,
        "f",
        &[("i", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let i = ps[0];
            let a = h.local("a");
            let els: Vec<_> = [10, 20, 30].iter().map(|&v| h.i64(v)).collect();
            let arr = h.array(&els);
            let la = h.let_(a, None, Some(arr));
            let gi = h.get(i);
            let tr = h.ext_call("trace", &[HostTy::Prim(Prim::I64)], HostTy::Unit, &[gi]);
            let ts = h.stmt(tr);
            let gi = h.get(i);
            let idx = h.block(&[ts], Some(gi));
            let ga = h.get(a);
            let place = h.index(ga, idx);
            let lp = h.let_place("p", place, |h, p| {
                let (t, r) = (h.get(p), h.get(p));
                let one = h.i64(1);
                let s = h.op(OpKind::Add, &[r, one]);
                h.assign(t, s)
            });
            let s1 = h.stmt(lp);
            let (ga, one) = (h.get(a), h.i64(1));
            let a1 = h.index(ga, one);
            let five = h.i64(5);
            let ca = h.assign_op(a1, Op::new(OpKind::Add), five);
            let s2 = h.stmt(ca);
            let mut sum = h.i64(0);
            for k in 0..3 {
                let (ga, kk) = (h.get(a), h.i64(k));
                let e = h.index(ga, kk);
                sum = h.op(OpKind::Add, &[sum, e]);
            }
            h.block(&[la, s1, s2], Some(sum))
        },
    );
    let u = h.finish();
    let typed = u.check();
    let out = u.lower(&typed);
    assert_eq!(
        common::run_lowered_traced(&out, u.item("f"), &[Val::i64(2)]),
        (ret_i64(10 + 25 + 31), vec![2])
    );
    for i in [0, 1, 2, 3] {
        agree(&u, "f", &[Val::i64(i)]);
    }
}

#[test]
fn test_every_integer_operation_and_policy_agrees_with_the_reference() {
    let kinds = [
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
        OpKind::Shl,
        OpKind::Shr,
        OpKind::Pow,
    ];
    let values: [i64; 9] = [0, 1, -1, 2, 7, -8, 63, i64::MAX, i64::MIN];
    for &p in &[Prim::I64, Prim::U8, Prim::I32, Prim::U64] {
        for &k in &kinds {
            for ov in [Overflow::Error, Overflow::Wrap, Overflow::Trap] {
                let mut h = Hb::new();
                let mut op = Op::new(k);
                if op.policy.overflow.is_some() {
                    op = op.with_overflow(ov);
                }
                if op.policy.div_zero.is_some() {
                    op = op.with_div_zero(if ov == Overflow::Trap {
                        DivZero::Trap
                    } else {
                        DivZero::Error
                    });
                }
                if op.policy.shift.is_some() {
                    op = op.with_shift(match ov {
                        Overflow::Error => Shift::Error,
                        Overflow::Wrap => Shift::Mask,
                        _ => Shift::Saturate,
                    });
                }
                let _ = func1(&mut h, "f", &[("a", p), ("b", p)], Some(p), |h, ps| {
                    let (ga, gb) = (h.get(ps[0]), h.get(ps[1]));
                    let e = h.opp(op, &[ga, gb]);
                    h.block(&[], Some(e))
                });
                let u = h.finish();
                for &a in &values {
                    for &b in &values {
                        let (va, vb) = (cast(a, p), cast(b, p));
                        agree(&u, "f", &[va, vb]);
                    }
                }
            }
        }
    }
}

fn cast(v: i64, p: Prim) -> Val {
    match p {
        Prim::U8 => Val::u8(v as u8),
        Prim::I32 => Val::i32(v as i32),
        Prim::U64 => Val::u64(v as u64),
        _ => Val::i64(v),
    }
}

#[test]
fn test_unary_and_conversion_operations() {
    let ops = [
        (OpKind::Neg, Prim::I32),
        (OpKind::Abs, Prim::I32),
        (OpKind::BitNot, Prim::I32),
        (OpKind::IntCast(Prim::U8), Prim::U8),
        (OpKind::IntCast(Prim::I64), Prim::I64),
        (OpKind::Zext(Prim::U64), Prim::U64),
        (OpKind::Sext(Prim::I64), Prim::I64),
        (OpKind::Narrow(Prim::I8), Prim::I8),
        (OpKind::Zext(Prim::U32), Prim::U32),
        (OpKind::Bitcast(Prim::U32), Prim::U32),
        (OpKind::IntToFloat(Prim::F64), Prim::F64),
    ];
    for (k, ret) in ops {
        for ov in [Overflow::Error, Overflow::Wrap, Overflow::Trap] {
            let mut h = Hb::new();
            let mut op = Op::new(k);
            if op.policy.overflow.is_some() {
                op = op.with_overflow(ov);
            }
            let _ = func1(&mut h, "f", &[("a", Prim::I32)], Some(ret), |h, ps| {
                let ga = h.get(ps[0]);
                let e = h.opp(op, &[ga]);
                h.block(&[], Some(e))
            });
            let u = h.finish();
            for a in [0, 1, -1, 300, i32::MIN, i32::MAX, -129] {
                agree(&u, "f", &[Val::i32(a)]);
            }
        }
    }
}

#[test]
fn test_float_operations_and_float_to_int() {
    let kinds = [
        OpKind::Add,
        OpKind::Sub,
        OpKind::Mul,
        OpKind::Div,
        OpKind::Rem,
        OpKind::Min,
        OpKind::Max,
        OpKind::Pow,
    ];
    let values = [
        0.0,
        -0.0,
        1.5,
        -2.5,
        3.0,
        f64::INFINITY,
        f64::NAN,
        1e308,
        2.0f64.powi(63),
    ];
    for k in kinds {
        let mut h = Hb::new();
        let _ = func1(
            &mut h,
            "f",
            &[("a", Prim::F64), ("b", Prim::F64)],
            Some(Prim::F64),
            |h, ps| {
                let (ga, gb) = (h.get(ps[0]), h.get(ps[1]));
                let e = h.op(k, &[ga, gb]);
                h.block(&[], Some(e))
            },
        );
        let u = h.finish();
        for a in values {
            for b in values {
                agree(&u, "f", &[Val::f64(a), Val::f64(b)]);
            }
        }
    }
    for k in [
        OpKind::Sqrt,
        OpKind::Floor,
        OpKind::Ceil,
        OpKind::Trunc,
        OpKind::Round,
        OpKind::RoundEven,
        OpKind::Neg,
        OpKind::Abs,
    ] {
        let mut h = Hb::new();
        let _ = func1(
            &mut h,
            "f",
            &[("a", Prim::F32)],
            Some(Prim::F32),
            |h, ps| {
                let ga = h.get(ps[0]);
                let e = h.op(k, &[ga]);
                h.block(&[], Some(e))
            },
        );
        let u = h.finish();
        for a in [0.5f32, -0.5, 2.5, -3.5, 4.0, f32::NAN, -0.0] {
            agree(&u, "f", &[Val::f32(a)]);
        }
    }
    for pol in [FloatToInt::Error, FloatToInt::Saturate] {
        let mut h = Hb::new();
        let _ = func1(
            &mut h,
            "f",
            &[("a", Prim::F64)],
            Some(Prim::I32),
            |h, ps| {
                let ga = h.get(ps[0]);
                let e = h.opp(
                    Op::new(OpKind::FloatToInt(Prim::I32)).with_float_to_int(pol),
                    &[ga],
                );
                h.block(&[], Some(e))
            },
        );
        let u = h.finish();
        for a in [
            0.0,
            -1.9,
            2.5e9,
            -2.5e9,
            f64::NAN,
            2147483647.9,
            -2147483648.9,
        ] {
            agree(&u, "f", &[Val::f64(a)]);
        }
    }
    // fma, total_cmp, ieee_rem are lowered to their instructions.
    let mut h = Hb::new();
    let _ = func1(
        &mut h,
        "f",
        &[("a", Prim::F64), ("b", Prim::F64)],
        Some(Prim::I8),
        |h, ps| {
            let (a1, b1, a2) = (h.get(ps[0]), h.get(ps[1]), h.get(ps[0]));
            let f = h.op(OpKind::Fma, &[a1, b1, a2]);
            let b2 = h.get(ps[1]);
            let r = h.op(OpKind::IeeeRem, &[f, b2]);
            let a3 = h.get(ps[0]);
            let e = h.op(OpKind::TotalCmp, &[r, a3]);
            h.block(&[], Some(e))
        },
    );
    let u = h.finish();
    let typed = u.check();
    let out = u.lower(&typed);
    let text = out.module().to_string();
    assert!(
        text.contains("fma") && text.contains("ieee_rem") && text.contains("total_cmp"),
        "{text}"
    );
    assert_eq!(
        run_lowered(&out, u.item("f"), &[Val::f64(3.0), Val::f64(2.0)]),
        Out::Ret(Some(u64::from(255u8)))
    );
}

#[test]
fn test_casts_between_scalars() {
    let cases: [(Prim, Prim); 9] = [
        (Prim::I64, Prim::U8),
        (Prim::U8, Prim::I64),
        (Prim::I64, Prim::F64),
        (Prim::F64, Prim::I64),
        (Prim::F64, Prim::F32),
        (Prim::Bool, Prim::I32),
        (Prim::U8, Prim::Char),
        (Prim::Char, Prim::U8),
        (Prim::I64, Prim::Isize),
    ];
    for (from, to) in cases {
        for ov in [Overflow::Error, Overflow::Wrap, Overflow::Trap] {
            for fi in [FloatToInt::Error, FloatToInt::Saturate] {
                let mut h = Hb::new();
                let _ = func1(&mut h, "f", &[("a", from)], Some(to), |h, ps| {
                    let ga = h.get(ps[0]);
                    let ty = h.prim(to);
                    let pol = Policy::NONE.with_overflow(ov).with_float_to_int(fi);
                    let e = h.cast(ga, ty, pol);
                    h.block(&[], Some(e))
                });
                let u = h.finish();
                let args: Vec<Val> = match from {
                    Prim::I64 => [0i64, 1, -1, 255, 256, i64::MAX]
                        .iter()
                        .map(|&v| Val::i64(v))
                        .collect(),
                    Prim::U8 => [0u8, 65, 255].iter().map(|&v| Val::u8(v)).collect(),
                    Prim::F64 => [0.0, -1.5, 3.9, 1e19, f64::NAN]
                        .iter()
                        .map(|&v| Val::f64(v))
                        .collect(),
                    Prim::Bool => vec![Val::bool(true), Val::bool(false)],
                    Prim::Char => vec![Val::u32(65), Val::u32(0x1F600)],
                    _ => vec![],
                };
                for a in args {
                    agree(&u, "f", &[a]);
                }
            }
        }
    }
}

#[test]
fn test_char_from_u32_raises_invalid_char() {
    let mut h = Hb::new();
    let _ = func1(
        &mut h,
        "f",
        &[("a", Prim::U32)],
        Some(Prim::Char),
        |h, ps| {
            let ga = h.get(ps[0]);
            let e = h.op(OpKind::CharFromU32, &[ga]);
            h.block(&[], Some(e))
        },
    );
    let u = h.finish();
    assert_eq!(u.run("f", &[Val::u32(65)]), Out::Ret(Some(65)));
    assert_eq!(u.run("f", &[Val::u32(0xD800)]), error(5));
    for a in [65, 0xD800, 0x11_0000] {
        agree(&u, "f", &[Val::u32(a)]);
    }
}

#[test]
fn test_saturating_shift_of_unsigned_by_negative_signed_amount_raises() {
    let mut h = Hb::new();
    // fn f(a: u32, n: i8) -> u32 { a << n } with shift = saturate
    let _ = func1(
        &mut h,
        "f",
        &[("a", Prim::U32), ("n", Prim::I8)],
        Some(Prim::U32),
        |h, ps| {
            let (ga, gn) = (h.get(ps[0]), h.get(ps[1]));
            let e = h.opp(Op::new(OpKind::Shl).with_shift(Shift::Saturate), &[ga, gn]);
            h.block(&[], Some(e))
        },
    );
    let u = h.finish();
    assert_eq!(u.run("f", &[Val::u32(1), Val::i8(-1)]), error(3));
    assert_eq!(u.run("f", &[Val::u32(1), Val::i8(40)]), Out::Ret(Some(0)));
    assert_eq!(u.run("f", &[Val::u32(1), Val::i8(4)]), Out::Ret(Some(16)));
    for n in [-1i8, 0, 4, 31, 32, 40, i8::MIN] {
        agree(&u, "f", &[Val::u32(1), Val::i8(n)]);
    }
}

#[test]
fn test_trap_mode_traps_with_the_error_code() {
    let u = {
        let mut h = Hb::new();
        let _ = func1(
            &mut h,
            "f",
            &[("a", Prim::I64), ("b", Prim::I64)],
            Some(Prim::I64),
            |h, ps| {
                let (ga, gb) = (h.get(ps[0]), h.get(ps[1]));
                let e = h.op(OpKind::Div, &[ga, gb]);
                h.block(&[], Some(e))
            },
        );
        h.finish()
    };
    assert_eq!(
        u.run_mode("f", &[Val::i64(1), Val::i64(0)], ErrorMode::Trap),
        Out::Trap(2)
    );
    assert_eq!(
        u.run_mode("f", &[Val::i64(i64::MIN), Val::i64(-1)], ErrorMode::Trap),
        Out::Trap(1)
    );
    assert_eq!(u.run("f", &[Val::i64(1), Val::i64(0)]), error(2));
}

#[test]
fn test_numeric_widening_coercions() {
    let mut h = Hb::new();
    // fn f(a: u8, b: i32, c: f32) -> f64 { let x: i64 = b; let y: i16 = a; let z: f64 = c; z + int_to_float(x + y) }
    let _ = func1(
        &mut h,
        "f",
        &[("a", Prim::U8), ("b", Prim::I32), ("c", Prim::F32)],
        Some(Prim::F64),
        |h, ps| {
            let (a, b, c) = (ps[0], ps[1], ps[2]);
            let (x, y, z) = (h.local("x"), h.local("y"), h.local("z"));
            let (tx, ty, tz) = (h.prim(Prim::I64), h.prim(Prim::I64), h.prim(Prim::F64));
            let (gb, ga, gc) = (h.get(b), h.get(a), h.get(c));
            let lx = h.let_(x, Some(tx), Some(gb));
            let ly = h.let_(y, Some(ty), Some(ga));
            let lz = h.let_(z, Some(tz), Some(gc));
            let (gx, gy) = (h.get(x), h.get(y));
            let s = h.op(OpKind::Add, &[gx, gy]);
            let f = h.op(OpKind::IntToFloat(Prim::F64), &[s]);
            let gz = h.get(z);
            let t = h.op(OpKind::Add, &[gz, f]);
            h.block(&[lx, ly, lz], Some(t))
        },
    );
    let mut u = h.finish();
    u.profile = typeck_lang::Profile::static_explicit().with_numeric_widening(true);
    let r = u.run("f", &[Val::u8(200), Val::i32(-5), Val::f32(0.5)]);
    assert_eq!(r, Out::Ret(Some(195.5f64.to_bits())));
    agree(&u, "f", &[Val::u8(200), Val::i32(-5), Val::f32(0.5)]);
}

#[test]
fn test_unit_values_never_functions_and_zero_initialized_lets() {
    let mut h = Hb::new();
    // fn boom(x: i64) -> ! { throw x }
    let t = h.prim(Prim::I64);
    let (px, x) = h.param("x", t);
    let gx = h.get(x);
    let th = h.throw(gx);
    let body = h.block(&[], Some(th));
    let never = h.b.ty(hir_lang::Ty::Never);
    let _ = h.func("boom", &[px], Some(never), body);
    // fn g(u: ()) -> () { u }
    let unit_ty = h.tuple_ty(&[]);
    let (pu, ub) = h.param("u", unit_ty);
    let gu = h.get(ub);
    let body = h.block(&[], Some(gu));
    let unit_ty = h.tuple_ty(&[]);
    let _ = h.func("g", &[pu], Some(unit_ty), body);
    // fn f(k: i64) -> i64 { let z: i64; let p: (i64, i64); g(()); if k > 0 { boom(k) } ; z + p.1 + k }
    let _ = func1(
        &mut h,
        "f",
        &[("k", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let k = ps[0];
            let z = h.local("z");
            let tz = h.prim(Prim::I64);
            let lz = h.let_(z, Some(tz), None);
            let p = h.local("p");
            let (a, b) = (h.prim(Prim::I64), h.prim(Prim::I64));
            let tp = h.tuple_ty(&[a, b]);
            let lp = h.let_(p, Some(tp), None);
            let un = h.unit();
            let cg = h.call("g", &[un]);
            let s1 = h.stmt(cg);
            let (gk, zero) = (h.get(k), h.i64(0));
            let pos = h.op(OpKind::Gt, &[gk, zero]);
            let gk = h.get(k);
            let cb = h.call("boom", &[gk]);
            let cbb = h.block(&[], Some(cb));
            let iff = h.if_(pos, cbb, None);
            let s2 = h.stmt(iff);
            let (gz, gp) = (h.get(z), h.get(p));
            let p1 = h.tfield(gp, 1);
            let s = h.op(OpKind::Add, &[gz, p1]);
            let gk = h.get(k);
            let s = h.op(OpKind::Add, &[s, gk]);
            h.block(&[lz, lp, s1, s2], Some(s))
        },
    );
    let u = h.finish();
    let typed = u.check();
    let out = u.lower(&typed);
    let boom = out.function(u.item("boom")).unwrap();
    assert!(
        out.module()
            .func_decl(boom)
            .unwrap()
            .attrs()
            .contains(ir_lang::Attrs::NORETURN)
    );
    assert_eq!(run_lowered(&out, u.item("f"), &[Val::i64(-3)]), ret_i64(-3));
    assert_eq!(
        run_lowered(&out, u.item("f"), &[Val::i64(4)]),
        thrown_i64(4)
    );
    for k in [-3, 0, 4] {
        agree(&u, "f", &[Val::i64(k)]);
    }
}

#[test]
fn test_module_body_is_a_function() {
    let mut h = Hb::new();
    let a = h.i64(1);
    let c = h.ext_call("trace", &[HostTy::Prim(Prim::I64)], HostTy::Unit, &[a]);
    let s = h.stmt(c);
    let body = h.block(&[s], None);
    let n = h.name("script");
    let items = h.b.list(&[]);
    let m = h.item(Item::new(
        Some(n),
        ItemKind::Module {
            items,
            body: Some(body),
            effects: hir_lang::Effects::NONE,
        },
    ));
    let u = h.finish();
    let typed = u.check();
    let out = u.lower(&typed);
    let f = out.module_body(m).unwrap();
    assert_eq!(
        out.module().func_decl(f).unwrap().name(),
        "script::__module_body"
    );
    let mut it =
        ir_lang::interp::Interpreter::new(out.module(), common::TestHost::default()).unwrap();
    let r = it.call(f, &[]);
    assert_eq!(common::decode(&it, r), Out::Ret(None));
    assert_eq!(it.host_mut().trace, vec![1]);
}

#[test]
fn test_abi_functions_without_body_are_imports_with_their_convention() {
    let mut h = Hb::new();
    let t = h.prim(Prim::I64);
    let (p, _) = h.param("x", t);
    let params = h.b.list(&[p]);
    let ret = h.prim(Prim::I64);
    let n = h.name("puts_i64");
    let c = h.names.intern("C");
    let item = h.item(Item::new(
        Some(n),
        ItemKind::Fn(hir_lang::FnDef {
            params,
            ret: Some(ret),
            abi: Some(c),
            ..hir_lang::FnDef::default()
        }),
    ));
    h.named("puts_i64", item);
    let _ = func1(
        &mut h,
        "f",
        &[("x", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let gx = h.get(ps[0]);
            let c = h.call("puts_i64", &[gx]);
            h.block(&[], Some(c))
        },
    );
    let u = h.finish();
    let typed = u.check();
    let out = u.lower(&typed);
    let f = out.function(item).unwrap();
    let decl = out.module().func_decl(f).unwrap();
    assert_eq!(decl.linkage(), ir_lang::Linkage::Import);
    assert_eq!(
        out.module().signature(decl.sig()).unwrap().conv,
        ir_lang::CallConv::C
    );
}

#[test]
fn test_evaluation_order_of_arguments_fields_and_assignments() {
    let mut h = Hb::new();
    let (xt, yt) = (h.prim(Prim::I64), h.prim(Prim::I64));
    let _ = h.record("P", &[("x", xt), ("y", yt)]);
    // fn t(v: i64) -> i64 { trace(v); v }
    let _ = func1(
        &mut h,
        "t",
        &[("v", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let gv = h.get(ps[0]);
            let c = h.ext_call("trace", &[HostTy::Prim(Prim::I64)], HostTy::Unit, &[gv]);
            let s = h.stmt(c);
            let gv = h.get(ps[0]);
            h.block(&[s], Some(gv))
        },
    );
    // fn sub(a: i64, b: i64) -> i64 { a - b }
    let _ = func1(
        &mut h,
        "sub",
        &[("a", Prim::I64), ("b", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let (ga, gb) = (h.get(ps[0]), h.get(ps[1]));
            let e = h.op(OpKind::Sub, &[ga, gb]);
            h.block(&[], Some(e))
        },
    );
    // fn f() -> i64 { let p = P { y: t(2), x: t(1) }; let q = sub(t(3), t(4)); let a = [0, 0]; a[t(1)] = t(5); p.x - p.y + q + a[1] }
    let _ = func1(&mut h, "f", &[], Some(Prim::I64), |h, _| {
        let t = |h: &mut Hb, v: i64| {
            let a = h.i64(v);
            h.call("t", &[a])
        };
        let ty = t(h, 2);
        let tx = t(h, 1);
        let lit = h.record_lit("P", &[("y", ty), ("x", tx)]);
        let p = h.local("p");
        let lp = h.let_(p, None, Some(lit));
        let t3 = t(h, 3);
        let t4 = t(h, 4);
        let s = h.call("sub", &[t3, t4]);
        let q = h.local("q");
        let lq = h.let_(q, None, Some(s));
        let (z1, z2) = (h.i64(0), h.i64(0));
        let arr = h.array(&[z1, z2]);
        let a = h.local("a");
        let la = h.let_(a, None, Some(arr));
        let ga = h.get(a);
        let t1 = t(h, 1);
        let ai = h.index(ga, t1);
        let t5 = t(h, 5);
        let asg = h.assign(ai, t5);
        let s4 = h.stmt(asg);
        let (gp1, gp2) = (h.get(p), h.get(p));
        let (px, py) = (h.field(gp1, "x"), h.field(gp2, "y"));
        let d = h.op(OpKind::Sub, &[px, py]);
        let gq = h.get(q);
        let d = h.op(OpKind::Add, &[d, gq]);
        let (ga, one) = (h.get(a), h.i64(1));
        let a1 = h.index(ga, one);
        let d = h.op(OpKind::Add, &[d, a1]);
        h.block(&[lp, lq, la, s4], Some(d))
    });
    let u = h.finish();
    let typed = u.check();
    let out = u.lower(&typed);
    assert_eq!(
        common::run_lowered_traced(&out, u.item("f"), &[]),
        (ret_i64(1 - 2 + (3 - 4) + 5), vec![2, 1, 3, 4, 1, 5])
    );
    agree(&u, "f", &[]);
}

#[test]
fn test_lowering_is_deterministic() {
    for seed in 0..20 {
        let p = common::progs::program(seed, 120);
        let typed = p.unit.check();
        let a = p.unit.lower(&typed).module().to_string();
        let b = p.unit.lower(&typed).module().to_string();
        assert_eq!(a, b);
    }
}

#[test]
fn test_ir_text_round_trips_through_ir_lang_parser() {
    for seed in 0..40 {
        let p = common::progs::program(seed, 120);
        let typed = p.unit.check();
        let out = p.unit.lower(&typed);
        let text = out.module().to_string();
        let back = ir_lang::parse(&text).unwrap();
        back.validate().unwrap();
        assert_eq!(back.to_string(), text);
    }
}

#[test]
fn test_unused_items_and_options() {
    let o = Options::new()
        .with_errors(ErrorMode::Trap)
        .with_max_steps(10);
    assert_eq!(o.max_steps(), Some(10));
    let _ = (Lit::Bool(true), BinderKind::Local, Expr::Err);
}

#[test]
fn test_negative_exponent_under_trap_is_the_ops_error() {
    // OPS v2: `pow` with a negative exponent is NegativeExponent (E0006) under
    // every overflow policy but promote, `trap` included (ir-lang's own `pow`
    // would trap; irgen-lang raises the error first).
    let mut h = Hb::new();
    let _ = func1(
        &mut h,
        "f",
        &[("a", Prim::I64), ("e", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let (ga, ge) = (h.get(ps[0]), h.get(ps[1]));
            let e = h.opp(
                Op::new(OpKind::Pow).with_overflow(Overflow::Trap),
                &[ga, ge],
            );
            h.block(&[], Some(e))
        },
    );
    let u = h.finish();
    assert_eq!(u.run("f", &[Val::i64(2), Val::i64(-1)]), error(6));
    assert_eq!(
        u.run_mode("f", &[Val::i64(2), Val::i64(-1)], ErrorMode::Trap),
        Out::Trap(6)
    );
    assert_eq!(u.run("f", &[Val::i64(2), Val::i64(10)]), ret_i64(1024));
    assert_eq!(u.run("f", &[Val::i64(2), Val::i64(64)]), Out::Trap(1));
    for (a, e) in [(2, -1), (2, 10), (2, 64), (-3, 3)] {
        agree(&u, "f", &[Val::i64(a), Val::i64(e)]);
    }
}

#[test]
fn test_more_patterns_bool_char_float_rest_and_shared_or_bindings() {
    let mut h = Hb::new();
    // fn f(a: i64, b: i64) -> i64 {
    //   let t = (a, b, 7);
    //   let (first, .., last) = t;
    //   let k = match (a, b) { (x, 1) | (1, x) => x * 100, _ => 0 };
    //   let c = match b > 0 { true => 1, false => 2 };
    //   let ch = match 'q' { 'a'..='m' => 10, 'n'..='z' => 20, _ => 30 };
    //   let fl = match 0.5 { 0.5 => 1000, _ => 0 };
    //   first + last + k + c + ch + fl
    // }
    let _ = func1(
        &mut h,
        "f",
        &[("a", Prim::I64), ("b", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let (a, b) = (ps[0], ps[1]);
            let (ga, gb, seven) = (h.get(a), h.get(b), h.i64(7));
            let tup = h.tuple(&[ga, gb, seven]);
            let t = h.local("t");
            let lt = h.let_(t, None, Some(tup));
            let (first, last) = (h.local("first"), h.local("last"));
            let (pf, pl) = (h.pat_bind(first), h.pat_bind(last));
            let rp = h.pat_tuple(&[pf, pl], Some(1));
            let gt = h.get(t);
            let l2 = h.let_pat(rp, gt, None);
            // k
            let x = h.local("x");
            let (px1, one1) = (h.pat_bind(x), h.pat_int(1, Prim::I64));
            let alt1 = h.pat_tuple(&[px1, one1], None);
            let (one2, px2) = (h.pat_int(1, Prim::I64), h.pat_bind(x));
            let alt2 = h.pat_tuple(&[one2, px2], None);
            let or = h.pat_or(&[alt1, alt2]);
            let (gx, hundred) = (h.get(x), h.i64(100));
            let m = h.op(OpKind::Mul, &[gx, hundred]);
            let arm1 = h.arm(or, None, m);
            let w = h.pat_wild();
            let z = h.i64(0);
            let arm2 = h.arm(w, None, z);
            let (ga, gb) = (h.get(a), h.get(b));
            let scr = h.tuple(&[ga, gb]);
            let km = h.match_(scr, &[arm1, arm2]);
            let k = h.local("k");
            let lk = h.let_(k, None, Some(km));
            // c
            let (gb, z) = (h.get(b), h.i64(0));
            let pos = h.op(OpKind::Gt, &[gb, z]);
            let pt = h.pat_lit(Lit::Bool(true));
            let pf2 = h.pat_lit(Lit::Bool(false));
            let (one, two) = (h.i64(1), h.i64(2));
            let (ca, cb) = (h.arm(pt, None, one), h.arm(pf2, None, two));
            let cm = h.match_(pos, &[ca, cb]);
            let c = h.local("c");
            let lc = h.let_(c, None, Some(cm));
            // ch
            let q = h.chr('q');
            let (lo1, hi1) = (h.pat_lit(Lit::Char('a')), h.pat_lit(Lit::Char('m')));
            let r1 = h.pat_range(Some(lo1), Some(hi1), true);
            let (lo2, hi2) = (h.pat_lit(Lit::Char('n')), h.pat_lit(Lit::Char('z')));
            let r2 = h.pat_range(Some(lo2), Some(hi2), true);
            let w = h.pat_wild();
            let (ten, twenty, thirty) = (h.i64(10), h.i64(20), h.i64(30));
            let (a1, a2, a3) = (
                h.arm(r1, None, ten),
                h.arm(r2, None, twenty),
                h.arm(w, None, thirty),
            );
            let chm = h.match_(q, &[a1, a2, a3]);
            let ch = h.local("ch");
            let lch = h.let_(ch, None, Some(chm));
            // fl
            let half = h.float(0.5, Prim::F64);
            let fp = h.pat_lit(Lit::Float(
                hir_lang::FloatLit::new(0.5).with_suffix(Prim::F64),
            ));
            let w = h.pat_wild();
            let (thousand, z) = (h.i64(1000), h.i64(0));
            let (f1, f2) = (h.arm(fp, None, thousand), h.arm(w, None, z));
            let flm = h.match_(half, &[f1, f2]);
            let fl = h.local("fl");
            let lfl = h.let_(fl, None, Some(flm));
            let mut s = h.get(first);
            for v in [last, k, c, ch, fl] {
                let g = h.get(v);
                s = h.op(OpKind::Add, &[s, g]);
            }
            h.block(&[lt, l2, lk, lc, lch, lfl], Some(s))
        },
    );
    let mut u = h.finish();
    u.profile = typeck_lang::Profile::static_explicit().with_exhaustive_match(false);
    // a = 5, b = 1: first 5, last 7, k 500, c 1, ch 20, fl 1000
    assert_eq!(
        u.run("f", &[Val::i64(5), Val::i64(1)]),
        ret_i64(5 + 7 + 500 + 1 + 20 + 1000)
    );
    for (a, b) in [(5, 1), (1, 9), (2, 2), (1, 1), (0, -3)] {
        agree(&u, "f", &[Val::i64(a), Val::i64(b)]);
    }
}

#[test]
fn test_aggregate_results_of_labeled_blocks_loops_matches_and_unit_records() {
    let mut h = Hb::new();
    let (xt, yt) = (h.prim(Prim::I64), h.prim(Prim::I64));
    let _ = h.record("P", &[("x", xt), ("y", yt)]);
    let n = h.name("U");
    let unit_rec = h.item(Item::new(
        Some(n),
        ItemKind::Record(hir_lang::RecordDef {
            shape: hir_lang::Shape::Unit,
            ..hir_lang::RecordDef::default()
        }),
    ));
    h.records.push(("U".to_string(), unit_rec));
    h.named("U", unit_rec);
    // global ORIGIN: P = P { x: 3, y: 4 };
    let ty = h.record_ty("P");
    let (x3, y4) = (h.i64(3), h.i64(4));
    let lit = h.record_lit("P", &[("x", x3), ("y", y4)]);
    let n = h.name("ORIGIN");
    let g = h.item(Item::new(
        Some(n),
        ItemKind::Global {
            ty: Some(ty),
            mutable: false,
            init: Some(lit),
        },
    ));
    h.named("ORIGIN", g);
    // fn f(c: i64) -> i64 {
    //   let p = 'b: { if c > 0 { break 'b P { x: c, y: c } }; ORIGIN };
    //   let q = loop { break (p.x, 1) };
    //   let r = match c { 0 => P { x: 9, y: 9 }, _ => p };
    //   let u = U;
    //   let U = u;
    //   p.x + p.y * 10 + q.0 * 100 + r.y * 1000
    // }
    let _ = func1(
        &mut h,
        "f",
        &[("c", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let c = ps[0];
            let blabel = h.label("b");
            let (gc, z) = (h.get(c), h.i64(0));
            let pos = h.op(OpKind::Gt, &[gc, z]);
            let (gc1, gc2) = (h.get(c), h.get(c));
            let pc = h.record_lit("P", &[("x", gc1), ("y", gc2)]);
            let br = h.brk(Some(blabel), Some(pc));
            let brb = h.block(&[], Some(br));
            let iff = h.if_(pos, brb, None);
            let s = h.stmt(iff);
            let origin = h.item_path("ORIGIN");
            let lb = h.labeled(blabel, &[s], Some(origin));
            let p = h.local("p");
            let lp = h.let_(p, None, Some(lb));
            let gp = h.get(p);
            let px = h.field(gp, "x");
            let one = h.i64(1);
            let t = h.tuple(&[px, one]);
            let br = h.brk(None, Some(t));
            let lbody = h.block(&[], Some(br));
            let lp2 = h.loop_(None, lbody, None);
            let q = h.local("q");
            let lq = h.let_(q, None, Some(lp2));
            let zero = h.pat_int(0, Prim::I64);
            let (n9a, n9b) = (h.i64(9), h.i64(9));
            let p9 = h.record_lit("P", &[("x", n9a), ("y", n9b)]);
            let arm0 = h.arm(zero, None, p9);
            let w = h.pat_wild();
            let gp = h.get(p);
            let arm1 = h.arm(w, None, gp);
            let gc = h.get(c);
            let m = h.match_(gc, &[arm0, arm1]);
            let r = h.local("r");
            let lr = h.let_(r, None, Some(m));
            let uv = h.item_path("U");
            let ub = h.local("u");
            let lu = h.let_(ub, None, Some(uv));
            let un = h.name("U");
            let up = h.b.name_path(un, hir_lang::Ns::Pattern);
            h.fixup_path(up, "U");
            let upat = h.b.pat(Pat::Path(up));
            let gu = h.get(ub);
            let lu2 = h.let_pat(upat, gu, None);
            let gp = h.get(p);
            let px = h.field(gp, "x");
            let gp = h.get(p);
            let py = h.field(gp, "y");
            let ten = h.i64(10);
            let py10 = h.op(OpKind::Mul, &[py, ten]);
            let gq = h.get(q);
            let q0 = h.tfield(gq, 0);
            let hundred = h.i64(100);
            let q100 = h.op(OpKind::Mul, &[q0, hundred]);
            let gr = h.get(r);
            let ry = h.field(gr, "y");
            let thousand = h.i64(1000);
            let r1000 = h.op(OpKind::Mul, &[ry, thousand]);
            let s = h.op(OpKind::Add, &[px, py10]);
            let s = h.op(OpKind::Add, &[s, q100]);
            let s = h.op(OpKind::Add, &[s, r1000]);
            h.block(&[lp, lq, lr, lu, lu2], Some(s))
        },
    );
    let mut u = h.finish();
    u.profile = typeck_lang::Profile::static_explicit().with_exhaustive_match(false);
    assert_eq!(u.run("f", &[Val::i64(2)]), ret_i64(2 + 20 + 200 + 2000));
    assert_eq!(u.run("f", &[Val::i64(0)]), ret_i64(3 + 40 + 300 + 9000));
    for c in [0, 2, -1] {
        agree(&u, "f", &[Val::i64(c)]);
    }
}
