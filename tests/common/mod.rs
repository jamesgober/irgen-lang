//! Shared test support: a small HIR building DSL, the type check, a host for
//! ir-lang's interpreter, and running a lowered function.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

#[cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
pub mod native;
pub mod progs;
pub mod reference;

use hir_lang::{
    Arg, Binder, BinderKind, Block, Builder, Def, Effects, Expr, ExprId, FieldDef, FieldInit,
    FnDef, Hir, Ident, IntLit, Item, ItemId, ItemKind, List, Lit, Name, Ns, Op, OpKind, Param,
    ParamId, Pat, PatId, PathId, Prim, RecordDef, Res, Shape, Span, Stmt, StmtId, Ty, TyId,
};
use intern_lang::Interner;
use ir_lang::interp::{Host, HostReturn, InterpError, Interpreter, Memory, Outcome, Val};
use irgen_lang::{ErrorMode, Lowered, Lowerer, Options, rt};
use typeck_lang::{Checker, Profile, TypeId, Typed};

/// The host type tests use for GC references.
pub const OBJ: &str = "Obj";

/// Builds HIR for tests. Function calls are by name and resolved after
/// `finish`, so functions can call later ones and themselves.
pub struct Hb {
    pub names: Interner,
    pub b: Builder,
    fixups: Vec<(PathId, String)>,
    type_fixups: Vec<(PathId, String)>,
    pub fns: Vec<(String, ItemId)>,
    pub records: Vec<(String, ItemId)>,
    pub items: Vec<ItemId>,
    pub externs: Vec<(String, Vec<HostTy>, HostTy)>,
}

/// A type in a host function's signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostTy {
    Prim(Prim),
    Unit,
    Obj,
    Never,
}

impl Default for Hb {
    fn default() -> Self {
        Self::new()
    }
}

impl Hb {
    pub fn new() -> Hb {
        let mut names = Interner::new();
        let _ = names.intern(OBJ);
        Hb {
            names,
            b: Builder::new(),
            fixups: Vec::new(),
            type_fixups: Vec::new(),
            fns: Vec::new(),
            records: Vec::new(),
            items: Vec::new(),
            externs: Vec::new(),
        }
    }

    pub fn name(&mut self, s: &str) -> Name {
        Name::new(self.names.intern(s))
    }

    pub fn ident(&mut self, s: &str) -> Ident {
        Ident::new(self.names.intern(s), Span::empty(0))
    }

    pub fn prim(&mut self, p: Prim) -> TyId {
        self.b.ty(Ty::Prim(p))
    }

    pub fn tuple_ty(&mut self, elems: &[TyId]) -> TyId {
        let l = self.b.list(elems);
        self.b.ty(Ty::Tuple(l))
    }

    pub fn array_ty(&mut self, elem: TyId, len: u64) -> TyId {
        let n = self
            .b
            .lit(Lit::Int(IntLit::new(len).with_suffix(Prim::Usize)));
        self.b.ty(Ty::Array { elem, len: n })
    }

    /// A type naming a record declared with [`Hb::record`].
    pub fn record_ty(&mut self, name: &str) -> TyId {
        let n = self.name(name);
        let p = self.b.name_path(n, Ns::Type);
        self.type_fixups.push((p, name.to_string()));
        self.b.ty(Ty::Path(p))
    }

    pub fn obj_ty(&mut self) -> TyId {
        let sym = self.names.intern(OBJ);
        let p = self
            .b
            .resolved_path(Name::new(sym), Ns::Type, Res::Extern(sym));
        self.b.ty(Ty::Path(p))
    }

    /// Declares a record with named fields.
    pub fn record(&mut self, name: &str, fields: &[(&str, TyId)]) -> ItemId {
        let mut ids = Vec::new();
        for &(f, ty) in fields {
            let id = self.ident(f);
            ids.push(self.b.field(FieldDef {
                ty: Some(ty),
                ..FieldDef::named(id)
            }));
        }
        let fields = self.b.list(&ids);
        let n = self.name(name);
        let item = self.b.item(Item::new(
            Some(n),
            ItemKind::Record(RecordDef {
                shape: Shape::Named,
                fields,
                ..RecordDef::default()
            }),
        ));
        self.records.push((name.to_string(), item));
        self.items.push(item);
        item
    }

