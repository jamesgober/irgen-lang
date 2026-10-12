//! Hostile and oversized input: deep nesting lowers in bounded native stack,
//! every budget turns into an error instead of an allocation or a hang, and
//! nothing panics.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Hb, HostTy, Unit};
use hir_lang::{Item, ItemKind, OpKind, Overflow, Prim};
use ir_lang::interp::Val;
use irgen_lang::{Limit, LowerError, Lowerer, Options};
use proptest::prelude::*;

fn lower(u: &Unit, options: Options) -> Result<irgen_lang::Lowered, LowerError> {
    let typed = u.check();
    Lowerer::new(options).lower(&u.hir, &typed, &u.names)
}

/// Runs `f` on a thread with a small stack, so recursion over the input
/// would overflow it.
fn small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn test_deeply_nested_blocks_ifs_and_operations_lower_iteratively() {
    const DEPTH: usize = 100_000;
    // { { { ... 1 ... } } } nested DEPTH deep, inside if c { .. } else { .. } chains.
    let mut h = Hb::new();
    let t = h.prim(Prim::I64);
    let (p, c) = h.param("c", t);
    let mut e = h.i64(1);
    for k in 0..DEPTH {
        e = match k % 3 {
            0 => h.block(&[], Some(e)),
            1 => {
                let one = h.i64(1);
                h.wrap(OpKind::Add, &[e, one])
            }
            _ => {
                let (gc, z) = (h.get(c), h.i64(0));
                let cond = h.op(OpKind::Gt, &[gc, z]);
                let other = h.i64(0);
                let ob = h.block(&[], Some(other));
                let tb = h.block(&[], Some(e));
                h.if_(cond, tb, Some(ob))
            }
        };
    }
    let rt = h.prim(Prim::I64);
    let _ = h.func("f", &[p], Some(rt), e);
    let u = h.finish();
    let typed = u.check();
    let out = small_stack(move || {
        let r = irgen_lang::lower(&u.hir, &typed, &u.names).map(|o| o.stats());
        (r, u)
    });
    let (stats, _u) = out;
    assert!(stats.unwrap().steps > DEPTH as u64);
}

#[test]
fn test_long_straight_line_bodies_and_wide_matches() {
    let mut h = Hb::new();
    let t = h.prim(Prim::I64);
    let (p, x) = h.param("x", t);
    let mut stmts = Vec::new();
    for _ in 0..50_000 {
        let (gx, one) = (h.get(x), h.i64(1));
        let inc = h.wrap(OpKind::Add, &[gx, one]);
        let a = h.set(x, inc);
        stmts.push(h.stmt(a));
    }
    // match x { 0 => 0, 1 => 1, ..., 4999 => 4999, _ => -1 }
    let mut arms = Vec::new();
    for k in 0..5_000 {
        let pat = h.pat_int(k, Prim::I64);
        let body = h.i64(k);
        arms.push(h.arm(pat, None, body));
    }
    let w = h.pat_wild();
    let m1 = h.i64(-1);
    arms.push(h.arm(w, None, m1));
    let gx = h.get(x);
    let m = h.match_(gx, &arms);
    let body = h.block(&stmts, Some(m));
    let rt = h.prim(Prim::I64);
    let _ = h.func("f", &[p], Some(rt), body);
    let u = h.finish();
    let typed = u.check();
    let out = u.lower(&typed);
    assert!(out.module().to_string().contains("switch"));
    assert_eq!(
        common::run_lowered(&out, u.item("f"), &[Val::i64(-49_990)]),
        common::ret_i64(10)
    );
}

#[test]
fn test_step_budget_is_an_error() {
    let p = common::progs::program(42, 200);
    let r = lower(&p.unit, Options::new().with_max_steps(10));
    assert_eq!(
        r.err(),
        Some(LowerError::BudgetExceeded {
            limit: Limit::Steps
        })
    );
}

