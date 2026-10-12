//! How HIR types are represented in ir-lang.
//!
//! Scalars (`bool`, integers, floats, `char`, host references) are SSA values.
//! Tuples, records, and fixed-length arrays are ir-lang aggregates: they live
//! in stack slots and globals and are reached through addresses. Unit and
//! never have no value at all.
//!
//! Types arrive two ways: as typeck-lang [`TypeId`]s (the type of an
//! expression or binder) and as HIR [`TyId`] annotations (record fields and
//! return types, which typeck-lang's result does not expose). Both map to one
//! [`Repr`], keyed structurally, so a field annotated `(f64, f64)` and an
//! expression typed `(f64, f64)` share one aggregate.

use alloc::{collections::BTreeMap, vec, vec::Vec};

use hir_lang::{Def, Expr, Hir, ItemId, ItemKind, Lit, Member, NodeRef, Prim, Res, Ty, TyId};
use intern_lang::Symbol;
use ir_lang::{AggId, Aggregate, MemType, Module, Type};
use typeck_lang::{Type as TType, TypeId, Typed};

use crate::{
    error::{Construct, LowerError},
    unit::unsupported,
};

/// A scalar: one SSA value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Scalar {
    Bool,
    /// An integer primitive (`isize`/`usize` included).
    Int(Prim),
    /// `f32` or `f64`.
    Float(Prim),
    /// A Unicode scalar value, a `u32` in ir-lang.
    Char,
    /// A value of a host (`extern`) type: an opaque GC reference.
    Host,
}

impl Scalar {
    fn of_prim(p: Prim) -> Option<Scalar> {
        Some(match p {
            Prim::Bool => Scalar::Bool,
            Prim::Char => Scalar::Char,
            Prim::F32 | Prim::F64 => Scalar::Float(p),
            Prim::Str => return None,
            _ => Scalar::Int(p),
        })
    }

    /// The ir-lang type (pointer width 64: `isize` is `i64`).
    pub(crate) const fn ir(self) -> Type {
        match self {
            Scalar::Bool => Type::Bool,
            Scalar::Char => Type::U32,
            Scalar::Host => Type::Ref,
            Scalar::Float(Prim::F32) => Type::F32,
            Scalar::Float(_) => Type::F64,
            Scalar::Int(p) => match p {
                Prim::I8 => Type::I8,
                Prim::I16 => Type::I16,
                Prim::I32 => Type::I32,
                Prim::U8 => Type::U8,
                Prim::U16 => Type::U16,
                Prim::U32 => Type::U32,
                Prim::U64 | Prim::Usize => Type::U64,
                _ => Type::I64,
            },
        }
    }

    /// The HIR primitive, for everything but host references.
    pub(crate) const fn prim(self) -> Option<Prim> {
        match self {
            Scalar::Bool => Some(Prim::Bool),
            Scalar::Char => Some(Prim::Char),
            Scalar::Int(p) | Scalar::Float(p) => Some(p),
            Scalar::Host => None,
        }
    }

    /// Size in bytes (equal to the alignment).
    pub(crate) const fn size(self) -> u32 {
        self.ir().size(ir_lang::PointerWidth::P64)
    }

    pub(crate) const fn is_int_like(self) -> bool {
        matches!(self, Scalar::Int(_) | Scalar::Char)
    }
}

/// How a HIR type is represented.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Repr {
    /// The unit tuple: no value.
    Unit,
    /// The never type: no value, and evaluation does not get here.
    Never,
    Scalar(Scalar),
    /// An aggregate, by index into [`TypeMap::aggs`].
    Agg(u32),
}

impl Repr {
    pub(crate) const fn has_value(self) -> bool {
        matches!(self, Repr::Scalar(_) | Repr::Agg(_))
    }
}

/// An aggregate's shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// A tuple or record: fields in order, with record field names.
    Struct {
        fields: Vec<Repr>,
        names: Vec<Option<Symbol>>,
    },
    /// A fixed-length array.
    Array { elem: Repr, len: u64 },
}

/// A declared aggregate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AggInfo {
    pub(crate) id: AggId,
    pub(crate) size: u64,
    pub(crate) align: u32,
    pub(crate) shape: Shape,
    /// Byte offset of each struct field (empty for arrays).
    pub(crate) offsets: Vec<u64>,
}