    /// A record literal.
    pub fn record_lit(&mut self, name: &str, fields: &[(&str, ExprId)]) -> ExprId {
        let n = self.name(name);
        let p = self.b.name_path(n, Ns::Type);
        self.type_fixups.push((p, name.to_string()));
        let mut inits = Vec::new();
        for &(f, v) in fields {
            let id = self.ident(f);
            inits.push(FieldInit { name: id, value: v });
        }
        let fields = self.b.list(&inits);
        self.b.expr(Expr::Record {
            path: Some(p),
            fields,
            base: None,
        })
    }

    pub fn field(&mut self, base: ExprId, name: &str) -> ExprId {
        let sym = self.names.intern(name);
        self.b.expr(Expr::Field {
            base,
            member: hir_lang::Member::Named(sym),
            span: Span::empty(0),
        })
    }

    pub fn tfield(&mut self, base: ExprId, i: u32) -> ExprId {
        self.b.expr(Expr::Field {
            base,
            member: hir_lang::Member::Index(i),
            span: Span::empty(0),
        })
    }

    pub fn index(&mut self, base: ExprId, index: ExprId) -> ExprId {
        self.b.expr(Expr::Index { base, index })
    }

    pub fn tuple(&mut self, elems: &[ExprId]) -> ExprId {
        let l = self.b.list(elems);
        self.b.expr(Expr::Tuple(l))
    }

    pub fn array(&mut self, elems: &[ExprId]) -> ExprId {
        let l = self.b.list(elems);
        self.b.expr(Expr::Array(l))
    }

    pub fn repeat(&mut self, elem: ExprId, n: u64) -> ExprId {
        let count = self
            .b
            .lit(Lit::Int(IntLit::new(n).with_suffix(Prim::Usize)));
        self.b.expr(Expr::Repeat { elem, count })
    }

    pub fn unit(&mut self) -> ExprId {
        self.b.expr(Expr::Tuple(List::EMPTY))
    }

    pub fn int(&mut self, v: i64, p: Prim) -> ExprId {
        self.b.lit(Lit::Int(IntLit::signed(v).with_suffix(p)))
    }

    pub fn uint(&mut self, v: u64, p: Prim) -> ExprId {
        self.b.lit(Lit::Int(IntLit::new(v).with_suffix(p)))
    }

    pub fn i64(&mut self, v: i64) -> ExprId {
        self.int(v, Prim::I64)
    }

    pub fn float(&mut self, v: f64, p: Prim) -> ExprId {
        self.b
            .lit(Lit::Float(hir_lang::FloatLit::new(v).with_suffix(p)))
    }

    pub fn boolean(&mut self, v: bool) -> ExprId {
        self.b.lit(Lit::Bool(v))
    }

    pub fn chr(&mut self, c: char) -> ExprId {
        self.b.lit(Lit::Char(c))
    }

    pub fn param(&mut self, name: &str, ty: TyId) -> (ParamId, hir_lang::BinderId) {
        let n = self.name(name);
        let x = self
            .b
            .binder(Binder::new(n, BinderKind::Param).with_mutable(true));
        let pat = self.b.bind(x);
        let p = self.b.param(Param {
            ty: Some(ty),
            ..Param::new(pat)
        });
        (p, x)
    }

    pub fn local(&mut self, name: &str) -> hir_lang::BinderId {
        let n = self.name(name);
        self.b
            .binder(Binder::new(n, BinderKind::Local).with_mutable(true))
    }

    pub fn label(&mut self, name: &str) -> hir_lang::BinderId {
        let n = self.name(name);
        self.b.binder(Binder::new(n, BinderKind::Label))
    }

