//! Lowering throughput at realistic scale: generated programs of about 100k
//! and 1M HIR nodes (functions with locals, loops, matches, aggregates,
//! exceptions, host calls, and GC references), and a straight-line body.
//! The type check runs once in setup; only `lower` is timed.
//!
//! `checked_chain`: one function of `x = x + k` under the default
//! `overflow = error`, so every addition is a `check` whose error edge joins
//! the function's one error block. That is the shape ir-lang
//! 2.0.0-alpha.3's validator handles in quadratic time (ISSUES P43); the
//! validation `lower` ends with dominates these numbers until ir-lang
//! fixes it.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code, missing_docs)]

#[path = "../tests/common/mod.rs"]
mod common;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use hir_lang::{IdKind, OpKind, Prim};

fn nodes(u: &common::Unit) -> u64 {
    [
        IdKind::Item,
        IdKind::Expr,
        IdKind::Stmt,
        IdKind::Pat,
        IdKind::Ty,
        IdKind::Path,
    ]
    .iter()
    .map(|&k| u.hir.count(k) as u64)
    .sum()
}

/// A generated program with about `target` HIR nodes.
fn generated(target: u64) -> common::Unit {
    // Functions generated at size 300 average about 90 nodes.
    let fns = (target / 90).max(1);
    common::progs::program_with(7, Some(fns), 300).unit
}

/// `x = x + k` (checked: `overflow = error`), `n` times.
fn checked_chain(n: usize) -> common::Unit {
    let mut h = common::Hb::new();
    let t = h.prim(Prim::I64);
    let (p, x) = h.param("x", t);
    let mut stmts = Vec::with_capacity(n);
    for k in 0..n {
        let (gx, kk) = (h.get(x), h.i64((k % 7) as i64));
        let e = h.op(OpKind::Add, &[gx, kk]);
        let a = h.set(x, e);
        stmts.push(h.stmt(a));
    }
    let gx = h.get(x);
    let body = h.block(&stmts, Some(gx));
    let rt = h.prim(Prim::I64);
    let _ = h.func("f", &[p], Some(rt), body);
    h.finish()
}

/// `x = x + k` / `x = x ^ k`, `n` times.
fn straight(n: usize) -> common::Unit {
    let mut h = common::Hb::new();
    let t = h.prim(Prim::I64);
    let (p, x) = h.param("x", t);
    let mut stmts = Vec::with_capacity(n);
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

fn bench_lower(c: &mut Criterion) {
    let mut group = c.benchmark_group("lower");
    group.sample_size(10);
    for (name, unit) in [
        ("generated_100k", generated(100_000)),
        ("generated_1m", generated(1_000_000)),
        ("straight_100k", straight(100_000 / 6)),
        ("straight_1m", straight(1_000_000 / 6)),
        ("checked_chain_5k", checked_chain(5_000)),
        ("checked_chain_20k", checked_chain(20_000)),
    ] {
        let typed = unit.check();
        let n = nodes(&unit);
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::new(name, n), &unit, |b, u| {
            b.iter(|| irgen_lang::lower(&u.hir, &typed, &u.names).unwrap());
        });
    }
    group.finish();
}

criterion_group!(benches, bench_lower);
criterion_main!(benches);
