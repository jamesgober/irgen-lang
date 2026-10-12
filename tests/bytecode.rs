//! The differential against the bytecode path: the same HIR runs through
//! irgen-lang + ir-lang's interpreter (the native path's IR) and through
//! bcgen-lang + bvm-lang (HIR -> LSB -> VM), and both must agree on the
//! result, the raised error, the thrown value, the trap, and the trace.
//!
//! bcgen-lang compiles the dynamic profile (every value is an LSB `dyn`) and
//! refuses declared records (its 0.5), so the programs are restricted to
//! `i64`, `bool`, and `f64`, where static and dynamic semantics coincide:
//! OPS operations with every policy, locals, `if`, counted loops with
//! labeled jumps, labeled blocks, `match`, calls, globals and constants,
//! `throw`, `try`/`catch`/`finally`, `defer`, and a host trace.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use bvm_lang::{Host, Limits, Program, Value, Vm, VmError};
use common::{Out, progs};
use hir_lang::{ItemKind, Prim, Ty};
use ir_lang::interp::Val;
use irgen_lang::rt;
use proptest::prelude::*;

/// A result both paths can produce.
#[derive(Clone, Debug, PartialEq)]
enum R {
    Int(i64),
    Bool(bool),
    /// Float bits, every NaN one NaN.
    Float(u64),
    Unit,
    Raised(u32),
    Thrown(i64),
    Trap(u32),
}

fn float(f: f64) -> R {
    R::Float(if f.is_nan() {
        f64::NAN.to_bits()
    } else {
        f.to_bits()
    })
}

fn native(p: &progs::Program) -> (R, Vec<i64>) {
    let typed = p.unit.check();
    let out = p.unit.lower(&typed);
    let args: Vec<Val> = p.args.iter().map(|&a| Val::i64(a)).collect();
    let (o, trace) = common::run_lowered_traced(&out, p.entry, &args);
    let ret = match p.unit.hir.item(p.entry).kind {
        ItemKind::Fn(def) => def.ret.map(|t| *p.unit.hir.ty(t)),
        _ => None,
    };
    let r = match o {
        Out::Ret(None) => R::Unit,
        Out::Ret(Some(bits)) => match ret {
            Some(Ty::Prim(Prim::Bool)) => R::Bool(bits != 0),
            Some(Ty::Prim(Prim::F64)) => float(f64::from_bits(bits)),
            _ => R::Int(bits as i64),
        },
        Out::Thrown {
            tag: rt::TAG_ERROR,
            bits,
        } => R::Raised(bits as u32),
        Out::Thrown { tag, bits } if Some(tag) == rt::tag(Prim::I64) => R::Thrown(bits as i64),
        Out::Thrown { tag, bits } => panic!("thrown tag {tag} bits {bits}"),
        Out::Trap(c) => R::Trap(c),
    };
    (r, trace)
}

fn bytecode(p: &progs::Program) -> (R, Vec<i64>) {
    let out = match bcgen_lang::compile(&p.unit.hir, &p.unit.names) {
        Ok(o) => o,
        Err(e) => panic!("bcgen-lang refused: {e}"),
    };
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut host = Host::new();
    let t = trace.clone();
    let _ = host.register("host", "trace", move |_, args| {
        if let Some(Value::Int(i)) = args.first() {
            if let Ok(mut v) = t.lock() {
                v.push(*i);
            }
        }
        Ok(Value::Nil)
    });
    let program = Program::load(out.module().clone(), &host).expect("loads");
    let mut vm = Vm::with_limits(
        &program,
        Limits::new().with_fuel(50_000_000).with_depth(400),
    );
    let f = out.function(p.entry).expect("compiled");
    let args: Vec<Value> = p.args.iter().map(|&a| Value::Int(a)).collect();
    let r = match vm.run(f, &args) {
        Ok(Value::Int(i)) => R::Int(i),
        Ok(Value::Bool(b)) => R::Bool(b),
        Ok(Value::Float(x)) => float(x),
        Ok(Value::Nil) => R::Unit,
        Ok(other) => panic!("unexpected value {other:?}"),
        Err(VmError::Raised { kind, .. }) => R::Raised(kind.code()),
        Err(VmError::Thrown {
            value: Value::Int(i),
            ..
        }) => R::Thrown(i),
        Err(VmError::Trap { kind, .. }) => R::Trap(kind.code()),
        Err(e) => panic!("unexpected VM error: {e:?}"),
    };
    let trace = trace.lock().map(|v| v.clone()).unwrap_or_default();
    (r, trace)
}

fn agree(seed: u64, size: i64) {
    let p = progs::scalar_program(seed, size);
    let (a, ta) = native(&p);
    let (b, tb) = bytecode(&p);
    assert_eq!(
        (&a, &ta),
        (&b, &tb),
        "seed {seed}: native path (left) and bytecode path (right) disagree\n{}",
        hir_lang::print(&p.unit.hir, &p.unit.names)
    );
}

#[test]
fn test_bytecode_path_sweep() {
    for seed in 0..300u64 {
        agree(seed, 80);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn prop_native_and_bytecode_paths_agree(seed in any::<u64>()) {
        agree(seed, 150);
    }
}

/// A long run: `cargo test --release --test bytecode -- --ignored`.
#[test]
#[ignore = "long: run with --ignored in release"]
fn test_bytecode_path_long_run() {
    for seed in 2_000_000..2_010_000u64 {
        agree(seed, [60, 150, 300][(seed % 3) as usize]);
    }
}