    pub fn let_(
        &mut self,
        x: hir_lang::BinderId,
        ty: Option<TyId>,
        init: Option<ExprId>,
    ) -> StmtId {
        let pat = self.b.bind(x);
        self.b.stmt(Stmt::Let {
            pat,
            ty,
            init,
            else_: None,
        })
    }

    pub fn let_pat(&mut self, pat: PatId, init: ExprId, else_: Option<ExprId>) -> StmtId {
        self.b.stmt(Stmt::Let {
            pat,
            ty: None,
            init: Some(init),
            else_,
        })
    }

    pub fn get(&mut self, x: hir_lang::BinderId) -> ExprId {
        self.b.use_binder(x)
    }

    pub fn assign(&mut self, target: ExprId, value: ExprId) -> ExprId {
        self.b.expr(Expr::Assign {
            target,
            op: None,
            value,
        })
    }

    pub fn assign_op(&mut self, target: ExprId, op: Op, value: ExprId) -> ExprId {
        self.b.expr(Expr::Assign {
            target,
            op: Some(op),
            value,
        })
    }

    pub fn set(&mut self, x: hir_lang::BinderId, v: ExprId) -> ExprId {
        let t = self.get(x);
        self.assign(t, v)
    }

    pub fn op(&mut self, k: OpKind, args: &[ExprId]) -> ExprId {
        self.b.op(k, args)
    }

    pub fn opp(&mut self, op: Op, args: &[ExprId]) -> ExprId {
        self.b.op_with(op, args)
    }

    pub fn wrap(&mut self, k: OpKind, args: &[ExprId]) -> ExprId {
        self.b
            .op_with(Op::new(k).with_overflow(hir_lang::Overflow::Wrap), args)
    }

    pub fn cast(&mut self, expr: ExprId, ty: TyId, policy: hir_lang::Policy) -> ExprId {
        self.b.expr(Expr::Cast { expr, ty, policy })
    }

    pub fn stmt(&mut self, e: ExprId) -> StmtId {
        self.b.expr_stmt(e)
    }

    pub fn block(&mut self, stmts: &[StmtId], tail: Option<ExprId>) -> ExprId {
        self.b.block(stmts, tail)
    }

    pub fn labeled(
        &mut self,
        label: hir_lang::BinderId,
        stmts: &[StmtId],
        tail: Option<ExprId>,
    ) -> ExprId {
        let stmts = self.b.list(stmts);
        self.b.expr(Expr::Block(Block {
            stmts,
            tail,
            label: Some(label),
            is_unsafe: false,
        }))
    }

    pub fn if_(&mut self, cond: ExprId, then: ExprId, other: Option<ExprId>) -> ExprId {
        self.b.expr(Expr::If {
            cond,
            then,
            else_: other,
        })
    }

    pub fn loop_(
        &mut self,
        label: Option<hir_lang::BinderId>,
        body: ExprId,
        step: Option<ExprId>,
    ) -> ExprId {
        self.b.expr(Expr::Loop { label, body, step })
    }

    pub fn brk(&mut self, label: Option<hir_lang::BinderId>, value: Option<ExprId>) -> ExprId {
        self.b.expr(Expr::Break { label, value })
    }

    pub fn cont(&mut self, label: Option<hir_lang::BinderId>) -> ExprId {
        self.b.expr(Expr::Continue { label })
    }

    pub fn ret(&mut self, v: Option<ExprId>) -> ExprId {
        self.b.expr(Expr::Return(v))
    }

    pub fn throw(&mut self, v: ExprId) -> ExprId {
        self.b.expr(Expr::Throw(v))
    }

    /// `try body catch arms finally f`.
    pub fn try_(
        &mut self,
        body: ExprId,
        catches: &[hir_lang::Arm],
        finally: Option<ExprId>,
    ) -> ExprId {
        let catches = self.b.list(catches);
        self.b.expr(Expr::Try {
            body,
            catches,
            finally,
        })
    }

