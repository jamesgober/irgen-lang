//! Places: locals, globals, fields, and array elements, as SSA variables or
//! addresses, and reading and writing them.

use hir_lang::{Def, Expr, ExprId, ItemKind, NodeRef, Res};
use intern_lang::Lookup;
use ir_lang::{CmpOp, ConvOp, Policy, Type};

use super::{FnCx, Local, Place, Task, Val, internal};
use crate::{
    error::LowerError,
    rt,
    types::{Repr, Scalar, Shape},
};

impl<L: Lookup> FnCx<'_, '_, '_, L> {
    /// `Task::Place(e)`: pushes the place `e` denotes.
    pub(super) fn place(&mut self, e: ExprId) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        match *hir.expr(e) {
            Expr::Path(p) => {
                let path = hir.path(p);
                match path.res {
                    Res::Local(b) => {
                        let place = self.binder_place(b)?;
                        self.places.push(place);
                        Ok(())
                    }
                    Res::Def(d) => match d.def() {
                        Def::Item(item) => match hir.get_item(item).map(|i| i.kind) {
                            Some(ItemKind::Global { .. }) => {
                                let g = self.t.global(item).ok_or_else(|| internal(self.node))?;
                                let place = match g.id {
                                    Some(id) => Place::Mem(self.b.global_addr(id)?, g.repr),
                                    None => Place::Unit,
                                };
                                self.places.push(place);
                                Ok(())
                            }
                            _ => self.place_from_value(e),
                        },
                        Def::Variant(_) => Err(internal(self.node)),
                    },
                    _ => Err(internal(self.node)),
                }
            }
            Expr::Field { base, .. } => {
                self.tasks.push(Task::PlaceField(e));
                self.tasks.push(Task::Place(base));
                Ok(())
            }
            Expr::Index { base, index } => {
                self.tasks.push(Task::PlaceIndex(e));
                self.tasks.push(Task::Expr(index));
                self.tasks.push(Task::Place(base));
                Ok(())
            }
            _ => self.place_from_value(e),
        }
    }

    fn place_from_value(&mut self, e: ExprId) -> Result<(), LowerError> {
        self.tasks.push(Task::PlaceOfVal);
        self.tasks.push(Task::Expr(e));
        Ok(())
    }

    pub(super) fn binder_place(&mut self, b: hir_lang::BinderId) -> Result<Place, LowerError> {
        Ok(match self.local(b)? {
            Local::Var(var, sc) => Place::Var(var, sc),
            Local::Slot(slot, agg) => Place::Mem(self.b.stack_addr(slot)?, Repr::Agg(agg)),
            Local::Ptr(v, agg) => Place::Mem(v, Repr::Agg(agg)),
            Local::Unit => Place::Unit,
            Local::Alias(i) => *self
                .aliases
                .get(i as usize)
                .ok_or_else(|| internal(self.node))?,
        })
    }

    pub(super) fn place_of_val(&mut self, v: Val) -> Result<Place, LowerError> {
        Ok(match v {
            Val::Mem(slot, agg) => Place::Mem(self.b.stack_addr(slot)?, Repr::Agg(agg)),
            Val::Scalar(..) => Place::Value(v),
            Val::None => Place::Unit,
        })
    }

    /// `Task::PlaceField(e)`: the field of the base place.
    pub(super) fn place_field(&mut self, e: ExprId) -> Result<(), LowerError> {
        let Expr::Field { member, .. } = *self.cx.hir.expr(e) else {
            return Err(internal(self.node));
        };
        let base = self.pop_place()?;
        let place = match base {
            Place::Mem(addr, Repr::Agg(agg)) => {
                let (id, (k, fr)) = {
                    let a = self.agg(agg)?;
                    (a.id, a.member(member).ok_or_else(|| internal(self.node))?)
                };
                let f = self.b.field_addr(addr, id, k)?;
                Place::Mem(f, fr)
            }
            Place::Unit => Place::Unit,
            _ => return Err(internal(self.node)),
        };
        self.places.push(place);
        Ok(())
    }

    /// `Task::PlaceIndex(e)`: the element of the base array, after a bounds
    /// check (an index outside `0..len` raises `IndexOutOfBounds`).
    pub(super) fn place_index(&mut self, _e: ExprId) -> Result<(), LowerError> {
        let index = self.pop_val()?;
        let base = self.pop_place()?;
        let Place::Mem(addr, Repr::Agg(agg)) = base else {
            return Err(internal(self.node));
        };
        let (elem, len) = match self.agg(agg)?.shape {
            Shape::Array { elem, len } => (elem, len),
            Shape::Struct { .. } => return Err(internal(self.node)),
        };
        let (i64v, ok) = match index {
            Val::Scalar(v, sc) => self.index_u64(v, sc, len)?,
            // Dead code: any address will do.
            _ => {
                let z = self.b.iconst(Type::U64, 0)?;
                (z, None)
            }
        };
        if let Some(ok) = ok {
            let oob = self.new_block()?;
            let cont = self.new_block()?;
            self.branch(ok, cont, oob)?;
            self.enter_sealed(oob)?;
            self.raise_const(rt::E_INDEX_OUT_OF_BOUNDS)?;
            self.enter_sealed(cont)?;
        }
        let mt = self
            .t
            .types
            .elem_memtype(elem)
            .ok_or_else(|| internal(self.node))?;
        let a = self.b.elem_addr(addr, mt, i64v)?;
        self.places.push(Place::Mem(a, elem));
        Ok(())
    }

    /// The index widened to `u64` (a negative signed index becomes huge),
    /// and the in-bounds condition (`None` when every value is in bounds).
    fn index_u64(
        &mut self,
        v: ir_lang::Value,
        sc: Scalar,
        len: u64,
    ) -> Result<(ir_lang::Value, Option<ir_lang::Value>), LowerError> {
        let ty = sc.ir();
        let bits = ty.int_bits().unwrap_or(64);
        let wide = if bits == 64 {
            if ty == Type::U64 {
                v
            } else {
                self.b
                    .convert(ConvOp::Bitcast, Policy::NONE, v, Type::U64)?
            }
        } else if ty.is_signed_int() {
            let s = self.b.convert(ConvOp::Sext, Policy::NONE, v, Type::I64)?;
            self.b
                .convert(ConvOp::Bitcast, Policy::NONE, s, Type::U64)?
        } else {
            self.b.convert(ConvOp::Zext, Policy::NONE, v, Type::U64)?
        };
        // An unsigned index narrower than the length is always in bounds.
        if !ty.is_signed_int() && bits < 64 && (1u128 << bits) <= u128::from(len) {
            return Ok((wide, None));
        }
        let l = self.b.iconst(Type::U64, i128::from(len))?;
        let ok = self.b.compare(CmpOp::Lt, wide, l)?;
        Ok((wide, Some(ok)))
    }

    /// Reads a place: scalars load (or read their variable), aggregates are
    /// copied into a fresh temporary, so a later write to the place cannot
    /// change a value already read.
    pub(super) fn read(&mut self, p: Place) -> Result<Val, LowerError> {
        Ok(match p {
            Place::Var(var, sc) => {
                if self.dead {
                    Val::Scalar(self.zero(sc.ir())?, sc)
                } else {
                    Val::Scalar(self.b.use_var(var)?, sc)
                }
            }
            Place::Mem(addr, Repr::Scalar(sc)) => Val::Scalar(self.load(sc, addr)?, sc),
            Place::Mem(addr, Repr::Agg(agg)) => {
                let slot = self.new_slot(agg)?;
                let dst = self.b.stack_addr(slot)?;
                self.copy(dst, addr, agg)?;
                Val::Mem(slot, agg)
            }
            Place::Mem(_, Repr::Unit | Repr::Never) | Place::Unit => Val::None,
            Place::Value(v) => v,
        })
    }

    /// Writes a value to a place.
    pub(super) fn write(&mut self, p: Place, v: Val) -> Result<(), LowerError> {
        match (p, v) {
            (Place::Var(var, _), Val::Scalar(x, _)) => self.b.def_var(var, x)?,
            (Place::Mem(addr, r), v) => self.write_mem(addr, r, v)?,
            // Writing into a temporary's stand-in, unit, or from dead code.
            _ => {}
        }
        Ok(())
    }

    /// `Task::LetPlace(e)`: names the evaluated place in the body.
    pub(super) fn let_place(&mut self, e: ExprId) -> Result<(), LowerError> {
        let Expr::LetPlace { binder, .. } = *self.cx.hir.expr(e) else {
            return Err(internal(self.node));
        };
        let p = self.pop_place()?;
        let i = u32::try_from(self.aliases.len()).map_err(|_| internal(self.node))?;
        self.aliases.push(p);
        self.set_local(binder, Local::Alias(i));
        Ok(())
    }

    /// A path in value position.
    pub(super) fn value_path(&mut self, e: ExprId, p: hir_lang::PathId) -> Result<(), LowerError> {
        let hir = self.cx.hir;
        match hir.path(p).res {
            Res::Local(_) => {
                self.tasks.push(Task::Read);
                self.tasks.push(Task::Place(e));
                Ok(())
            }
            Res::Def(d) => {
                let Def::Item(item) = d.def() else {
                    return Err(internal(self.node));
                };
                match hir.get_item(item).map(|i| i.kind) {
                    Some(ItemKind::Const { value: Some(v), .. }) => {
                        if !self.const_set.insert(item) {
                            return Err(LowerError::ConstCycle { item });
                        }
                        self.consts.push(item);
                        self.tasks.push(Task::ConstExit);
                        self.tasks.push(Task::Expr(v));
                        Ok(())
                    }
                    Some(ItemKind::Global { .. }) => {
                        self.tasks.push(Task::Read);
                        self.tasks.push(Task::Place(e));
                        Ok(())
                    }
                    Some(ItemKind::Record(_)) => {
                        // A unit record's value.
                        match self.repr_of(e)? {
                            Repr::Agg(agg) => {
                                let slot = self.new_slot(agg)?;
                                self.vals.push(Val::Mem(slot, agg));
                            }
                            _ => self.vals.push(Val::None),
                        }
                        Ok(())
                    }
                    _ => Err(crate::unit::unsupported(
                        hir,
                        crate::error::Construct::FunctionValue,
                        NodeRef::Expr(e),
                    )),
                }
            }
            _ => Err(internal(self.node)),
        }
    }
}