#[test]
fn test_huge_global_and_stack_arrays_are_budget_errors() {
    // global G: [u8; 2^40] = [0; 2^40]
    let mut h = Hb::new();
    let e = h.prim(Prim::U8);
    let ty = h.array_ty(e, 1 << 40);
    let z = h.uint(0, Prim::U8);
    let init = h.repeat(z, 1 << 40);
    let n = h.name("G");
    let _ = h.item(Item::new(
        Some(n),
        ItemKind::Global {
            ty: Some(ty),
            mutable: false,
            init: Some(init),
        },
    ));
    let u = h.finish();
    assert_eq!(
        lower(&u, Options::new()).err(),
        Some(LowerError::BudgetExceeded {
            limit: Limit::GlobalBytes
        })
    );
    // A small cap refuses a small global too.
    let mut h = Hb::new();
    let ty = h.prim(Prim::I64);
    let v = h.i64(1);
    let n = h.name("G");
    let _ = h.item(Item::new(
        Some(n),
        ItemKind::Global {
            ty: Some(ty),
            mutable: false,
            init: Some(v),
        },
    ));
    let u = h.finish();
    assert_eq!(
        lower(&u, Options::new().with_max_global_bytes(4)).err(),
        Some(LowerError::BudgetExceeded {
            limit: Limit::GlobalBytes
        })
    );

    // fn f() { let a = [0u8; 2^33]; }
    let mut h = Hb::new();
    let z = h.uint(0, Prim::U8);
    let arr = h.repeat(z, 1 << 33);
    let a = h.local("a");
    let l = h.let_(a, None, Some(arr));
    let body = h.block(&[l], None);
    let _ = h.func("f", &[], None, body);
    let u = h.finish();
    assert_eq!(
        lower(&u, Options::new()).err(),
        Some(LowerError::BudgetExceeded {
            limit: Limit::FrameSize
        })
    );
}

#[test]
fn test_statepoint_budget_is_an_error() {
    let mut h = Hb::new();
    let t = h.prim(Prim::I64);
    let (p, v) = h.param("v", t);
    let o = h.local("o");
    let gv = h.get(v);
    let mk = h.ext_call("obj_new", &[HostTy::Prim(Prim::I64)], HostTy::Obj, &[gv]);
    let lo = h.let_(o, None, Some(mk));
    let poll = h.ext_call("gc_poll", &[], HostTy::Unit, &[]);
    let s = h.stmt(poll);
    let go = h.get(o);
    let get = h.ext_call("obj_get", &[HostTy::Obj], HostTy::Prim(Prim::I64), &[go]);
    let body = h.block(&[lo, s], Some(get));
    let rt = h.prim(Prim::I64);
    let _ = h.func("f", &[p], Some(rt), body);
    let u = h.finish();
    assert_eq!(
        lower(&u, Options::new().with_statepoint_budget(0)).err(),
        Some(LowerError::BudgetExceeded {
            limit: Limit::Statepoints
        })
    );
    assert!(lower(&u, Options::new()).is_ok());
}

fn nested_tuple_unit(depth: usize) -> Unit {
    // let ((((x, 0), 1), ...)) = ((((v, 0), 1), ...)) else { return -1 }; x
    let mut h = Hb::new();
    let t = h.prim(Prim::I64);
    let (p, v) = h.param("v", t);
    let x = h.local("x");
    let mut pat = h.pat_bind(x);
    let mut val = h.get(v);
    for k in 0..depth {
        let lit = h.pat_int(k as i64, Prim::I64);
        pat = h.pat_tuple(&[pat, lit], None);
        let kk = h.i64(k as i64);
        val = h.tuple(&[val, kk]);
    }
    let m1 = h.i64(-1);
    let r = h.ret(Some(m1));
    let rb = h.block(&[], Some(r));
    let l = h.let_pat(pat, val, Some(rb));
    let gx = h.get(x);
    let body = h.block(&[l], Some(gx));
    let rt = h.prim(Prim::I64);
    let _ = h.func("f", &[p], Some(rt), body);
    h.finish()
}

