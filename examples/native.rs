//! The native pipeline end to end, every stage a family crate: HIR built
//! here, type-checked by typeck-lang, lowered by irgen-lang to ir-lang SSA,
//! optimized by opt-lang, compiled by backend-lang to x86-64 machine code
//! (isa-lang encodes every instruction), then loaded into executable memory
//! and run on the processor. No Cranelift, no LLVM, no system compiler.
//!
//! ```text
//! cargo run --example native
//! ```
//!
//! The program is
//!
//! ```text
//! fn fib(n: i64) -> i64 { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }
//! ```
//!
//! opt-lang and backend-lang are dev-dependencies of irgen-lang: the
//! library itself only produces the ir-lang module. Running the code needs
//! a loader; this example reuses the one the native end-to-end test uses
//! (`tests/common/native.rs`: `mmap`/`VirtualAlloc`, an entry thunk, an
//! unwinder over backend-lang's landing-pad records), so it runs on x86-64
//! Linux and Windows and only compiles elsewhere.

use hir_lang::{
    Binder, BinderKind, Builder, Def, Expr, FnDef, Item, ItemKind, Name, Ns, OpKind, Param, Prim,
    Res, Ty,
};
use intern_lang::Interner;
use ir_lang::interp::{self, Val};
use typeck_lang::{Profile, check};

#[cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
#[path = "../tests/common/native.rs"]
mod native;

fn int(b: &mut Builder, v: u64) -> hir_lang::ExprId {
    b.lit(hir_lang::Lit::Int(
        hir_lang::IntLit::new(v).with_suffix(Prim::I64),
    ))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- HIR
    let mut names = Interner::new();
    let mut b = Builder::new();
    let fib_name = Name::new(names.intern("fib"));
    let n = b.binder(Binder::new(Name::new(names.intern("n")), BinderKind::Param));
    let n_pat = b.bind(n);
    let i64_ty = b.ty(Ty::Prim(Prim::I64));
    let param = b.param(Param {
        ty: Some(i64_ty),
        ..Param::new(n_pat)
    });
    // if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
    let (un, two) = (b.use_binder(n), int(&mut b, 2));
    let small = b.op(OpKind::Lt, &[un, two]);
    let un = b.use_binder(n);
    let then = b.block(&[], Some(un));
    let call = |b: &mut Builder, k: u64| {
        let (un, kk) = (b.use_binder(n), int(b, k));
        let arg = b.op(OpKind::Sub, &[un, kk]);
        let path = b.name_path(fib_name, Ns::Value);
        let callee = b.expr(Expr::Path(path));
        (b.call(callee, &[arg]), path)
    };
    let (c1, p1) = call(&mut b, 1);
    let (c2, p2) = call(&mut b, 2);
    let sum = b.op(OpKind::Add, &[c1, c2]);
    let other = b.block(&[], Some(sum));
    let body = b.expr(Expr::If {
        cond: small,
        then,
        else_: Some(other),
    });
    let body = b.block(&[], Some(body));
    let params = b.list(&[param]);
    let ret = b.ty(Ty::Prim(Prim::I64));
    let fib = b.item(Item::new(
        Some(fib_name),
        ItemKind::Fn(FnDef {
            params,
            ret: Some(ret),
            body: Some(body),
            ..FnDef::default()
        }),
    ));
    let root = b.module(None, &[fib]);
    let mut hir = b.finish(root)?;
    // The recursive calls name `fib` itself (what a resolver would record).
    let d = hir.def(Def::Item(fib));
    hir.resolve(p1, Res::Def(d))?;
    hir.resolve(p2, Res::Def(d))?;

    // ---- typeck-lang, irgen-lang, opt-lang
    let typed = check(&hir, &Profile::static_explicit())
        .map_err(|e| format!("type check failed: {e:?}"))?;
    let lowered = irgen_lang::lower(&hir, &typed, &names)?;
    let f = lowered.function(fib).ok_or("not lowered")?;
    let mut module = lowered.into_module();
    let stats = opt_lang::optimize(&mut module)?;
    println!("{module}");
    println!("opt-lang: {stats:?}");

    // ---- backend-lang: x86-64 machine code for this host's convention
    let compiled = backend_lang::compile(&module, &backend_lang::Options::host())?;
    let symbol = module
        .func_decl(f)
        .ok_or("no declaration")?
        .name()
        .to_string();
    println!(
        "backend-lang: {} bytes of x86-64 code, functions {:?}, imports {:?}",
        compiled.code().len(),
        compiled
            .functions()
            .iter()
            .map(|f| (f.name(), f.offset(), f.size()))
            .collect::<Vec<_>>(),
        compiled.imports(),
    );

    // ---- the reference: ir-lang's interpreter on the same module (fib(30)
    // makes 1.6 million calls: more fuel than the default)
    let limits = interp::Limits {
        fuel: 1 << 32,
        ..interp::Limits::default()
    };
    let mut vm = interp::Interpreter::with_limits(&module, interp::NoHost, limits)?;
    let want = vm.call(f, &[Val::i64(30)])?;
    println!("interpreter: fib(30) = {want:?}");
    if want != interp::Outcome::Return(vec![Val::i64(832_040)]) {
        return Err("the interpreter's result differs".into());
    }

    // ---- run it
    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
    {
        let conv = if cfg!(windows) {
            native::Conv::Win64
        } else {
            native::Conv::SysV
        };
        let loaded = native::Native::load(&compiled, conv)?;
        let got = loaded.call(&symbol, &[30])?;
        println!("native ({conv:?}): fib(30) = {got:?}");
        if got != native::Ran::Returned(832_040) {
            return Err("the native result differs".into());
        }
    }
    #[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", windows))))]
    println!("not run: the example's loader supports x86-64 Linux and Windows ({symbol})");
    Ok(())
}
