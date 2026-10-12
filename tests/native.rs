//! The native end-to-end test (ISSUES P37): HIR lowered by irgen-lang,
//! optimized by opt-lang, compiled by backend-lang to x86-64 machine code
//! (instruction selection, register allocation, emission; isa-lang encodes
//! every instruction), loaded into executable memory, and **run on the
//! processor**, in this process. No Cranelift, no LLVM, no system compiler:
//! every stage is a family crate.
//!
//! Every run is compared with ir-lang's reference interpreter (on the
//! lowered and on the optimized module) and with this crate's independent
//! HIR evaluator (`common/reference.rs`): the result, the thrown value or
//! run-time error, the trap code, and the host trace must all agree, in
//! both error modes, under both the SysV and the Win64 conventions, with
//! and without a frame pointer.
//!
//! What runs natively: the program generator's programs (every integer OPS
//! operation with every policy, conversions, control flow, `match`, calls,
//! records, tuples, and arrays, globals and constants, `throw`/`try`/
//! `catch`/`finally`/`defer` across calls through `invoke` and landing
//! pads, host calls with a trace, host functions that throw, and host GC
//! references across safepoints), and hand-written programs below
//! (recursion, loops, pow, aggregates, exceptions, float `pow` through the
//! runtime).
//!
//! What does not, and why: backend-lang 0.4 refuses float arithmetic other
//! than `pow`, float comparisons, int/float conversions, and float
//! arguments and results (isa-lang has no SSE yet, ISSUES P11), so the
//! generator runs here with floats off, and float `pow` is reached through
//! `bitcast` from and to `i64`. The collector never moves objects (the
//! host's objects are boxes), so stack maps are exercised (spills and
//! reloads) but relocation is not; backend-lang's own tests cover moving.
//!
//! x86-64 Linux and Windows (in-memory loading through `mmap` or
//! `VirtualAlloc`).

#![cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use common::native::{self, Conv, Native, Ran};
use common::{Hb, HostTy, Out, TestHost, Unit, decode, progs, reference};
use hir_lang::{ItemId, Op, OpKind, Overflow, Prim, Shift};
use ir_lang::interp::{Interpreter, Val};
use ir_lang::{FuncId, Module, Type};
use irgen_lang::{ErrorMode, Options};

// ---------------------------------------------------------------- harness

fn mask(ty: Type, v: u64) -> u64 {
    if ty == Type::Bool {
        return v & 1;
    }
    match ty.int_bits() {
        Some(w) if w < 64 => v & ((1u64 << w) - 1),
        _ if ty == Type::F32 => v & 0xFFFF_FFFF,
        _ => v,
    }
}

/// Runs `f` of `m` in ir-lang's interpreter with the tests' host.
fn interp(m: &Module, f: FuncId, args: &[Val]) -> (Out, Vec<i64>) {
    let mut it = Interpreter::new(m, TestHost::default()).expect("valid module");
    let r = it.call(f, args);
    let o = decode(&it, r);
    let trace = core::mem::take(&mut it.host_mut().trace);
    (o, trace)
}

/// Compiles `m` with backend-lang, loads it, and runs `f` natively.
fn run_native(m: &Module, f: FuncId, args: &[u64], conv: Conv, fp: bool) -> (Out, Vec<i64>) {
    let c = backend_lang::compile(m, &conv.options().with_frame_pointer(fp))
        .unwrap_or_else(|e| panic!("backend-lang refused the module: {e}\n{m}"));
    let decl = m.func_decl(f).expect("declared");
    let ret = m
        .signature(decl.sig())
        .and_then(|s| s.returns.first().copied());
    native::reset_host();
    let n = Native::load(&c, conv).expect("loads");
    let out = match n.call(decl.name(), args).expect("runs") {
        Ran::Returned(v) => Out::Ret(ret.map(|t| mask(t, v))),
        Ran::Trap(code) => Out::Trap(code as u32),
        Ran::Unwound(p) => {
            let (tag, bits) = native::record(p).expect("the payload is a host record");
            Out::Thrown { tag, bits }
        }
    };
    (out, native::take_trace())
}

/// What a comparison covered.
#[derive(Default, Debug)]
struct Tally {
    native_runs: u64,
    returned: u64,
    thrown: u64,
    trapped: u64,
}

