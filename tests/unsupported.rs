//! Every construct outside the lowered subset is refused with the
//! `Construct` that names it, at the node that uses it (never a panic and
//! never a silently wrong module).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Hb, HostTy};
use hir_lang::{
    Arg, ArgKind, Binder, BinderKind, Builder, Closure, Def, DefId, Effects, Expr, FnDef,
    GenericParam, Generics, Item, ItemKind, Lit, Name, Ns, Param, Pat, Prim, RecordDef, Res, Shape,
    Stmt, Ty, UnitId,
};
use intern_lang::Interner;
use irgen_lang::{Construct, LowerError};
use typeck_lang::{Checker, Profile, TypeId};

fn lower_err(u: &common::Unit, typed: &typeck_lang::Typed) -> LowerError {
    match irgen_lang::lower(&u.hir, typed, &u.names) {
        Ok(_) => panic!("lowered a unit outside the subset"),
        Err(e) => e,
    }
}

fn construct(u: &common::Unit) -> Construct {
    let typed = u.check();
    match lower_err(u, &typed) {
        LowerError::Unsupported { construct, .. } => construct,
        other => panic!("expected Unsupported, got {other}"),
    }
}

/// `fn main() -> ret { stmts; tail }` built by `f`.
fn main_fn(h: &mut Hb, ret: Option<Prim>, f: impl FnOnce(&mut Hb) -> hir_lang::ExprId) {
    let body = f(h);
    let ret = ret.map(|p| h.prim(p));
    let _ = h.func("main", &[], ret, body);
}

#[test]
fn test_dynamic_profile_is_refused() {
    let mut h = Hb::new();
    main_fn(&mut h, None, |h| h.block(&[], None));
    let u = h.finish();
    let typed = u.check_with(Profile::dynamic());
    assert!(matches!(
        lower_err(&u, &typed),
        LowerError::Unsupported {
            construct: Construct::DynamicProfile,
            ..
        }
    ));
}

#[test]
fn test_closures_are_refused() {
    let mut h = Hb::new();
    main_fn(&mut h, None, |h| {
        let t = h.prim(Prim::I64);
        let (p, x) = h.param("x", t);
        let gx = h.get(x);
        let params = h.b.list(&[p]);
        let c = h.b.expr(Expr::Closure(Closure {
            params,
            ..Closure::new(gx)
        }));
        let f = h.local("f");
        let l = h.let_(f, None, Some(c));
        h.block(&[l], None)
    });
    assert_eq!(construct(&h.finish()), Construct::Closure);
}

#[test]
fn test_calls_of_generic_functions_are_refused() {
    let mut h = Hb::new();
    // fn id<T>(x: T) -> T { x }
    let tn = h.name("T");
    let tb = h.b.binder(Binder::new(tn, BinderKind::TypeParam));
    let tp = |h: &mut Hb| {
        let p = h.b.resolved_path(tn, Ns::Type, Res::Local(tb));
        h.b.ty(Ty::Path(p))
    };
    let pt = tp(&mut h);
    let (p, x) = h.param("x", pt);
    let gx = h.get(x);
    let body = h.block(&[], Some(gx));
    let rt = tp(&mut h);
    let generics = Generics {
        params: h.b.list(&[GenericParam::new(tb)]),
        preds: hir_lang::List::EMPTY,
    };
    let params = h.b.list(&[p]);
    let n = h.name("id");
    let id = h.item(Item::new(
        Some(n),
        ItemKind::Fn(FnDef {
            generics,
            params,
            ret: Some(rt),
            body: Some(body),
            ..FnDef::default()
        }),
    ));
    h.named("id", id);
    main_fn(&mut h, Some(Prim::I64), |h| {
        let one = h.i64(1);
        let c = h.call("id", &[one]);
        h.block(&[], Some(c))
    });
    let u = h.finish();
    assert_eq!(construct(&u), Construct::Generic);
    // The generic function itself is not lowered; nothing else refers to it.
}