impl AggInfo {
    /// The representation of field `index` (or the element of an array).
    pub(crate) fn field(&self, index: u32) -> Option<Repr> {
        match &self.shape {
            Shape::Struct { fields, .. } => fields.get(index as usize).copied(),
            Shape::Array { elem, len } => (u64::from(index) < *len).then_some(*elem),
        }
    }

    /// The byte offset of field `index` (or element `index`, given the
    /// element size).
    pub(crate) fn offset(&self, index: u32, elem_size: u64) -> Option<u64> {
        match &self.shape {
            Shape::Struct { .. } => self.offsets.get(index as usize).copied(),
            Shape::Array { len, .. } => {
                (u64::from(index) < *len).then(|| elem_size.checked_mul(u64::from(index)))?
            }
        }
    }

    /// The number of fields (or elements).
    pub(crate) fn len(&self) -> u64 {
        match &self.shape {
            Shape::Struct { fields, .. } => fields.len() as u64,
            Shape::Array { len, .. } => *len,
        }
    }

    /// The field a member names.
    pub(crate) fn member(&self, member: Member) -> Option<(u32, Repr)> {
        let index = match (member, &self.shape) {
            (Member::Index(i), Shape::Struct { .. }) => i,
            (Member::Named(sym), Shape::Struct { names, .. }) => {
                let pos = names.iter().position(|n| *n == Some(sym))?;
                u32::try_from(pos).ok()?
            }
            (_, Shape::Array { .. }) => return None,
        };
        Some((index, self.field(index)?))
    }

