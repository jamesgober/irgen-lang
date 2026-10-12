//! Differential tests: generated programs run through irgen-lang + ir-lang's
//! reference interpreter and through the independent HIR evaluator in
//! `common/reference.rs`, in both error modes, must agree on the result,
//! the thrown value or run-time error, the trap code, and the host trace.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{progs, reference};
use ir_lang::interp::Val;
use proptest::prelude::*;

fn check_seed(seed: u64, size: i64) {
    let p = progs::program(seed, size);
    let typed = p.unit.check();
    let args: Vec<Val> = p.args.iter().map(|&a| Val::i64(a)).collect();
    reference::agree_typed(&p.unit, &typed, p.entry, &args);
}

#[test]
fn test_differential_sweep_of_seeds() {
    for seed in 0..400u64 {
        check_seed(seed, 60);
    }
}

#[test]
fn test_differential_larger_programs() {
    for seed in 10_000..10_060u64 {
        check_seed(seed, 400);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_lowered_programs_agree_with_the_reference(seed in any::<u64>()) {
        check_seed(seed, 120);
    }
}

/// A long run for release checks: `cargo test --release --test differential
/// -- --ignored` (20,000 programs; set `IRGEN_SEED0` to move the window).
#[test]
#[ignore = "long: run with --ignored in release"]
fn test_differential_long_run() {
    let start: u64 = std::env::var("IRGEN_SEED0")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);
    for seed in start..start + 20_000 {
        let size = [40, 120, 300][(seed % 3) as usize];
        check_seed(seed, size);
    }
}