#[test]
fn test_async_functions_are_refused() {
    let mut h = Hb::new();
    let body = h.block(&[], None);
    let n = h.name("a");
    let _ = h.item(Item::new(
        Some(n),
        ItemKind::Fn(FnDef {
            effects: Effects::ASYNC,
            body: Some(body),
            ..FnDef::default()
        }),
    ));
    assert_eq!(construct(&h.finish()), Construct::Coroutine);
}

#[test]
fn test_strings_null_and_references_are_refused() {
    let mut h = Hb::new();
    main_fn(&mut h, None, |h| {
        let s = h.b.str_lit("hi");
        let x = h.local("s");
        let l = h.let_(x, None, Some(s));
        h.block(&[l], None)
    });
    assert_eq!(construct(&h.finish()), Construct::String);

    let mut h = Hb::new();
    main_fn(&mut h, None, |h| {
        let one = h.i64(1);
        let x = h.local("x");
        let l = h.let_(x, None, Some(one));
        let gx = h.get(x);
        let r = h.b.expr(Expr::Borrow {
            kind: hir_lang::BorrowKind::Shared,
            expr: gx,
        });
        let y = h.local("y");
        let l2 = h.let_(y, None, Some(r));
        h.block(&[l, l2], None)
    });
    assert_eq!(construct(&h.finish()), Construct::Reference);

    let mut h = Hb::new();
    main_fn(&mut h, None, |h| {
        let i = h.prim(Prim::I64);
        let t = h.b.ty(Ty::Nullable(i));
        let n = h.b.lit(Lit::Null);
        let x = h.local("x");
        let l = h.let_(x, Some(t), Some(n));
        h.block(&[l], None)
    });
    assert_eq!(construct(&h.finish()), Construct::Nullable);
}

#[test]
fn test_function_values_and_host_values_are_refused() {
    let mut h = Hb::new();
    let body = h.block(&[], None);
    let _ = h.func("g", &[], None, body);
    main_fn(&mut h, None, |h| {
        let g = h.item_path("g");
        let x = h.local("f");
        let l = h.let_(x, None, Some(g));
        h.block(&[l], None)
    });
    assert_eq!(construct(&h.finish()), Construct::FunctionValue);
}

#[test]
fn test_gc_references_in_aggregates_are_refused() {
    let mut h = Hb::new();
    main_fn(&mut h, None, |h| {
        let one = h.i64(1);
        let o = h.ext_call("obj_new", &[HostTy::Prim(Prim::I64)], HostTy::Obj, &[one]);
        let two = h.i64(2);
        let t = h.tuple(&[o, two]);
        let x = h.local("t");
        let l = h.let_(x, None, Some(t));
        h.block(&[l], None)
    });
    assert_eq!(construct(&h.finish()), Construct::GcRefInMemory);
}

#[test]
fn test_named_arguments_and_defaults_are_refused() {
    let mut h = Hb::new();
    let t = h.prim(Prim::I64);
    let (p, x) = h.param("x", t);
    let gx = h.get(x);
    let body = h.block(&[], Some(gx));
    let rt = h.prim(Prim::I64);
    let _ = h.func("g", &[p], Some(rt), body);
    main_fn(&mut h, Some(Prim::I64), |h| {
        let one = h.i64(1);
        let id = h.ident("x");
        let args = h.b.list(&[Arg {
            kind: ArgKind::Named(id),
            value: one,
            place: false,
        }]);
        let n = h.name("g");
        let path = h.b.name_path(n, Ns::Value);
        h.fixup_path(path, "g");
        let callee = h.b.expr(Expr::Path(path));
        let c = h.b.expr(Expr::Call { callee, args });
        h.block(&[], Some(c))
    });
    assert_eq!(construct(&h.finish()), Construct::CallShape);

    let mut h = Hb::new();
    let t = h.prim(Prim::I64);
    let n = h.name("x");
    let x = h.b.binder(Binder::new(n, BinderKind::Param));
    let pat = h.b.bind(x);
    let one = h.i64(1);
    let p = h.b.param(Param {
        ty: Some(t),
        default: Some(one),
        ..Param::new(pat)
    });
    let gx = h.get(x);
    let body = h.block(&[], Some(gx));
    let rt = h.prim(Prim::I64);
    let _ = h.func("g", &[p], Some(rt), body);
    assert_eq!(construct(&h.finish()), Construct::CallShape);
}