    /// A catch arm testing the thrown value's type, binding it.
    pub fn catch_typed(
        &mut self,
        p: Prim,
        x: Option<hir_lang::BinderId>,
        guard: Option<ExprId>,
        body: ExprId,
    ) -> hir_lang::Arm {
        let ty = self.prim(p);
        let pat = x.map(|x| self.b.bind(x));
        let pat = self.b.pat(Pat::TypeTest { ty, pat });
        hir_lang::Arm { pat, guard, body }
    }

    pub fn catch_all(&mut self, body: ExprId) -> hir_lang::Arm {
        let pat = self.b.pat(Pat::Wild);
        hir_lang::Arm {
            pat,
            guard: None,
            body,
        }
    }

    pub fn arm(&mut self, pat: PatId, guard: Option<ExprId>, body: ExprId) -> hir_lang::Arm {
        hir_lang::Arm { pat, guard, body }
    }

    pub fn match_(&mut self, scrutinee: ExprId, arms: &[hir_lang::Arm]) -> ExprId {
        let arms = self.b.list(arms);
        self.b.expr(Expr::Match { scrutinee, arms })
    }

    pub fn pat_lit(&mut self, lit: Lit) -> PatId {
        self.b.pat(Pat::Lit(lit))
    }

    pub fn pat_int(&mut self, v: i64, p: Prim) -> PatId {
        self.b
            .pat(Pat::Lit(Lit::Int(IntLit::signed(v).with_suffix(p))))
    }

    pub fn pat_wild(&mut self) -> PatId {
        self.b.pat(Pat::Wild)
    }

    pub fn pat_bind(&mut self, x: hir_lang::BinderId) -> PatId {
        self.b.bind(x)
    }

    pub fn pat_or(&mut self, alts: &[PatId]) -> PatId {
        let l = self.b.list(alts);
        self.b.pat(Pat::Or(l))
    }

    pub fn pat_range(&mut self, lo: Option<PatId>, hi: Option<PatId>, inclusive: bool) -> PatId {
        self.b.pat(Pat::Range { lo, hi, inclusive })
    }

    pub fn pat_tuple(&mut self, elems: &[PatId], rest: Option<u32>) -> PatId {
        let l = self.b.list(elems);
        self.b.pat(Pat::Tuple { elems: l, rest })
    }

    pub fn defer(&mut self, e: ExprId) -> StmtId {
        self.b.stmt(Stmt::Defer(e))
    }

    pub fn let_place(
        &mut self,
        name: &str,
        place: ExprId,
        body_of: impl FnOnce(&mut Hb, hir_lang::BinderId) -> ExprId,
    ) -> ExprId {
        let n = self.name(name);
        let p = self
            .b
            .binder(Binder::new(n, BinderKind::Place).with_mutable(true));
        let body = body_of(self, p);
        self.b.expr(Expr::LetPlace {
            binder: p,
            place,
            body,
        })
    }

    /// A call of a function of this unit, by name.
    pub fn call(&mut self, fname: &str, args: &[ExprId]) -> ExprId {
        let n = self.name(fname);
        let p = self.b.name_path(n, Ns::Value);
        self.fixups.push((p, fname.to_string()));
        let callee = self.b.expr(Expr::Path(p));
        self.b.call(callee, args)
    }

    /// A call of a host function, declared with its signature.
    pub fn ext_call(
        &mut self,
        sym: &str,
        params: &[HostTy],
        ret: HostTy,
        args: &[ExprId],
    ) -> ExprId {
        if !self.externs.iter().any(|(s, _, _)| s == sym) {
            self.externs.push((sym.to_string(), params.to_vec(), ret));
        }
        let s = self.names.intern(sym);
        let p = self
            .b
            .resolved_path(Name::new(s), Ns::Value, Res::Extern(s));
        let callee = self.b.expr(Expr::Path(p));
        let args: Vec<Arg> = args.iter().map(|&a| Arg::positional(a)).collect();
        let args = self.b.list(&args);
        self.b.expr(Expr::Call { callee, args })
    }