#[test]
fn test_deeply_nested_tuple_types_and_patterns() {
    // Deep: lowers in a small stack (types, values, and patterns are all
    // walked iteratively).
    let u = nested_tuple_unit(3_000);
    let typed = u.check();
    let out = small_stack(move || irgen_lang::lower(&u.hir, &typed, &u.names).map(|o| o.stats()));
    assert!(out.is_ok());
    // Shallower, and run: value semantics copy each level (no SROA here).
    let u = nested_tuple_unit(60);
    assert_eq!(u.run("f", &[Val::i64(7)]), common::ret_i64(7));
    common::reference::agree(&u, "f", &[Val::i64(7)]);
}

#[test]
fn test_lowering_time_is_linear_in_the_body() {
    fn unit(n: usize) -> Unit {
        let mut h = Hb::new();
        let t = h.prim(Prim::I64);
        let (p, x) = h.param("x", t);
        let mut stmts = Vec::new();
        for k in 0..n {
            let (gx, kk) = (h.get(x), h.i64(k as i64));
            let e = if k % 2 == 0 {
                h.wrap(OpKind::Add, &[gx, kk])
            } else {
                h.op(OpKind::Xor, &[gx, kk])
            };
            let a = h.set(x, e);
            stmts.push(h.stmt(a));
        }
        let gx = h.get(x);
        let body = h.block(&stmts, Some(gx));
        let rt = h.prim(Prim::I64);
        let _ = h.func("f", &[p], Some(rt), body);
        h.finish()
    }
    let time = |n: usize| {
        let u = unit(n);
        let typed = u.check();
        let start = std::time::Instant::now();
        let out = u.lower(&typed);
        let d = start.elapsed();
        assert_eq!(out.stats().functions, 1);
        d
    };
    let small = time(100_000);
    let large = time(400_000);
    // 4x the input: linear is about 4x; allow generous noise, but a
    // quadratic path (16x) fails.
    assert!(
        large.as_secs_f64() < small.as_secs_f64() * 10.0 + 0.5,
        "100k: {small:?}, 400k: {large:?}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Any generated program under any budget lowers or reports an error;
    /// it never panics.
    #[test]
    fn prop_any_budget_lowers_or_errors(seed in any::<u64>(), steps in 0u64..4000, sp in 0usize..64) {
        let p = common::progs::program(seed, 150);
        let typed = p.unit.check();
        let options = Options::new().with_max_steps(steps).with_statepoint_budget(sp).with_max_global_bytes(steps);
        let _ = Lowerer::new(options).lower(&p.unit.hir, &typed, &p.unit.names);
    }
}

#[test]
fn test_unused_overflow_import() {
    let _ = Overflow::Wrap;
}

#[test]
fn test_deep_cleanups_and_jumps_stay_within_budget() {
    // D nested `'l_k: loop { try { .. } finally { } }`, and in the innermost
    // body a `break 'l_k` for every k: each break crosses k cleanups, so the
    // routed exits grow as D^2. That work is charged to the step budget.
    fn unit(depth: usize) -> Unit {
        let mut h = Hb::new();
        let labels: Vec<_> = (0..depth).map(|_| h.label("l")).collect();
        let mut stmts = Vec::new();
        for &l in &labels {
            let br = h.brk(Some(l), None);
            stmts.push(h.stmt(br));
        }
        let mut body = h.block(&stmts, None);
        for &l in labels.iter().rev() {
            let fin = h.block(&[], None);
            let t = h.try_(body, &[], Some(fin));
            let ts = h.stmt(t);
            let lb = h.block(&[ts], None);
            let lp = h.loop_(Some(l), lb, None);
            let s = h.stmt(lp);
            body = h.block(&[s], None);
        }
        let _ = h.func("f", &[], None, body);
        h.finish()
    }
    let small = unit(40);
    assert!(lower(&small, Options::new()).is_ok());
    let big = unit(3_000);
    let start = std::time::Instant::now();
    let r = small_stack(move || {
        let typed = big.check();
        Lowerer::new(Options::new())
            .lower(&big.hir, &typed, &big.names)
            .map(|o| o.stats())
    });
    assert!(
        matches!(
            r,
            Ok(_)
                | Err(LowerError::BudgetExceeded {
                    limit: Limit::Steps
                })
        ),
        "{r:?}"
    );
    assert!(start.elapsed().as_secs() < 60);
}