#[test]
fn test_slice_patterns_untyped_catches_and_aggregate_throws_are_refused() {
    let mut h = Hb::new();
    main_fn(&mut h, None, |h| {
        let (a, b) = (h.i64(1), h.i64(2));
        let arr = h.array(&[a, b]);
        let (x, y) = (h.local("x"), h.local("y"));
        let (px, py) = (h.pat_bind(x), h.pat_bind(y));
        let prefix = h.b.list(&[px, py]);
        let sp = h.b.pat(Pat::Slice { prefix, rest: None });
        let l = h.let_pat(sp, arr, None);
        h.block(&[l], None)
    });
    assert_eq!(construct(&h.finish()), Construct::Pattern);

    let mut h = Hb::new();
    main_fn(&mut h, None, |h| {
        let one = h.i64(1);
        let th = h.throw(one);
        let body = h.block(&[], Some(th));
        let e = h.local("e");
        let pe = h.pat_bind(e);
        let u = h.unit();
        let arm = h.arm(pe, None, u);
        let t = h.try_(body, &[arm], None);
        h.block(&[], Some(t))
    });
    assert_eq!(construct(&h.finish()), Construct::DynamicCatch);

    let mut h = Hb::new();
    main_fn(&mut h, None, |h| {
        let (a, b) = (h.i64(1), h.i64(2));
        let t = h.tuple(&[a, b]);
        let th = h.throw(t);
        h.block(&[], Some(th))
    });
    assert_eq!(construct(&h.finish()), Construct::ThrowValue);
}

#[test]
fn test_unknown_abi_union_records_and_non_constant_globals_are_refused() {
    let mut h = Hb::new();
    let fc = h.names.intern("fastcall");
    let n = h.name("ext");
    let _ = h.item(Item::new(
        Some(n),
        ItemKind::Fn(FnDef {
            abi: Some(fc),
            ..FnDef::default()
        }),
    ));
    assert_eq!(construct(&h.finish()), Construct::Abi);

    let mut h = Hb::new();
    let t = h.prim(Prim::I64);
    let id = h.ident("a");
    let f = h.b.field(hir_lang::FieldDef {
        ty: Some(t),
        ..hir_lang::FieldDef::named(id)
    });
    let fields = h.b.list(&[f]);
    let n = h.name("U");
    let r = h.item(Item::new(
        Some(n),
        ItemKind::Record(RecordDef {
            shape: Shape::Named,
            fields,
            is_union: true,
            ..RecordDef::default()
        }),
    ));
    h.records.push(("U".to_string(), r));
    let ut = h.record_ty("U");
    let (p, _) = h.param("u", ut);
    let body = h.block(&[], None);
    let _ = h.func("g", &[p], None, body);
    assert_eq!(construct(&h.finish()), Construct::UnionRecord);

    let mut h = Hb::new();
    let body = h.i64(3);
    let rt = h.prim(Prim::I64);
    let bb = h.block(&[], Some(body));
    let _ = h.func("three", &[], Some(rt), bb);
    let c = h.call("three", &[]);
    let t = h.prim(Prim::I64);
    let n = h.name("G");
    let _ = h.item(Item::new(
        Some(n),
        ItemKind::Global {
            ty: Some(t),
            mutable: false,
            init: Some(c),
        },
    ));
    assert_eq!(construct(&h.finish()), Construct::NonConstant);
}