    /// Declares a function (public, may throw).
    pub fn func(
        &mut self,
        name: &str,
        params: &[ParamId],
        ret: Option<TyId>,
        body: ExprId,
    ) -> ItemId {
        let params = self.b.list(params);
        let n = self.name(name);
        let item = self.b.item(
            Item::new(
                Some(n),
                ItemKind::Fn(FnDef {
                    params,
                    ret,
                    effects: Effects::THROWS,
                    body: Some(body),
                    ..FnDef::default()
                }),
            )
            .with_vis(hir_lang::Vis::Public),
        );
        self.fns.push((name.to_string(), item));
        self.items.push(item);
        item
    }

    pub fn item(&mut self, item: Item) -> ItemId {
        let id = self.b.item(item);
        self.items.push(id);
        id
    }

    /// Resolves `p` at `finish` to the item registered under `name`.
    pub fn fixup_path(&mut self, p: PathId, name: &str) {
        self.fixups.push((p, name.to_string()));
    }

    /// A type-namespace path to the record `name`, resolved at `finish`.
    pub fn type_path(&mut self, name: &str) -> PathId {
        let n = self.name(name);
        let p = self.b.name_path(n, Ns::Type);
        self.type_fixups.push((p, name.to_string()));
        p
    }

    /// Registers an item (a constant, a global) for by-name paths.
    pub fn named(&mut self, name: &str, item: ItemId) {
        self.fns.push((name.to_string(), item));
    }

    /// A value path to the item registered under `name`, resolved at
    /// `finish`.
    pub fn item_path(&mut self, name: &str) -> ExprId {
        let n = self.name(name);
        let p = self.b.name_path(n, Ns::Value);
        self.fixups.push((p, name.to_string()));
        self.b.expr(Expr::Path(p))
    }

    /// Finishes (strict) and resolves the by-name paths.
    pub fn finish(self) -> Unit {
        let Hb {
            names,
            mut b,
            fixups,
            type_fixups,
            fns,
            records,
            items,
            externs,
        } = self;
        let root = b.module(None, &items);
        let mut hir = b.finish(root).expect("valid HIR");
        for (p, name) in fixups {
            let item = fns
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, i)| *i)
                .unwrap_or_else(|| panic!("no function {name}"));
            let d = hir.def(Def::Item(item));
            hir.resolve(p, Res::Def(d)).expect("resolves");
        }
        for (p, name) in type_fixups {
            let item = records
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, i)| *i)
                .unwrap_or_else(|| panic!("no record {name}"));
            let d = hir.def(Def::Item(item));
            hir.resolve(p, Res::Def(d)).expect("resolves");
        }
        Unit {
            profile: Profile::static_explicit(),
            hir,
            names,
            fns,
            externs,
        }
    }
}

/// A finished, resolved unit.
pub struct Unit {
    pub profile: Profile,
    pub hir: Hir,
    pub names: Interner,
    pub fns: Vec<(String, ItemId)>,
    pub externs: Vec<(String, Vec<HostTy>, HostTy)>,
}