impl Tally {
    fn add(&mut self, o: &Out) {
        self.native_runs += 1;
        match o {
            Out::Ret(_) => self.returned += 1,
            Out::Thrown { .. } => self.thrown += 1,
            Out::Trap(_) => self.trapped += 1,
        }
    }
}

/// The full comparison for one program and argument list: reference
/// evaluator, interpreter (lowered and optimized), and native (lowered and
/// optimized, SysV and Win64, frame pointer as `fp`), in both error modes.
fn agree_native(
    u: &Unit,
    typed: &typeck_lang::Typed,
    item: ItemId,
    args: &[i64],
    fp: bool,
    tally: &mut Tally,
) -> Vec<Out> {
    let vals: Vec<Val> = args.iter().map(|&a| Val::i64(a)).collect();
    let regs: Vec<u64> = args.iter().map(|&a| a as u64).collect();
    let mut outs = Vec::new();
    for mode in [ErrorMode::Raise, ErrorMode::Trap] {
        let (want, want_trace) = reference::run(u, typed, mode, item, &vals);
        let lowered = u.lower_with(typed, Options::new().with_errors(mode));
        let f = lowered.function(item).expect("lowered");
        let plain = lowered.module().clone();
        let mut optimized = plain.clone();
        let _ = opt_lang::optimize(&mut optimized)
            .unwrap_or_else(|e| panic!("opt-lang failed: {e}\n{plain}"));
        for (what, m) in [("lowered", &plain), ("optimized", &optimized)] {
            let (got, trace) = interp(m, f, &vals);
            assert_eq!(
                (&got, &trace),
                (&want, &want_trace),
                "interpreter ({what}, {mode:?}) vs reference, args {args:?}\n{m}"
            );
            for conv in Conv::ALL {
                let (got, trace) = run_native(m, f, &regs, conv, fp);
                assert_eq!(
                    (&got, &trace),
                    (&want, &want_trace),
                    "native ({what}, {mode:?}, {conv:?}, fp {fp}) vs reference, args {args:?}\n{m}"
                );
                tally.add(&got);
            }
        }
        outs.push(want);
    }
    outs
}

/// [`agree_native`] for a hand-written unit's function `name`, over several
/// argument lists; returns the outcomes under [`ErrorMode::Raise`].
fn agree_all(u: &Unit, name: &str, arg_sets: &[&[i64]]) -> Vec<Out> {
    let typed = u.check();
    let mut tally = Tally::default();
    let mut raised = Vec::new();
    for (k, args) in arg_sets.iter().enumerate() {
        let outs = agree_native(u, &typed, u.item(name), args, k % 2 == 1, &mut tally);
        raised.push(outs[0].clone());
    }
    assert_eq!(tally.native_runs, 8 * arg_sets.len() as u64);
    raised
}