    pub(crate) fn field_index_by_name(&self, sym: Symbol) -> Option<(u32, Repr)> {
        self.member(Member::Named(sym))
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum AggKey {
    Tuple(Vec<Repr>),
    Array(Repr, u64),
    Record(ItemId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Memo {
    Todo,
    Busy,
    Done(Repr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Node {
    Type(TypeId),
    Ty(TyId),
    Record(ItemId),
}

/// What a node needs before its representation is known.
enum Step {
    Leaf(Repr),
    Children(Vec<Node>),
}

/// The type mapping of one unit: memo tables and the declared aggregates.
pub(crate) struct TypeMap {
    by_type: Vec<Memo>,
    by_ty: Vec<Memo>,
    records: Vec<Memo>,
    keys: BTreeMap<AggKey, u32>,
    pub(crate) aggs: Vec<AggInfo>,
    unit_agg: Option<AggId>,
}

impl TypeMap {
    pub(crate) fn new(hir: &Hir, typed: &Typed) -> TypeMap {
        TypeMap {
            by_type: vec![Memo::Todo; typed.types().len()],
            by_ty: vec![Memo::Todo; hir.count(hir_lang::IdKind::Ty)],
            records: vec![Memo::Todo; hir.count(hir_lang::IdKind::Item)],
            keys: BTreeMap::new(),
            aggs: Vec::new(),
            unit_agg: None,
        }
    }

    pub(crate) fn agg(&self, index: u32) -> Option<&AggInfo> {
        self.aggs.get(index as usize)
    }

    /// The memory type of a representation, for a field or an element (unit
    /// and never become an empty struct).
    fn memtype(&mut self, module: &mut Module, r: Repr) -> Result<MemType, LowerError> {
        Ok(match r {
            Repr::Scalar(s) => MemType::Scalar(s.ir()),
            Repr::Agg(i) => match self.aggs.get(i as usize) {
                Some(a) => MemType::Agg(a.id),
                None => return Err(internal_type()),
            },
            Repr::Unit | Repr::Never => MemType::Agg(self.unit_agg(module)?),
        })
    }

    fn unit_agg(&mut self, module: &mut Module) -> Result<AggId, LowerError> {
        if let Some(id) = self.unit_agg {
            return Ok(id);
        }
        let id = module.declare_aggregate(Aggregate::structure(&[]))?;
        self.unit_agg = Some(id);
        Ok(id)
    }

    /// The memory type of a value of this representation (the element type
    /// of `elem_addr`).
    pub(crate) fn elem_memtype(&self, r: Repr) -> Option<MemType> {
        match r {
            Repr::Scalar(s) => Some(MemType::Scalar(s.ir())),
            Repr::Agg(i) => self.aggs.get(i as usize).map(|a| MemType::Agg(a.id)),
            Repr::Unit | Repr::Never => self.unit_agg.map(MemType::Agg),
        }
    }

    /// The representation of a typeck-lang type, declaring aggregates.
    pub(crate) fn of_type(
        &mut self,
        hir: &Hir,
        typed: &Typed,
        module: &mut Module,
        t: TypeId,
        at: NodeRef,
    ) -> Result<Repr, LowerError> {
        if let Some(Memo::Done(r)) = self.by_type.get(t.index()) {
            return Ok(*r);
        }
        self.resolve(hir, typed, module, Node::Type(t), at)
    }

    /// The representation of a HIR annotation, declaring aggregates.
    pub(crate) fn of_ty(
        &mut self,
        hir: &Hir,
        typed: &Typed,
        module: &mut Module,
        ty: TyId,
        at: NodeRef,
    ) -> Result<Repr, LowerError> {
        if let Some(Memo::Done(r)) = self.by_ty.get(ty.index()) {
            return Ok(*r);
        }
        self.resolve(hir, typed, module, Node::Ty(ty), at)
    }

    /// An already-mapped typeck type (after the pre-scan).
    pub(crate) fn known(&self, t: TypeId) -> Option<Repr> {
        match self.by_type.get(t.index()) {
            Some(Memo::Done(r)) => Some(*r),
            _ => None,
        }
    }

    /// An already-mapped annotation (after the pre-scan).
    pub(crate) fn known_ty(&self, ty: TyId) -> Option<Repr> {
        match self.by_ty.get(ty.index()) {
            Some(Memo::Done(r)) => Some(*r),
            _ => None,
        }
    }

    fn memo(&self, n: Node) -> Memo {
        let m = match n {
            Node::Type(t) => self.by_type.get(t.index()),
            Node::Ty(t) => self.by_ty.get(t.index()),
            Node::Record(i) => self.records.get(i.index()),
        };
        // An id outside the tables is foreign: treat it as a leaf the step
        // function reports.
        m.copied().unwrap_or(Memo::Todo)
    }

    fn set(&mut self, n: Node, m: Memo) {
        let slot = match n {
            Node::Type(t) => self.by_type.get_mut(t.index()),
            Node::Ty(t) => self.by_ty.get_mut(t.index()),
            Node::Record(i) => self.records.get_mut(i.index()),
        };
        if let Some(s) = slot {
            *s = m;
        }
    }

    /// Iterative post-order over the type graph: children first, a node in
    /// progress met again is a type containing itself.
    fn resolve(
        &mut self,
        hir: &Hir,
        typed: &Typed,
        module: &mut Module,
        root: Node,
        at: NodeRef,
    ) -> Result<Repr, LowerError> {
        let mut stack: Vec<(Node, Option<Vec<Node>>)> = vec![(root, None)];
        while let Some((node, children)) = stack.pop() {
            match children {
                None => match self.memo(node) {
                    Memo::Done(_) => {}
                    Memo::Busy => {
                        return Err(unsupported(hir, Construct::RecursiveType, at));
                    }
                    Memo::Todo => match step(hir, typed, node, at)? {
                        Step::Leaf(r) => self.set(node, Memo::Done(r)),
                        Step::Children(kids) => {
                            self.set(node, Memo::Busy);
                            let pending: Vec<Node> = kids
                                .iter()
                                .copied()
                                .filter(|&k| !matches!(self.memo(k), Memo::Done(_)))
                                .collect();
                            for &k in &pending {
                                if self.memo(k) == Memo::Busy {
                                    return Err(unsupported(hir, Construct::RecursiveType, at));
                                }
                            }
                            stack.push((node, Some(kids)));
                            stack.extend(pending.into_iter().rev().map(|k| (k, None)));
                        }
                    },
                },
                Some(kids) => {
                    let mut reprs = Vec::with_capacity(kids.len());
                    for k in &kids {
                        match self.memo(*k) {
                            Memo::Done(r) => reprs.push(r),
                            _ => return Err(unsupported(hir, Construct::RecursiveType, at)),
                        }
                    }
                    let r = self.combine(hir, typed, module, node, &reprs, at)?;
                    self.set(node, Memo::Done(r));
                }
            }
        }
        match self.memo(root) {
            Memo::Done(r) => Ok(r),
            _ => Err(LowerError::MissingType { node: at }),
        }
    }

    /// The representation of a node whose children are known.
    fn combine(
        &mut self,
        hir: &Hir,
        typed: &Typed,
        module: &mut Module,
        node: Node,
        kids: &[Repr],
        at: NodeRef,
    ) -> Result<Repr, LowerError> {
        match node {
            Node::Type(t) => match typed.types().get(t) {
                TType::Tuple(_) => self.declare(module, AggKey::Tuple(kids.to_vec()), at, hir),
                TType::Array { len, .. } => {
                    let elem = kids.first().copied().ok_or_else(internal_type)?;
                    self.declare(module, AggKey::Array(elem, len), at, hir)
                }
                // `Adt` of a record: its record node's representation.
                _ => kids.first().copied().ok_or_else(internal_type),
            },
            Node::Ty(ty) => match *hir.ty(ty) {
                Ty::Tuple(_) => self.declare(module, AggKey::Tuple(kids.to_vec()), at, hir),
                Ty::Array { len, .. } => {
                    let elem = kids.first().copied().ok_or_else(internal_type)?;
                    let n = const_len(hir, len, at)?;
                    self.declare(module, AggKey::Array(elem, n), at, hir)
                }
                _ => kids.first().copied().ok_or_else(internal_type),
            },
            Node::Record(item) => {
                let ItemKind::Record(def) = hir.item(item).kind else {
                    return Err(internal_type());
                };
                let names = hir
                    .list(def.fields)
                    .iter()
                    .map(|&f| hir.field(f).name.map(|i| i.sym))
                    .collect();
                self.declare_record(module, item, kids, names, hir, at)
            }
        }
    }

    fn declare(
        &mut self,
        module: &mut Module,
        key: AggKey,
        at: NodeRef,
        hir: &Hir,
    ) -> Result<Repr, LowerError> {
        if let Some(&i) = self.keys.get(&key) {
            return Ok(Repr::Agg(i));
        }
        let (agg, shape) = match &key {
            AggKey::Tuple(fields) => {
                let mut mts = Vec::with_capacity(fields.len());
                for &f in fields {
                    mts.push(self.field_memtype(module, f, hir, at)?);
                }
                let names = vec![None; fields.len()];
                (
                    Aggregate::structure(&mts),
                    Shape::Struct {
                        fields: fields.clone(),
                        names,
                    },
                )
            }
            AggKey::Array(elem, len) => {
                let mt = self.field_memtype(module, *elem, hir, at)?;
                (
                    Aggregate::array(mt, *len),
                    Shape::Array {
                        elem: *elem,
                        len: *len,
                    },
                )
            }
            AggKey::Record(_) => return Err(internal_type()),
        };
        self.finish_decl(module, key, agg, shape)
    }

    fn field_memtype(
        &mut self,
        module: &mut Module,
        f: Repr,
        hir: &Hir,
        at: NodeRef,
    ) -> Result<MemType, LowerError> {
        if f == Repr::Scalar(Scalar::Host) {
            return Err(unsupported(hir, Construct::GcRefInMemory, at));
        }
        self.memtype(module, f)
    }

    fn finish_decl(
        &mut self,
        module: &mut Module,
        key: AggKey,
        agg: Aggregate,
        shape: Shape,
    ) -> Result<Repr, LowerError> {
        let id = module.declare_aggregate(agg)?;
        let layout = module.layout(MemType::Agg(id)).ok_or_else(internal_type)?;
        let index = u32::try_from(self.aggs.len()).map_err(|_| internal_type())?;
        let mut offsets = Vec::new();
        if let Shape::Struct { fields, .. } = &shape {
            for k in 0..fields.len() {
                let k = u32::try_from(k).map_err(|_| internal_type())?;
                offsets.push(module.field_offset(id, k).ok_or_else(internal_type)?);
            }
        }
        self.aggs.push(AggInfo {
            id,
            size: layout.size(),
            align: layout.align(),
            shape,
            offsets,
        });
        let _ = self.keys.insert(key, index);
        Ok(Repr::Agg(index))
    }
}

impl TypeMap {
    fn declare_record(
        &mut self,
        module: &mut Module,
        item: ItemId,
        fields: &[Repr],
        names: Vec<Option<Symbol>>,
        hir: &Hir,
        at: NodeRef,
    ) -> Result<Repr, LowerError> {
        let key = AggKey::Record(item);
        if let Some(&i) = self.keys.get(&key) {
            return Ok(Repr::Agg(i));
        }
        let mut mts = Vec::with_capacity(fields.len());
        for &f in fields {
            mts.push(self.field_memtype(module, f, hir, at)?);
        }
        let shape = Shape::Struct {
            fields: fields.to_vec(),
            names,
        };
        self.finish_decl(module, key, Aggregate::structure(&mts), shape)
    }
}

fn internal_type() -> LowerError {
    LowerError::Internal { node: None }
}

/// The children of a node, or its representation if it has none.
fn step(hir: &Hir, typed: &Typed, node: Node, at: NodeRef) -> Result<Step, LowerError> {
    let no = |c: Construct| Err(unsupported(hir, c, at));
    match node {
        Node::Type(t) => match typed.types().get(t) {
            TType::Prim(p) => match Scalar::of_prim(p) {
                Some(s) => Ok(Step::Leaf(Repr::Scalar(s))),
                None => no(Construct::String),
            },
            TType::Tuple([]) => Ok(Step::Leaf(Repr::Unit)),
            TType::Tuple(elems) => Ok(Step::Children(
                elems.iter().map(|&e| Node::Type(e)).collect(),
            )),
            TType::Array { elem, .. } => Ok(Step::Children(vec![Node::Type(elem)])),
            TType::Never => Ok(Step::Leaf(Repr::Never)),
            TType::Adt { item, args } => {
                if !args.is_empty() {
                    return no(Construct::Generic);
                }
                record_step(hir, item, at)
            }
            TType::Extern { args, .. } => {
                if args.is_empty() {
                    Ok(Step::Leaf(Repr::Scalar(Scalar::Host)))
                } else {
                    no(Construct::Generic)
                }
            }
            TType::Error => Err(LowerError::MissingType { node: at }),
            TType::Any => no(Construct::DynamicValue),
            TType::Null | TType::Nullable(_) | TType::Union(_) | TType::Intersection(_) => {
                no(Construct::Nullable)
            }
            TType::Slice(_) | TType::Map { .. } => no(Construct::Collection),
            TType::Record(_) => no(Construct::StructuralRecord),
            TType::Ref { .. } | TType::Ptr { .. } => no(Construct::Reference),
            TType::Fn { .. } => no(Construct::FunctionValue),
            TType::Foreign { .. } => no(Construct::ForeignUnit),
            TType::Param(_)
            | TType::ImplParam(_)
            | TType::ParamArray { .. }
            | TType::SelfOf(_)
            | TType::Projection { .. }
            | TType::Opaque { .. }
            | TType::Const(_) => no(Construct::Generic),
            TType::Trait { .. } | TType::Dyn(_) => no(Construct::Class),
            TType::Async(_) | TType::Task(_) | TType::Generator { .. } => no(Construct::Coroutine),
            _ => no(Construct::OtherType),
        },
        Node::Ty(ty) => match *hir.ty(ty) {
            Ty::Prim(p) => match Scalar::of_prim(p) {
                Some(s) => Ok(Step::Leaf(Repr::Scalar(s))),
                None => no(Construct::String),
            },
            Ty::Tuple(list) if list.is_empty() => Ok(Step::Leaf(Repr::Unit)),
            Ty::Tuple(list) => Ok(Step::Children(
                hir.list(list).iter().map(|&e| Node::Ty(e)).collect(),
            )),
            Ty::Array { elem, .. } => Ok(Step::Children(vec![Node::Ty(elem)])),
            Ty::Never => Ok(Step::Leaf(Repr::Never)),
            Ty::Path(path) => {
                let p = hir.path(path);
                if p.unresolved > 0 {
                    return no(Construct::Unresolved);
                }
                if hir.list(p.segments).iter().any(|s| !s.args.is_empty()) {
                    return no(Construct::Generic);
                }
                match p.res {
                    Res::Prim(prim) => match Scalar::of_prim(prim) {
                        Some(s) => Ok(Step::Leaf(Repr::Scalar(s))),
                        None => no(Construct::String),
                    },
                    Res::Extern(_) => Ok(Step::Leaf(Repr::Scalar(Scalar::Host))),
                    Res::Local(_) => no(Construct::Generic),
                    Res::Def(d) => {
                        if d.unit() != hir.unit() {
                            return no(Construct::ForeignUnit);
                        }
                        match d.def() {
                            Def::Variant(_) => no(Construct::Sum),
                            Def::Item(item) => match hir.item(item).kind {
                                ItemKind::Alias { generics, ty } => {
                                    if generics.params.is_empty() {
                                        Ok(Step::Children(vec![Node::Ty(ty)]))
                                    } else {
                                        no(Construct::Generic)
                                    }
                                }
                                _ => record_step(hir, item, at),
                            },
                        }
                    }
                    Res::Unresolved => no(Construct::Unresolved),
                    Res::Err => no(Construct::ErrorNode),
                }
            }
            Ty::Infer => Err(LowerError::MissingType { node: at }),
            Ty::Slice(_) => no(Construct::Collection),
            Ty::Ref { .. } | Ty::Ptr { .. } => no(Construct::Reference),
            Ty::Fn { .. } => no(Construct::FunctionValue),
            Ty::Nullable(_) | Ty::Union(_) | Ty::Intersection(_) => no(Construct::Nullable),
            Ty::Any => no(Construct::DynamicValue),
            Ty::Object(_) | Ty::Impl(_) | Ty::SelfTy => no(Construct::Class),
            Ty::Err => no(Construct::ErrorNode),
        },
        Node::Record(item) => {
            let ItemKind::Record(def) = hir.item(item).kind else {
                return Err(internal_type());
            };
            if !def.generics.params.is_empty() {
                return no(Construct::Generic);
            }
            if def.is_union {
                return no(Construct::UnionRecord);
            }
            let mut kids = Vec::new();
            for &f in hir.list(def.fields) {
                match hir.field(f).ty {
                    Some(ty) => kids.push(Node::Ty(ty)),
                    None => {
                        return Err(LowerError::MissingType {
                            node: NodeRef::Field(f),
                        });
                    }
                }
            }
            Ok(Step::Children(kids))
        }
    }
}

/// A nominal type: a record maps through its record node; anything else is
/// refused by kind.
fn record_step(hir: &Hir, item: ItemId, at: NodeRef) -> Result<Step, LowerError> {
    match hir.item(item).kind {
        ItemKind::Record(_) => Ok(Step::Children(vec![Node::Record(item)])),
        ItemKind::Sum(_) => Err(unsupported(hir, Construct::Sum, at)),
        _ => Err(unsupported(hir, Construct::Class, at)),
    }
}

/// The value of an array-length annotation: an integer literal, or a
/// constant whose value is one (followed through constants, at most as many
/// as the unit has items, so a cycle ends).
pub(crate) fn const_len(
    hir: &Hir,
    mut e: hir_lang::ExprId,
    at: NodeRef,
) -> Result<u64, LowerError> {
    let mut hops = 0usize;
    loop {
        match *hir.expr(e) {
            Expr::Lit(Lit::Int(i)) if !i.negative => return Ok(i.value),
            Expr::Path(p) => {
                let path = hir.path(p);
                match path.res {
                    Res::Def(d) if d.unit() == hir.unit() && path.unresolved == 0 => {
                        let Def::Item(item) = d.def() else {
                            break;
                        };
                        let ItemKind::Const { value: Some(v), .. } = hir.item(item).kind else {
                            break;
                        };
                        hops += 1;
                        if hops > hir.count(hir_lang::IdKind::Item) {
                            return Err(LowerError::ConstCycle { item });
                        }
                        e = v;
                    }
                    _ => break,
                }
            }
            _ => break,
        }
    }
    Err(unsupported(hir, Construct::NonConstant, at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scalar_ir_types_follow_pointer_width_64() {
        assert_eq!(Scalar::Int(Prim::Usize).ir(), Type::U64);
        assert_eq!(Scalar::Int(Prim::Isize).ir(), Type::I64);
        assert_eq!(Scalar::Char.ir(), Type::U32);
        assert_eq!(Scalar::Host.ir(), Type::Ref);
        assert_eq!(Scalar::Bool.size(), 1);
        assert_eq!(Scalar::Float(Prim::F32).size(), 4);
    }

    #[test]
    fn test_scalar_of_prim_refuses_str() {
        assert_eq!(Scalar::of_prim(Prim::Str), None);
        assert_eq!(Scalar::of_prim(Prim::U8), Some(Scalar::Int(Prim::U8)));
    }
}