impl Unit {
    pub fn item(&self, name: &str) -> ItemId {
        self.fns
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, i)| *i)
            .unwrap_or_else(|| panic!("no function {name}"))
    }

    /// Type-checks under the static-explicit profile, with the host
    /// functions declared.
    pub fn check(&self) -> Typed {
        self.check_with(self.profile)
    }

    pub fn check_with(&self, profile: Profile) -> Typed {
        let mut checker = Checker::new(&self.hir, profile);
        let obj_sym = self.names.get(OBJ);
        let obj = obj_sym.map(|s| checker.types_mut().external(s));
        for (sym, params, ret) in &self.externs {
            let conv = |t: &HostTy| match t {
                HostTy::Prim(p) => TypeId::prim(*p),
                HostTy::Unit => TypeId::UNIT,
                HostTy::Never => TypeId::NEVER,
                HostTy::Obj => obj.unwrap_or(TypeId::ERROR),
            };
            let ps: Vec<TypeId> = params.iter().map(conv).collect();
            let r = conv(ret);
            let f = checker.types_mut().func(&ps, r, Effects::NONE);
            let s = self.names.get(sym).expect("interned");
            let _ = checker.declare_extern(s, f);
        }
        let typed = checker.check().expect("checks");
        if !typed.is_ok() {
            let msgs: Vec<String> = typed
                .diagnostics()
                .iter()
                .map(|d| typed.message(d, &self.names))
                .collect();
            panic!("type errors: {msgs:?}");
        }
        typed
    }

    pub fn lower(&self, typed: &Typed) -> Lowered {
        self.lower_with(typed, Options::new())
    }

    pub fn lower_with(&self, typed: &Typed, options: Options) -> Lowered {
        match Lowerer::new(options).lower(&self.hir, typed, &self.names) {
            Ok(l) => l,
            Err(e) => panic!("lowering failed: {e}"),
        }
    }

    /// Checks, lowers, and runs `name` with `args`.
    pub fn run(&self, name: &str, args: &[Val]) -> Out {
        let typed = self.check();
        let out = self.lower(&typed);
        run_lowered(&out, self.item(name), args)
    }

    pub fn run_mode(&self, name: &str, args: &[Val], mode: ErrorMode) -> Out {
        let typed = self.check();
        let out = self.lower_with(&typed, Options::new().with_errors(mode));
        run_lowered(&out, self.item(name), args)
    }
}

/// The outcome of a run, comparable across tiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    /// Returned: the value's bits (masked to its width), or none.
    Ret(Option<u64>),
    /// Unwound with an exception record.
    Thrown { tag: u32, bits: u64 },
    /// Trapped with a code.
    Trap(u32),
}

impl Out {
    /// Equal, treating any two NaN floats of a type as the same.
    pub fn same(&self, other: &Out, float: Option<Prim>) -> bool {
        match (self, other, float) {
            (Out::Ret(Some(a)), Out::Ret(Some(b)), Some(Prim::F64)) => {
                let (x, y) = (f64::from_bits(*a), f64::from_bits(*b));
                a == b || (x.is_nan() && y.is_nan())
            }
            (Out::Ret(Some(a)), Out::Ret(Some(b)), Some(Prim::F32)) => {
                let (x, y) = (f32::from_bits(*a as u32), f32::from_bits(*b as u32));
                a == b || (x.is_nan() && y.is_nan())
            }
            _ => self == other,
        }
    }
}

/// The interpreter's host: the exception allocator, and host functions
/// over GC objects (`obj_new`, `obj_get`, `obj_set`), a GC poll
/// (`gc_poll`), a trace (`trace`), and host functions that throw
/// (`host_throw` always, `host_check` on multiples of 3).
#[derive(Default)]
pub struct TestHost {
    pub trace: Vec<i64>,
    pub allocated: usize,
}

fn exception_record(mem: &mut Memory, tag: u32, bits: u64) -> Result<u64, InterpError> {
    let p = mem.alloc(rt::RECORD_SIZE, rt::RECORD_ALIGN)?;
    mem.write(p + rt::TAG_OFFSET, &tag.to_le_bytes())?;
    mem.write(p + rt::BITS_OFFSET, &bits.to_le_bytes())?;
    Ok(p)
}