#[test]
fn test_static_statements_and_dynamic_types_are_refused() {
    let mut h = Hb::new();
    main_fn(&mut h, None, |h| {
        let n = h.name("s");
        let s = h.b.binder(Binder::new(n, BinderKind::Local));
        let t = h.prim(Prim::I64);
        let one = h.i64(1);
        let st = h.b.stmt(Stmt::Static {
            binder: s,
            ty: Some(t),
            init: Some(one),
        });
        h.block(&[st], None)
    });
    assert_eq!(construct(&h.finish()), Construct::Static);

    let mut h = Hb::new();
    let t = h.b.ty(Ty::Any);
    let (p, _) = h.param("x", t);
    let body = h.block(&[], None);
    let _ = h.func("g", &[p], None, body);
    assert_eq!(construct(&h.finish()), Construct::DynamicValue);

    let mut h = Hb::new();
    let e = h.prim(Prim::I64);
    let t = h.b.ty(Ty::Slice(e));
    let (p, _) = h.param("x", t);
    let body = h.block(&[], None);
    let _ = h.func("g", &[p], None, body);
    assert_eq!(construct(&h.finish()), Construct::Collection);
}

#[test]
fn test_foreign_definitions_are_refused() {
    let mut names = Interner::new();
    let mut b = Builder::new();
    let other = DefId::foreign(
        UnitId::new(9),
        Def::Item(hir_lang::ItemId::from_index(0).unwrap()),
    );
    let n = Name::new(names.intern("ext"));
    let path = b.resolved_path(n, Ns::Value, Res::Def(other));
    let callee = b.expr(Expr::Path(path));
    let call = b.call(callee, &[]);
    let body = b.block(&[], Some(call));
    let main = b.func(Name::new(names.intern("main")), &[], body);
    let root = b.module(None, &[main]);
    let hir = b.finish(root).unwrap();
    let mut checker = Checker::new(&hir, Profile::static_explicit());
    let f = checker.types_mut().func(&[], TypeId::UNIT, Effects::NONE);
    let _ = checker.declare_foreign(other, f);
    let typed = checker.check().unwrap();
    assert!(typed.is_ok());
    assert!(matches!(
        irgen_lang::lower(&hir, &typed, &names),
        Err(LowerError::Unsupported {
            construct: Construct::ForeignUnit,
            ..
        })
    ));
}

#[test]
fn test_type_errors_are_reported_before_lowering() {
    let mut h = Hb::new();
    main_fn(&mut h, Some(Prim::I64), |h| {
        let b = h.boolean(true);
        h.block(&[], Some(b))
    });
    let u = h.finish();
    let typed = typeck_lang::check(&u.hir, &Profile::static_explicit()).unwrap();
    assert!(!typed.is_ok());
    assert!(matches!(
        lower_err(&u, &typed),
        LowerError::TypeErrors { count: 1 }
    ));
}

#[test]
fn test_a_type_check_of_another_unit_is_an_error_not_a_panic() {
    let mut h = Hb::new();
    main_fn(&mut h, Some(Prim::I64), |h| {
        let one = h.i64(1);
        h.block(&[], Some(one))
    });
    let small = h.finish();
    let p = common::progs::program(3, 120);
    let typed_small = small.check();
    // The generated program's HIR checked against the small unit's types.
    let r = irgen_lang::lower(&p.unit.hir, &typed_small, &p.unit.names);
    assert!(r.is_err());
}

#[test]
fn test_error_nodes_from_lenient_validation_are_refused() {
    let mut names = Interner::new();
    let mut b = Builder::new();
    let stray = b.expr(Expr::Break {
        label: None,
        value: None,
    });
    let s = b.expr_stmt(stray);
    let body = b.block(&[s], None);
    let main = b.func(Name::new(names.intern("main")), &[], body);
    let root = b.module(None, &[main]);
    let (hir, problems) = b.finish_lenient(root).unwrap();
    assert_eq!(problems.len(), 1);
    let typed = typeck_lang::check(&hir, &Profile::static_explicit()).unwrap();
    let r = irgen_lang::lower(&hir, &typed, &names);
    assert!(
        matches!(
            r,
            Err(LowerError::Unsupported {
                construct: Construct::ErrorNode,
                ..
            }) | Err(LowerError::TypeErrors { .. })
        ),
        "{r:?}"
    );
}

#[test]
fn test_construct_planning_matches_the_roadmap() {
    for c in [
        Construct::Closure,
        Construct::Generic,
        Construct::Coroutine,
        Construct::Sum,
        Construct::DynamicProfile,
        Construct::DynamicValue,
    ] {
        assert!(c.planned_for_0_5(), "{c}");
    }
    let _ = (Lit::Bool(true), Shape::Unit);
}

