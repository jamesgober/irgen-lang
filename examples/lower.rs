//! The Tier-1 path: build HIR, type-check it, lower it to ir-lang, print the
//! SSA, and run it in ir-lang's reference interpreter.
//!
//! ```text
//! cargo run --example lower
//! ```
//!
//! The program is
//!
//! ```text
//! fn sum_to(n: i64) -> i64 {
//!     let s = 0;
//!     let i = 0;
//!     loop [step: i = i + 1] {
//!         if i > n { break }
//!         s = s + i            // overflow = error: a `check`
//!     }
//!     s
//! }
//! ```

use hir_lang::{
    Binder, BinderKind, Builder, Expr, FnDef, Item, ItemKind, Name, Op, OpKind, Overflow, Param,
    Prim, Stmt, Ty,
};
use intern_lang::Interner;
use ir_lang::interp::{self, Val};
use typeck_lang::{Profile, check};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut names = Interner::new();
    let mut b = Builder::new();
    let local = |b: &mut Builder, names: &mut Interner, s: &str, kind| {
        b.binder(Binder::new(Name::new(names.intern(s)), kind).with_mutable(true))
    };

    // fn sum_to(n: i64) -> i64
    let n = local(&mut b, &mut names, "n", BinderKind::Param);
    let n_pat = b.bind(n);
    let i64_ty = b.ty(Ty::Prim(Prim::I64));
    let param = b.param(Param {
        ty: Some(i64_ty),
        ..Param::new(n_pat)
    });

    // let s = 0; let i = 0;
    let s = local(&mut b, &mut names, "s", BinderKind::Local);
    let i = local(&mut b, &mut names, "i", BinderKind::Local);
    let zero_s = b.lit(hir_lang::Lit::Int(
        hir_lang::IntLit::new(0).with_suffix(Prim::I64),
    ));
    let zero_i = b.lit(hir_lang::Lit::Int(
        hir_lang::IntLit::new(0).with_suffix(Prim::I64),
    ));
    let (s_pat, i_pat) = (b.bind(s), b.bind(i));
    let let_s = b.let_stmt(s_pat, Some(zero_s));
    let let_i = b.let_stmt(i_pat, Some(zero_i));

    // if i > n { break }
    let (ui, un) = (b.use_binder(i), b.use_binder(n));
    let past = b.op(OpKind::Gt, &[ui, un]);
    let brk = b.expr(Expr::Break {
        label: None,
        value: None,
    });
    let then = b.block(&[], Some(brk));
    let exit = b.expr(Expr::If {
        cond: past,
        then,
        else_: None,
    });
    let exit = b.expr_stmt(exit);

    // s = s + i
    let (ts, us, ui) = (b.use_binder(s), b.use_binder(s), b.use_binder(i));
    let add = b.op(OpKind::Add, &[us, ui]);
    let assign = b.expr(Expr::Assign {
        target: ts,
        op: None,
        value: add,
    });
    let assign = b.expr_stmt(assign);
    let body = b.block(&[exit, assign], None);

    // step: i = i + 1 (wrapping)
    let (ti, ui) = (b.use_binder(i), b.use_binder(i));
    let one = b.lit(hir_lang::Lit::Int(
        hir_lang::IntLit::new(1).with_suffix(Prim::I64),
    ));
    let inc = b.op_with(
        Op::new(OpKind::Add).with_overflow(Overflow::Wrap),
        &[ui, one],
    );
    let step = b.expr(Expr::Assign {
        target: ti,
        op: None,
        value: inc,
    });
    let lp = b.expr(Expr::Loop {
        label: None,
        body,
        step: Some(step),
    });
    let lp = b.stmt(Stmt::Expr(lp));
    let result = b.use_binder(s);
    let fn_body = b.block(&[let_s, let_i, lp], Some(result));
    let params = b.list(&[param]);
    let ret = b.ty(Ty::Prim(Prim::I64));
    let sum_to = b.item(Item::new(
        Some(Name::new(names.intern("sum_to"))),
        ItemKind::Fn(FnDef {
            params,
            ret: Some(ret),
            body: Some(fn_body),
            ..FnDef::default()
        }),
    ));
    let root = b.module(None, &[sum_to]);
    let hir = b.finish(root)?;

    // Type-check, lower, print.
    let typed = check(&hir, &Profile::static_explicit()).unwrap();
    let lowered = irgen_lang::lower(&hir, &typed, &names)?;
    println!("{}", lowered.module());
    println!("stats: {:?}", lowered.stats());

    // Run it.
    let f = lowered.function(sum_to).ok_or("not lowered")?;
    let outcome = interp::run(lowered.module(), f, &[Val::i64(100)])?;
    println!("sum_to(100) = {outcome:?}");
    Ok(())
}