/// `fn name(params..) -> ret { body }`.
fn func(
    h: &mut Hb,
    name: &str,
    params: &[(&str, Prim)],
    ret: Option<Prim>,
    f: impl FnOnce(&mut Hb, &[hir_lang::BinderId]) -> hir_lang::ExprId,
) -> ItemId {
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

fn count(default: u64) -> u64 {
    std::env::var("IRGEN_NATIVE_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ------------------------------------------------------ generated programs

fn sweep(seeds: core::ops::Range<u64>, size: i64) -> Tally {
    let mut tally = Tally::default();
    for seed in seeds {
        // GC references in every other program: the host's objects never
        // move, so these check spills and reloads around safepoints, not
        // relocation.
        let p = progs::native_program(seed, size, seed % 2 == 0);
        let typed = p.unit.check();
        let _ = agree_native(&p.unit, &typed, p.entry, &p.args, seed % 4 < 2, &mut tally);
    }
    tally
}

#[test]
fn test_generated_programs_run_natively_and_agree() {
    let n = count(150);
    let before = native::landings();
    let t = sweep(0..n, 60);
    assert_eq!(t.native_runs, 8 * n);
    // Exceptions crossed native frames into landing pads.
    assert!(native::landings() > before);
    // The sweep reaches every kind of outcome on the processor.
    assert!(t.returned > 0 && t.thrown > 0 && t.trapped > 0, "{t:?}");
}

#[test]
fn test_larger_generated_programs_run_natively_and_agree() {
    let n = count(20);
    let t = sweep(50_000..50_000 + n, 300);
    assert_eq!(t.native_runs, 8 * n);
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

    /// Any seed: the generated program agrees natively (8 native runs).
    #[test]
    fn prop_native_runs_agree(seed in 0u64..u64::MAX) {
        let t = sweep(seed..seed + 1, 80);
        proptest::prop_assert_eq!(t.native_runs, 8);
    }
}

/// A long run for release checks: `cargo test --release --test native --
/// --ignored` (3,000 programs, 24,000 native runs; set `IRGEN_SEED0` to
/// move the window).
#[test]
#[ignore = "long: run with --ignored in release"]
fn test_native_long_run() {
    let start: u64 = std::env::var("IRGEN_SEED0")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);
    let mut total = Tally::default();
    for (k, size) in [40, 120, 300].into_iter().enumerate() {
        let lo = start + 1_000 * k as u64;
        let t = sweep(lo..lo + 1_000, size);
        total.native_runs += t.native_runs;
        total.returned += t.returned;
        total.thrown += t.thrown;
        total.trapped += t.trapped;
    }
    eprintln!("native long run: {total:?}");
    assert_eq!(total.native_runs, 24_000);
}

// ---------------------------------------------------- hand-written programs

/// `fib(n)` by double recursion: calls, a deep native call stack, and the
/// `check`ed additions of the default `overflow = error` policy.
#[test]
fn test_recursive_fib() {
    let mut h = Hb::new();
    // fn fib(n: i64) -> i64 { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }
    let _ = func(
        &mut h,
        "fib",
        &[("n", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let n = ps[0];
            let (gn, two) = (h.get(n), h.i64(2));
            let small = h.op(OpKind::Lt, &[gn, two]);
            let gn = h.get(n);
            let then = h.block(&[], Some(gn));
            let (gn, one) = (h.get(n), h.i64(1));
            let n1 = h.op(OpKind::Sub, &[gn, one]);
            let c1 = h.call("fib", &[n1]);
            let (gn, two) = (h.get(n), h.i64(2));
            let n2 = h.op(OpKind::Sub, &[gn, two]);
            let c2 = h.call("fib", &[n2]);
            let sum = h.op(OpKind::Add, &[c1, c2]);
            let other = h.block(&[], Some(sum));
            let e = h.if_(small, then, Some(other));
            h.block(&[], Some(e))
        },
    );
    let u = h.finish();
    let outs = agree_all(&u, "fib", &[&[0], &[1], &[2], &[10], &[20], &[25]]);
    assert_eq!(outs[3], Out::Ret(Some(55)));
    assert_eq!(outs[4], Out::Ret(Some(6_765)));
    assert_eq!(outs[5], Out::Ret(Some(75_025)));
}

/// A counted loop with `continue` and `break`, and a wrapping accumulator:
/// `sum of i*i for i in 0..n, i odd`, stopping early past a bound.
#[test]
fn test_loops_with_break_and_continue() {
    let mut h = Hb::new();
    let _ = func(
        &mut h,
        "f",
        &[("n", Prim::I64), ("bound", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let (n, bound) = (ps[0], ps[1]);
            let i = h.local("i");
            let z = h.i64(0);
            let li = h.let_(i, None, Some(z));
            let acc = h.local("acc");
            let z = h.i64(0);
            let la = h.let_(acc, None, Some(z));
            let l = h.label("l");
            // if i >= n { break }
            let (gi, gn) = (h.get(i), h.get(n));
            let done = h.op(OpKind::Ge, &[gi, gn]);
            let br = h.brk(Some(l), None);
            let brb = h.block(&[], Some(br));
            let s1 = h.if_(done, brb, None);
            let s1 = h.stmt(s1);
            // if i % 2 == 0 { continue }
            let (gi, two) = (h.get(i), h.i64(2));
            let r = h.op(OpKind::Rem, &[gi, two]);
            let z = h.i64(0);
            let even = h.op(OpKind::Eq, &[r, z]);
            let ct = h.cont(Some(l));
            let ctb = h.block(&[], Some(ct));
            let s2 = h.if_(even, ctb, None);
            let s2 = h.stmt(s2);
            // acc = acc +% i * i
            let (gi, gi2) = (h.get(i), h.get(i));
            let sq = h.wrap(OpKind::Mul, &[gi, gi2]);
            let ga = h.get(acc);
            let s = h.wrap(OpKind::Add, &[ga, sq]);
            let s3 = h.set(acc, s);
            let s3 = h.stmt(s3);
            // if acc > bound { break }
            let (ga, gb) = (h.get(acc), h.get(bound));
            let over = h.op(OpKind::Gt, &[ga, gb]);
            let br = h.brk(Some(l), None);
            let brb = h.block(&[], Some(br));
            let s4 = h.if_(over, brb, None);
            let s4 = h.stmt(s4);
            let body = h.block(&[s1, s2, s3, s4], None);
            let (gi, one) = (h.get(i), h.i64(1));
            let inc = h.wrap(OpKind::Add, &[gi, one]);
            let step = h.set(i, inc);
            let lp = h.loop_(Some(l), body, Some(step));
            let lp = h.stmt(lp);
            let ga = h.get(acc);
            h.block(&[li, la, lp], Some(ga))
        },
    );
    let u = h.finish();
    let outs = agree_all(
        &u,
        "f",
        &[
            &[0, 100],
            &[10, 1_000_000],
            &[1_000, i64::MAX],
            &[100_000, 50],
        ],
    );
    // 1 + 9 + 25 + 49 + 81
    assert_eq!(outs[1], Out::Ret(Some(165)));
    // 1 + 9 + 25 + 49 = 84 > 50: stops there.
    assert_eq!(outs[3], Out::Ret(Some(84)));
}

/// Integer `pow` under every overflow policy: a negative exponent is
/// `NegativeExponent` (E0006) under all of them (ir-lang 2.0.0-alpha.3's
/// `check`, ISSUES P36), overflow follows the policy.
#[test]
fn test_integer_pow_under_every_policy() {
    for o in [Overflow::Error, Overflow::Wrap, Overflow::Trap] {
        let mut h = Hb::new();
        let _ = func(
            &mut h,
            "p",
            &[("a", Prim::I64), ("e", Prim::I64)],
            Some(Prim::I64),
            |h, ps| {
                let (ga, ge) = (h.get(ps[0]), h.get(ps[1]));
                let e = h.opp(Op::new(OpKind::Pow).with_overflow(o), &[ga, ge]);
                h.block(&[], Some(e))
            },
        );
        let u = h.finish();
        let outs = agree_all(
            &u,
            "p",
            &[
                &[2, 10],
                &[-3, 3],
                &[2, -1],
                &[7, 0],
                &[2, 63],
                &[2, 64],
                &[-2, 63],
            ],
        );
        assert_eq!(outs[0], Out::Ret(Some(1024)));
        assert_eq!(outs[1], Out::Ret(Some((-27i64) as u64)));
        assert_eq!(outs[2], common::error(6), "{o:?}");
        assert_eq!(outs[3], Out::Ret(Some(1)));
    }
}

/// Float `pow` through the runtime's `__lexersketch_pow_f64` (bound to
/// ir-lang's `ls_pow` on bit patterns, as backend-lang's tests bind it):
/// the operands and the result cross as `i64` bit patterns, because
/// backend-lang 0.4 passes no float arguments or results (ISSUES P11).
#[test]
fn test_float_pow_through_the_runtime() {
    let mut h = Hb::new();
    let _ = func(
        &mut h,
        "fpow",
        &[("a", Prim::I64), ("b", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let (ga, gb) = (h.get(ps[0]), h.get(ps[1]));
            let fa = h.op(OpKind::Bitcast(Prim::F64), &[ga]);
            let fb = h.op(OpKind::Bitcast(Prim::F64), &[gb]);
            let p = h.op(OpKind::Pow, &[fa, fb]);
            let bits = h.op(OpKind::Bitcast(Prim::I64), &[p]);
            h.block(&[], Some(bits))
        },
    );
    let u = h.finish();
    let b = |x: f64| x.to_bits() as i64;
    let cases: Vec<[i64; 2]> = vec![
        [b(2.0), b(10.0)],
        [b(2.5), b(-1.5)],
        [b(-8.0), b(1.0 / 3.0)],
        [b(0.0), b(-1.0)],
        [b(f64::NAN), b(0.0)],
        [b(1.0), b(f64::NAN)],
        [b(-1.0), b(f64::INFINITY)],
        [b(1e300), b(2.0)],
    ];
    let sets: Vec<&[i64]> = cases.iter().map(|c| &c[..]).collect();
    let outs = agree_all(&u, "fpow", &sets);
    assert_eq!(outs[0], Out::Ret(Some(1024f64.to_bits())));
    assert_eq!(
        outs[1],
        Out::Ret(Some(ir_lang::ls_pow(2.5, -1.5).to_bits()))
    );
}

/// Records and arrays in memory: a record passed by address and returned
/// through the hidden result pointer, array element writes and reads, and a
/// bounds-checked index (E0102).
#[test]
fn test_records_and_arrays() {
    let mut h = Hb::new();
    let (xt, yt) = (h.prim(Prim::I64), h.prim(Prim::I64));
    let _ = h.record("P", &[("x", xt), ("y", yt)]);
    // fn mid(a: P, b: P) -> P { P { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 } }
    let (at, bt) = (h.record_ty("P"), h.record_ty("P"));
    let (pa, a) = h.param("a", at);
    let (pb, b) = h.param("b", bt);
    let half = |h: &mut Hb, f: &str| {
        let (ga, gb) = (h.get(a), h.get(b));
        let (fa, fb) = (h.field(ga, f), h.field(gb, f));
        let s = h.op(OpKind::Add, &[fa, fb]);
        let two = h.i64(2);
        h.op(OpKind::Div, &[s, two])
    };
    let (mx, my) = (half(&mut h, "x"), half(&mut h, "y"));
    let lit = h.record_lit("P", &[("x", mx), ("y", my)]);
    let body = h.block(&[], Some(lit));
    let rt = h.record_ty("P");
    let _ = h.func("mid", &[pa, pb], Some(rt), body);
    // fn f(i: i64, v: i64) -> i64 {
    //   let arr = [10, 20, 30, 40]; arr[i] = v;
    //   let m = mid(P { x: arr[0], y: arr[1] }, P { x: arr[2], y: arr[3] });
    //   m.x * 1000 + m.y
    // }
    let _ = func(
        &mut h,
        "f",
        &[("i", Prim::I64), ("v", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let (i, v) = (ps[0], ps[1]);
            let arr = h.local("arr");
            let es: Vec<_> = [10, 20, 30, 40].iter().map(|&k| h.i64(k)).collect();
            let lit = h.array(&es);
            let la = h.let_(arr, None, Some(lit));
            let (gar, gi) = (h.get(arr), h.get(i));
            let place = h.index(gar, gi);
            let gv = h.get(v);
            let st = h.assign(place, gv);
            let st = h.stmt(st);
            let elem = |h: &mut Hb, k: u64| {
                let g = h.get(arr);
                let ix = h.uint(k, Prim::Usize);
                h.index(g, ix)
            };
            let (e0, e1) = (elem(h, 0), elem(h, 1));
            let p1 = h.record_lit("P", &[("x", e0), ("y", e1)]);
            let (e2, e3) = (elem(h, 2), elem(h, 3));
            let p2 = h.record_lit("P", &[("x", e2), ("y", e3)]);
            let m = h.local("m");
            let c = h.call("mid", &[p1, p2]);
            let lm = h.let_(m, None, Some(c));
            let gm = h.get(m);
            let mx = h.field(gm, "x");
            let k = h.i64(1000);
            let hi = h.op(OpKind::Mul, &[mx, k]);
            let gm = h.get(m);
            let my = h.field(gm, "y");
            let r = h.op(OpKind::Add, &[hi, my]);
            h.block(&[la, st, lm], Some(r))
        },
    );
    let u = h.finish();
    let outs = agree_all(&u, "f", &[&[0, 10], &[3, 0], &[4, 1], &[-1, 1], &[1, 99]]);
    assert_eq!(outs[0], Out::Ret(Some(20 * 1000 + 30)));
    assert_eq!(outs[1], Out::Ret(Some(20 * 1000 + 10)));
    assert_eq!(
        outs[2],
        common::error(irgen_lang::rt::E_INDEX_OUT_OF_BOUNDS)
    );
    assert_eq!(
        outs[3],
        common::error(irgen_lang::rt::E_INDEX_OUT_OF_BOUNDS)
    );
}

/// Exceptions across native frames: a thrown value and a run-time error
/// unwind out of a callee (through `__lexersketch_resume` and the
/// unwinder) into the caller's landing pad, `catch` tests the type, and
/// `finally` runs on every path, observed through the host trace; an
/// exception no handler catches leaves the entry.
#[test]
fn test_exceptions_unwind_across_native_frames() {
    let mut h = Hb::new();
    // fn inner(x: i64) -> i64 {
    //   if x < 0 { throw x }            // a thrown i64
    //   x * 1_000_000_000_000           // overflow: the ArithOverflow error
    // }
    let _ = func(
        &mut h,
        "inner",
        &[("x", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let x = ps[0];
            let (gx, z) = (h.get(x), h.i64(0));
            let neg = h.op(OpKind::Lt, &[gx, z]);
            let gx = h.get(x);
            let th = h.throw(gx);
            let thb = h.block(&[], Some(th));
            let s = h.if_(neg, thb, None);
            let s = h.stmt(s);
            let (gx, k) = (h.get(x), h.i64(1_000_000_000_000));
            let m = h.op(OpKind::Mul, &[gx, k]);
            h.block(&[s], Some(m))
        },
    );
    // fn middle(x: i64) -> i64 { defer trace(1); inner(x) + 1 }
    let _ = func(
        &mut h,
        "middle",
        &[("x", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let one = h.i64(1);
            let tr = h.ext_call("trace", &[HostTy::Prim(Prim::I64)], HostTy::Unit, &[one]);
            let trs = h.stmt(tr);
            let db = h.block(&[trs], None);
            let d = h.defer(db);
            let gx = h.get(ps[0]);
            let c = h.call("inner", &[gx]);
            let one = h.i64(1);
            let r = h.op(OpKind::Add, &[c, one]);
            h.block(&[d], Some(r))
        },
    );
    // fn outer(x: i64, catch_it: i64) -> i64 {
    //   if catch_it == 0 { return middle(x) }      // uncaught: leaves the entry
    //   try { middle(x) } catch (e: i64) { e * 10 } catch _ { -7 } finally { trace(2) }
    // }
    let _ = func(
        &mut h,
        "outer",
        &[("x", Prim::I64), ("catch_it", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let (x, k) = (ps[0], ps[1]);
            let (gk, z) = (h.get(k), h.i64(0));
            let no = h.op(OpKind::Eq, &[gk, z]);
            let gx = h.get(x);
            let c = h.call("middle", &[gx]);
            let rt = h.ret(Some(c));
            let rtb = h.block(&[], Some(rt));
            let s = h.if_(no, rtb, None);
            let s = h.stmt(s);
            let gx = h.get(x);
            let c = h.call("middle", &[gx]);
            let body = h.block(&[], Some(c));
            let e = h.local("e");
            let (ge, ten) = (h.get(e), h.i64(10));
            let m = h.wrap(OpKind::Mul, &[ge, ten]);
            let arm1 = h.catch_typed(Prim::I64, Some(e), None, m);
            let m7 = h.i64(-7);
            let arm2 = h.catch_all(m7);
            let two = h.i64(2);
            let tr = h.ext_call("trace", &[HostTy::Prim(Prim::I64)], HostTy::Unit, &[two]);
            let trs = h.stmt(tr);
            let fin = h.block(&[trs], None);
            let t = h.try_(body, &[arm1, arm2], Some(fin));
            h.block(&[s], Some(t))
        },
    );
    let u = h.finish();
    let before = native::landings();
    let outs = agree_all(
        &u,
        "outer",
        &[
            &[5, 1],
            &[-4, 1],
            &[10_000_000, 1],
            &[5, 0],
            &[-4, 0],
            &[10_000_000, 0],
        ],
    );
    assert_eq!(outs[0], Out::Ret(Some(5_000_000_000_001)));
    assert_eq!(outs[1], Out::Ret(Some((-40i64) as u64)));
    assert_eq!(outs[2], Out::Ret(Some((-7i64) as u64)));
    assert_eq!(outs[4], common::thrown_i64(-4));
    assert_eq!(outs[5], common::error(1));
    // The caught cases entered landing pads on the processor (in `middle`,
    // for its `defer`, and in `outer`).
    assert!(native::landings() > before);
}

/// The unwinder restores the callee-saved registers every unwound frame
/// saved: many values live across an `invoke` (more than the scratch
/// registers hold) are read in the landing pad and after it, while the
/// throwing callees clobber registers of their own.
#[test]
fn test_values_live_across_an_unwinding_call_survive() {
    let mut h = Hb::new();
    // fn thrower(x: i64) -> i64 {
    //   let a.. = x * k..;   (pressure in the callee too)
    //   if x % 2 == 1 { throw a1 ^ a2 ^ .. } a1 + a2 + ..
    // }
    let _ = func(
        &mut h,
        "thrower",
        &[("x", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let x = ps[0];
            let mut lets = Vec::new();
            let mut vars = Vec::new();
            for k in 0..12i64 {
                let v = h.local("a");
                let (gx, kk) = (h.get(x), h.i64(k * 7 + 3));
                let m = h.wrap(OpKind::Mul, &[gx, kk]);
                let kk = h.i64(k);
                let m = h.wrap(OpKind::Add, &[m, kk]);
                lets.push(h.let_(v, None, Some(m)));
                vars.push(v);
            }
            let mut acc = h.get(vars[0]);
            for &v in &vars[1..] {
                let gv = h.get(v);
                acc = h.op(OpKind::Xor, &[acc, gv]);
            }
            let (gx, two) = (h.get(x), h.i64(2));
            let r = h.op(OpKind::Rem, &[gx, two]);
            let one = h.i64(1);
            let odd = h.op(OpKind::Eq, &[r, one]);
            let th = h.throw(acc);
            let thb = h.block(&[], Some(th));
            let s = h.if_(odd, thb, None);
            lets.push(h.stmt(s));
            let mut sum = h.get(vars[0]);
            for &v in &vars[1..] {
                let gv = h.get(v);
                sum = h.wrap(OpKind::Add, &[sum, gv]);
            }
            h.block(&lets, Some(sum))
        },
    );
    // fn f(x: i64) -> i64 {
    //   let b.. = x + k..;
    //   let r = try { thrower(x) } catch (e: i64) { e ^ b0 ^ b1 .. };
    //   r +% b0 *% 3 +% b1 *% 5 ..
    // }
    let _ = func(
        &mut h,
        "f",
        &[("x", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let x = ps[0];
            let mut stmts = Vec::new();
            let mut vars = Vec::new();
            for k in 0..14i64 {
                let v = h.local("b");
                let (gx, kk) = (h.get(x), h.i64(1_000 * k + 11));
                let m = h.wrap(OpKind::Mul, &[gx, kk]);
                stmts.push(h.let_(v, None, Some(m)));
                vars.push(v);
            }
            let gx = h.get(x);
            let c = h.call("thrower", &[gx]);
            let body = h.block(&[], Some(c));
            let e = h.local("e");
            let mut acc = h.get(e);
            for &v in &vars {
                let gv = h.get(v);
                acc = h.op(OpKind::Xor, &[acc, gv]);
            }
            let arm = h.catch_typed(Prim::I64, Some(e), None, acc);
            let t = h.try_(body, &[arm], None);
            let r = h.local("r");
            stmts.push(h.let_(r, None, Some(t)));
            let mut total = h.get(r);
            for (k, &v) in vars.iter().enumerate() {
                let gv = h.get(v);
                let kk = h.i64(2 * k as i64 + 3);
                let m = h.wrap(OpKind::Mul, &[gv, kk]);
                total = h.wrap(OpKind::Add, &[total, m]);
            }
            h.block(&stmts, Some(total))
        },
    );
    let u = h.finish();
    let before = native::landings();
    let _ = agree_all(&u, "f", &[&[1], &[2], &[12_345], &[-77], &[i64::MAX], &[0]]);
    assert!(native::landings() > before);
}

/// A host function that throws (`host_check` unwinds on multiples of 3)
/// reaches the caller's landing pad through the unwinder, with a GC
/// reference live across the call.
#[test]
fn test_host_exceptions_and_references() {
    let mut h = Hb::new();
    // fn f(x: i64) -> i64 {
    //   let o = obj_new(x);
    //   let r = try { host_check(x); obj_get(o) } catch (e: i64) { e + 100 };
    //   obj_set(o, r); obj_get(o) + 1
    // }
    let _ = func(
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
            let gx = h.get(x);
            let hc = h.ext_call(
                "host_check",
                &[HostTy::Prim(Prim::I64)],
                HostTy::Unit,
                &[gx],
            );
            let s = h.stmt(hc);
            let go = h.get(o);
            let og = h.ext_call("obj_get", &[HostTy::Obj], HostTy::Prim(Prim::I64), &[go]);
            let body = h.block(&[s], Some(og));
            let e = h.local("e");
            let (ge, k) = (h.get(e), h.i64(100));
            let sum = h.op(OpKind::Add, &[ge, k]);
            let arm = h.catch_typed(Prim::I64, Some(e), None, sum);
            let t = h.try_(body, &[arm], None);
            let r = h.local("r");
            let lr = h.let_(r, None, Some(t));
            let (go, gr) = (h.get(o), h.get(r));
            let set = h.ext_call(
                "obj_set",
                &[HostTy::Obj, HostTy::Prim(Prim::I64)],
                HostTy::Unit,
                &[go, gr],
            );
            let ss = h.stmt(set);
            let go = h.get(o);
            let og = h.ext_call("obj_get", &[HostTy::Obj], HostTy::Prim(Prim::I64), &[go]);
            let one = h.i64(1);
            let total = h.op(OpKind::Add, &[og, one]);
            h.block(&[lo, lr, ss], Some(total))
        },
    );
    let u = h.finish();
    let before = native::landings();
    let outs = agree_all(&u, "f", &[&[4], &[9], &[0], &[-3]]);
    assert!(native::landings() > before);
    assert_eq!(outs[0], Out::Ret(Some(5)));
    assert_eq!(outs[1], Out::Ret(Some(110)));
}

/// OPS policies on the processor: division by zero as error and as trap,
/// `MIN / -1`, and shifts under `error`, `mask`, and `saturate`.
#[test]
fn test_ops_policies() {
    let mut h = Hb::new();
    // fn f(a: i64, b: i64, which: i64) -> i64 { match which { .. } }
    let _ = func(
        &mut h,
        "f",
        &[("a", Prim::I64), ("b", Prim::I64), ("w", Prim::I64)],
        Some(Prim::I64),
        |h, ps| {
            let (a, b, w) = (ps[0], ps[1], ps[2]);
            let ops = [
                Op::new(OpKind::Div),
                Op::new(OpKind::Div).with_div_zero(hir_lang::DivZero::Trap),
                Op::new(OpKind::FloorMod),
                Op::new(OpKind::Shl).with_shift(Shift::Error),
                Op::new(OpKind::Shl).with_shift(Shift::Mask),
                Op::new(OpKind::Shr).with_shift(Shift::Saturate),
                Op::new(OpKind::Mul).with_overflow(Overflow::Trap),
                Op::new(OpKind::Sub).with_overflow(Overflow::Wrap),
            ];
            let mut arms = Vec::new();
            for (k, op) in ops.into_iter().enumerate() {
                let pat = h.pat_int(k as i64, Prim::I64);
                let (ga, gb) = (h.get(a), h.get(b));
                let e = h.opp(op, &[ga, gb]);
                arms.push(h.arm(pat, None, e));
            }
            let w0 = h.pat_wild();
            let z = h.i64(0);
            arms.push(h.arm(w0, None, z));
            let gw = h.get(w);
            let m = h.match_(gw, &arms);
            h.block(&[], Some(m))
        },
    );
    let u = h.finish();
    let mut sets: Vec<[i64; 3]> = Vec::new();
    for w in 0..8 {
        for (a, b) in [
            (7, 2),
            (-7, 2),
            (7, 0),
            (i64::MIN, -1),
            (1, 63),
            (1, 64),
            (-1, -1),
            (i64::MAX, 2),
        ] {
            sets.push([a, b, w]);
        }
    }
    let sets: Vec<&[i64]> = sets.iter().map(|s| &s[..]).collect();
    let _ = agree_all(&u, "f", &sets);
}