impl Host for TestHost {
    fn call(
        &mut self,
        name: &str,
        args: &[Val],
        mem: &mut Memory,
    ) -> Result<HostReturn, InterpError> {
        let arg = |i: usize| args.get(i).copied().ok_or(InterpError::BadArguments);
        match name {
            rt::EXCEPTION => {
                let tag = arg(0)?.bits() as u32;
                let bits = arg(1)?.bits();
                let p = exception_record(mem, tag, bits)?;
                Ok(HostReturn::Values(vec![Val::ptr(p)]))
            }
            "obj_new" => {
                let v = arg(0)?.bits();
                let r = mem.alloc_object(8)?;
                let a = mem.ref_address(r)?;
                mem.write(a, &v.to_le_bytes())?;
                self.allocated += 1;
                Ok(HostReturn::Values(vec![r]))
            }
            "obj_get" => {
                let a = mem.ref_address(arg(0)?)?;
                let b = mem.read(a, 8)?;
                let mut x = [0u8; 8];
                x.copy_from_slice(b);
                Ok(HostReturn::Values(vec![Val::i64(i64::from_le_bytes(x))]))
            }
            "obj_set" => {
                let a = mem.ref_address(arg(0)?)?;
                mem.write(a, &arg(1)?.bits().to_le_bytes())?;
                Ok(HostReturn::Values(vec![]))
            }
            "gc_poll" => Ok(HostReturn::Values(vec![])),
            "trace" => {
                self.trace.push(arg(0)?.bits() as i64);
                Ok(HostReturn::Values(vec![]))
            }
            "host_throw" => {
                let bits = arg(0)?.bits();
                let tag = rt::tag(Prim::I64).unwrap_or(0);
                let p = exception_record(mem, tag, bits)?;
                Ok(HostReturn::Unwind(Val::ptr(p)))
            }
            "host_check" => {
                // Unwinds with the value when it is a multiple of 3.
                let bits = arg(0)?.bits();
                if (bits as i64) % 3 == 0 {
                    let tag = rt::tag(Prim::I64).unwrap_or(0);
                    let p = exception_record(mem, tag, bits)?;
                    return Ok(HostReturn::Unwind(Val::ptr(p)));
                }
                Ok(HostReturn::Values(vec![]))
            }
            _ => Err(InterpError::UnresolvedImport { name: name.into() }),
        }
    }
}

/// Runs a lowered function in ir-lang's reference interpreter.
pub fn run_lowered(out: &Lowered, item: ItemId, args: &[Val]) -> Out {
    run_lowered_traced(out, item, args).0
}

pub fn run_lowered_traced(out: &Lowered, item: ItemId, args: &[Val]) -> (Out, Vec<i64>) {
    let f = out.function(item).expect("lowered");
    let mut it = Interpreter::new(out.module(), TestHost::default()).expect("valid module");
    let r = it.call(f, args);
    let o = decode(&it, r);
    let trace = core::mem::take(&mut it.host_mut().trace);
    (o, trace)
}

pub fn decode<H: Host>(it: &Interpreter<'_, H>, r: Result<Outcome, InterpError>) -> Out {
    match r {
        Ok(Outcome::Return(vs)) => Out::Ret(vs.first().map(|v| v.bits())),
        Ok(Outcome::Unwind(p)) => {
            let m = it.memory();
            let tag = m.read(p.bits() + rt::TAG_OFFSET, 4).expect("record");
            let bits = m.read(p.bits() + rt::BITS_OFFSET, 8).expect("record");
            Out::Thrown {
                tag: u32::from_le_bytes(tag.try_into().expect("4")),
                bits: u64::from_le_bytes(bits.try_into().expect("8")),
            }
        }
        Ok(Outcome::Trap(c)) => Out::Trap(c),
        Ok(other) => panic!("unexpected outcome {other:?}"),
        Err(e) => panic!("interpreter error: {e}"),
    }
}

/// A thrown run-time error with this code.
pub fn error(code: u32) -> Out {
    Out::Thrown {
        tag: rt::TAG_ERROR,
        bits: u64::from(code),
    }
}

/// A thrown `i64`.
pub fn thrown_i64(v: i64) -> Out {
    Out::Thrown {
        tag: rt::tag(Prim::I64).unwrap_or(0),
        bits: v as u64,
    }
}

/// A returned `i64`.
pub fn ret_i64(v: i64) -> Out {
    Out::Ret(Some(v as u64))
}

/// Every lowered function is valid (the lowering validates, and this checks
/// the module once more as a whole).
pub fn assert_valid(out: &Lowered) {
    out.module().validate().expect("valid module");
}