#[test]
fn test_sum_and_class_types_are_refused() {
    let mut h = Hb::new();
    // sum S { A, B(i64) }; fn g(s: S) {}
    let id_a = h.ident("A");
    let va = h.b.variant(hir_lang::Variant {
        name: id_a,
        shape: Shape::Unit,
        fields: hir_lang::List::EMPTY,
        discriminant: None,
    });
    let t = h.prim(Prim::I64);
    let fb = h.b.field(hir_lang::FieldDef {
        ty: Some(t),
        ..hir_lang::FieldDef::default()
    });
    let id_b = h.ident("B");
    let fields = h.b.list(&[fb]);
    let vb = h.b.variant(hir_lang::Variant {
        name: id_b,
        shape: Shape::Tuple,
        fields,
        discriminant: None,
    });
    let variants = h.b.list(&[va, vb]);
    let n = h.name("S");
    let s = h.item(Item::new(
        Some(n),
        ItemKind::Sum(hir_lang::SumDef {
            variants,
            ..hir_lang::SumDef::default()
        }),
    ));
    h.records.push(("S".to_string(), s));
    let st = h.record_ty("S");
    let (p, _) = h.param("s", st);
    let body = h.block(&[], None);
    let _ = h.func("g", &[p], None, body);
    assert_eq!(construct(&h.finish()), Construct::Sum);

    let mut h = Hb::new();
    let n = h.name("C");
    let c = h.item(Item::new(
        Some(n),
        ItemKind::Class(hir_lang::ClassDef::default()),
    ));
    h.records.push(("C".to_string(), c));
    let ct = h.record_ty("C");
    let (p, _) = h.param("c", ct);
    let body = h.block(&[], None);
    let _ = h.func("g", &[p], None, body);
    assert_eq!(construct(&h.finish()), Construct::Class);
}

#[test]
fn test_recursive_records_and_constant_cycles_are_refused_without_hanging() {
    let mut h = Hb::new();
    // record R { a: i64, next: (R, i64) }
    let t = h.prim(Prim::I64);
    let rt = h.record_ty("R");
    let t2 = h.prim(Prim::I64);
    let tup = h.tuple_ty(&[rt, t2]);
    let _ = h.record("R", &[("a", t), ("next", tup)]);
    let rt = h.record_ty("R");
    let (p, _) = h.param("r", rt);
    let body = h.block(&[], None);
    let _ = h.func("g", &[p], None, body);
    let u = h.finish();
    match typeck_lang::check(&u.hir, &Profile::static_explicit()) {
        Ok(typed) if typed.is_ok() => {
            assert!(matches!(
                lower_err(&u, &typed),
                LowerError::Unsupported {
                    construct: Construct::RecursiveType,
                    ..
                }
            ));
        }
        // typeck may refuse the type first; either way nothing hangs.
        other => println!(
            "recursive record: typeck said {:?}",
            other.map(|t| t.diagnostics().len())
        ),
    }

    // const A: i64 = B; const B: i64 = A; fn main() -> i64 { A }
    let mut h = Hb::new();
    for (name, other) in [("A", "B"), ("B", "A")] {
        let t = h.prim(Prim::I64);
        let v = h.item_path(other);
        let n = h.name(name);
        let it = h.item(Item::new(
            Some(n),
            ItemKind::Const {
                ty: Some(t),
                value: Some(v),
            },
        ));
        h.named(name, it);
    }
    main_fn(&mut h, Some(Prim::I64), |h| {
        let a = h.item_path("A");
        h.block(&[], Some(a))
    });
    let u = h.finish();
    match typeck_lang::check(&u.hir, &Profile::static_explicit()) {
        Ok(typed) if typed.is_ok() => {
            assert!(matches!(
                lower_err(&u, &typed),
                LowerError::ConstCycle { .. }
            ));
        }
        other => println!(
            "const cycle: typeck said {:?}",
            other.map(|t| t.diagnostics().len())
        ),
    }
}

#[test]
fn test_host_values_methods_and_intrinsics_are_refused() {
    // A host symbol used as a value.
    let mut names = Interner::new();
    let mut b = Builder::new();
    let sym = names.intern("host_fn");
    let path = b.resolved_path(Name::new(sym), Ns::Value, Res::Extern(sym));
    let e = b.expr(Expr::Path(path));
    let x = b.new_binder(Name::new(names.intern("x")), BinderKind::Local);
    let pat = b.bind(x);
    let l = b.let_stmt(pat, Some(e));
    let body = b.block(&[l], None);
    let main = b.func(Name::new(names.intern("main")), &[], body);
    let root = b.module(None, &[main]);
    let hir = b.finish(root).unwrap();
    let mut checker = Checker::new(&hir, Profile::static_explicit());
    let _ = checker.declare_extern(sym, TypeId::prim(Prim::I64));
    let typed = checker.check().unwrap();
    assert!(typed.is_ok());
    assert!(matches!(
        irgen_lang::lower(&hir, &typed, &names),
        Err(LowerError::Unsupported {
            construct: Construct::HostBoundary,
            ..
        })
    ));

    // A named intrinsic.
    let mut names = Interner::new();
    let mut b = Builder::new();
    let isym = names.intern("cpu_pause");
    let e = b.expr(Expr::Intrinsic {
        kind: hir_lang::Intrinsic::Named(isym),
        generic_args: hir_lang::List::EMPTY,
        args: hir_lang::List::EMPTY,
    });
    let s = b.expr_stmt(e);
    let body = b.block(&[s], None);
    let main = b.func(Name::new(names.intern("main")), &[], body);
    let root = b.module(None, &[main]);
    let hir = b.finish(root).unwrap();
    let mut checker = Checker::new(&hir, Profile::static_explicit());
    let f = checker.types_mut().func(&[], TypeId::UNIT, Effects::NONE);
    let _ = checker.declare_intrinsic(isym, f);
    let typed = checker.check().unwrap();
    assert!(typed.is_ok(), "{:?}", typed.diagnostics());
    assert!(matches!(
        irgen_lang::lower(&hir, &typed, &names),
        Err(LowerError::Unsupported {
            construct: Construct::Intrinsic,
            ..
        })
    ));

    // A method call on a host type.
    let mut names = Interner::new();
    let mut b = Builder::new();
    let obj = names.intern(common::OBJ);
    let len = names.intern("len");
    let mk = names.intern("mk");
    let path = b.resolved_path(Name::new(mk), Ns::Value, Res::Extern(mk));
    let callee = b.expr(Expr::Path(path));
    let made = b.call(callee, &[]);
    let args = b.list::<Arg>(&[]);
    let call = b.expr(Expr::MethodCall {
        receiver: made,
        method: hir_lang::Ident::new(len, hir_lang::Span::empty(0)),
        generic_args: hir_lang::List::EMPTY,
        args,
    });
    let body = b.block(&[], Some(call));
    let ret = b.ty(Ty::Prim(Prim::I64));
    let main = b.item(Item::new(
        Some(Name::new(names.intern("main"))),
        ItemKind::Fn(FnDef {
            ret: Some(ret),
            body: Some(body),
            ..FnDef::default()
        }),
    ));
    let root = b.module(None, &[main]);
    let hir = b.finish(root).unwrap();
    let mut checker = Checker::new(&hir, Profile::static_explicit());
    let ot = checker.types_mut().external(obj);
    let mk_t = checker.types_mut().func(&[], ot, Effects::NONE);
    let len_t = checker
        .types_mut()
        .func(&[ot], TypeId::prim(Prim::I64), Effects::NONE);
    let _ = checker
        .declare_extern(mk, mk_t)
        .declare_member(ot, len, len_t);
    let typed = checker.check().unwrap();
    assert!(typed.is_ok());
    assert!(matches!(
        irgen_lang::lower(&hir, &typed, &names),
        Err(LowerError::Unsupported {
            construct: Construct::MethodCall,
            ..
        })
    ));
}
