//! OPS operations, conversions, and casts, with their exact policies.
//!
//! Each HIR op instance becomes one ir-lang instruction carrying exactly the
//! policy fields ir-lang requires for its operand class (a float operation's
//! overflow policy is dropped: IEEE operations consult none). An instance
//! that can raise an error (`BinaryOp::can_error` and friends, which depend
//! on the operand type) becomes a `check` terminator whose error edge goes to
//! the error block of the innermost exception region.

use alloc::vec::Vec;

use hir_lang::{Expr, ExprId, NodeRef, OpKind, Prim};
use intern_lang::Lookup;
use ir_lang::{
    BinaryOp, BlockArg, BlockCall, CmpOp, ConvOp, InstData, Policy, PolicyFields, Type, UnaryOp,
    Value,
};

use super::{FnCx, Val, internal};
use crate::{
    error::{Construct, LowerError},
    types::{Repr, Scalar},
    unit::unsupported,
};

fn overflow(o: hir_lang::Overflow) -> Option<ir_lang::Overflow> {
    match o {
        hir_lang::Overflow::Error => Some(ir_lang::Overflow::Error),
        hir_lang::Overflow::Wrap => Some(ir_lang::Overflow::Wrap),
        hir_lang::Overflow::Trap => Some(ir_lang::Overflow::Trap),
        hir_lang::Overflow::Promote => None,
    }
}

fn div_zero(d: hir_lang::DivZero) -> ir_lang::DivZero {
    match d {
        hir_lang::DivZero::Error => ir_lang::DivZero::Error,
        hir_lang::DivZero::Trap => ir_lang::DivZero::Trap,
    }
}

fn shift(s: hir_lang::Shift) -> ir_lang::Shift {
    match s {
        hir_lang::Shift::Error => ir_lang::Shift::Error,
        hir_lang::Shift::Mask => ir_lang::Shift::Mask,
        hir_lang::Shift::Saturate => ir_lang::Shift::Saturate,
    }
}

fn float_to_int(f: hir_lang::FloatToInt) -> ir_lang::FloatToInt {
    match f {
        hir_lang::FloatToInt::Error => ir_lang::FloatToInt::Error,
        hir_lang::FloatToInt::Saturate => ir_lang::FloatToInt::Saturate,
    }
}

/// The ir-lang policy with exactly the `req` fields, taken from HIR's.
fn policy(hp: hir_lang::Policy, req: PolicyFields) -> Option<Policy> {
    let mut p = Policy::NONE;
    if req.overflow {
        p.overflow = Some(overflow(hp.overflow?)?);
    }
    if req.div_zero {
        p.div_zero = Some(div_zero(hp.div_zero?));
    }
    if req.shift {
        p.shift = Some(shift(hp.shift?));
    }
    if req.float_to_int {
        p.float_to_int = Some(float_to_int(hp.float_to_int?));
    }
    Some(p)
}

fn binary_op(k: OpKind) -> Option<BinaryOp> {
    Some(match k {
        OpKind::Add => BinaryOp::Add,
        OpKind::Sub => BinaryOp::Sub,
        OpKind::Mul => BinaryOp::Mul,
        OpKind::Div => BinaryOp::Div,
        OpKind::Rem => BinaryOp::Rem,
        OpKind::FloorDiv => BinaryOp::FloorDiv,
        OpKind::FloorMod => BinaryOp::FloorMod,
        OpKind::IeeeRem => BinaryOp::IeeeRem,
        OpKind::And => BinaryOp::And,
        OpKind::Or => BinaryOp::Or,
        OpKind::Xor => BinaryOp::Xor,
        OpKind::Shl => BinaryOp::Shl,
        OpKind::Shr => BinaryOp::Shr,
        OpKind::Min => BinaryOp::Min,
        OpKind::Max => BinaryOp::Max,
        OpKind::TotalCmp => BinaryOp::TotalCmp,
        OpKind::Pow => BinaryOp::Pow,
        _ => return None,
    })
}

fn unary_op(k: OpKind) -> Option<UnaryOp> {
    Some(match k {
        OpKind::Neg => UnaryOp::Neg,
        OpKind::Abs => UnaryOp::Abs,
        OpKind::Not | OpKind::BitNot => UnaryOp::Not,
        OpKind::Sqrt => UnaryOp::Sqrt,
        OpKind::Floor => UnaryOp::Floor,
        OpKind::Ceil => UnaryOp::Ceil,
        OpKind::Trunc => UnaryOp::Trunc,
        OpKind::Round => UnaryOp::Round,
        OpKind::RoundEven => UnaryOp::RoundEven,
        _ => return None,
    })
}

fn cmp_op(k: OpKind) -> Option<CmpOp> {
    Some(match k {
        OpKind::Eq => CmpOp::Eq,
        OpKind::Ne => CmpOp::Ne,
        OpKind::Lt => CmpOp::Lt,
        OpKind::Le => CmpOp::Le,
        OpKind::Gt => CmpOp::Gt,
        OpKind::Ge => CmpOp::Ge,
        _ => return None,
    })
}

impl<L: Lookup> FnCx<'_, '_, '_, L> {
    /// `Task::Op(e)`.
    pub(super) fn op_task(&mut self, e: ExprId) -> Result<(), LowerError> {
        let Expr::Op { op, args } = *self.cx.hir.expr(e) else {
            return Err(internal(self.node));
        };
        let n = args.len();
        let at = self
            .vals
            .len()
            .checked_sub(n)
            .ok_or_else(|| internal(self.node))?;
        let vals: Vec<Val> = self.vals.drain(at..).collect();
        let result = self.repr_of(e)?;
        let v = self.apply_op(op, &vals, result)?;
        self.vals.push(v);
        Ok(())
    }

    /// `Task::Compound(e)`: `place op= value`, the place evaluated once.
    pub(super) fn compound(&mut self, e: ExprId) -> Result<(), LowerError> {
        let Expr::Assign { op: Some(op), .. } = *self.cx.hir.expr(e) else {
            return Err(internal(self.node));
        };
        let rhs = self.pop_val()?;
        let old = self.pop_val()?;
        let place = self.pop_place()?;
        let result = self.repr_of(e)?;
        let v = self.apply_op(op, &[old, rhs], result)?;
        self.write(place, v)?;
        self.vals.push(v);
        Ok(())
    }

    fn apply_op(
        &mut self,
        op: hir_lang::Op,
        vals: &[Val],
        result: Repr,
    ) -> Result<Val, LowerError> {
        let Repr::Scalar(rsc) = result else {
            return Err(internal(self.node));
        };
        let mut xs: Vec<(Value, Scalar)> = Vec::with_capacity(vals.len());
        for v in vals {
            match *v {
                Val::Scalar(x, sc) => xs.push((x, sc)),
                // An operand from dead code: the operation does not run.
                _ => return Ok(Val::Scalar(self.zero(rsc.ir())?, rsc)),
            }
        }
        let x = self.op_value(op, &xs, rsc)?;
        Ok(Val::Scalar(x, rsc))
    }

    fn op_value(
        &mut self,
        op: hir_lang::Op,
        xs: &[(Value, Scalar)],
        rsc: Scalar,
    ) -> Result<Value, LowerError> {
        let bad = || internal(None);
        let &(a, asc) = xs.first().ok_or_else(bad)?;
        let aty = asc.ir();
        let k = op.kind;
        if let Some(bop) = binary_op(k) {
            let &(mut b, bsc) = xs.get(1).ok_or_else(bad)?;
            if matches!(k, OpKind::Shl | OpKind::Shr) && bsc.ir() != aty {
                b = self.shift_amount(b, bsc, aty, op.policy)?;
            }
            let p = if asc.is_int_like() {
                policy(op.policy, Policy::required_binary(bop, aty)).ok_or_else(bad)?
            } else {
                Policy::NONE
            };
            if bop == BinaryOp::Pow
                && aty.is_signed_int()
                && p.overflow == Some(ir_lang::Overflow::Trap)
            {
                self.negative_exponent_guard(b, aty)?;
            }
            return self.emit_op(
                InstData::Binary {
                    op: bop,
                    policy: p,
                    args: [a, b],
                },
                rsc.ir(),
            );
        }
        if let Some(uop) = unary_op(k) {
            let p = if asc.is_int_like() {
                policy(op.policy, Policy::required_unary(uop, aty)).ok_or_else(bad)?
            } else {
                Policy::NONE
            };
            return self.emit_op(
                InstData::Unary {
                    op: uop,
                    policy: p,
                    arg: a,
                },
                rsc.ir(),
            );
        }
        if let Some(cop) = cmp_op(k) {
            let &(b, _) = xs.get(1).ok_or_else(bad)?;
            return Ok(self.b.compare(cop, a, b)?);
        }
        match k {
            OpKind::Fma => {
                let (&(b, _), &(c, _)) = (xs.get(1).ok_or_else(bad)?, xs.get(2).ok_or_else(bad)?);
                Ok(self.b.fma(a, b, c)?)
            }
            OpKind::IntCast(_) => {
                let to = rsc.ir();
                if aty == to {
                    return Ok(a);
                }
                let p = policy(op.policy, Policy::required_conversion(ConvOp::IntCast))
                    .ok_or_else(bad)?;
                self.convert(ConvOp::IntCast, p, a, to)
            }
            OpKind::Zext(_) | OpKind::Sext(_) | OpKind::Narrow(_) => {
                let to = rsc.ir();
                let (fb, tb) = (aty.int_bits().unwrap_or(64), to.int_bits().unwrap_or(64));
                if tb > fb {
                    let c = if matches!(k, OpKind::Sext(_)) {
                        ConvOp::Sext
                    } else {
                        ConvOp::Zext
                    };
                    self.convert(c, Policy::NONE, a, to)
                } else if tb < fb {
                    self.convert(ConvOp::Narrow, Policy::NONE, a, to)
                } else if aty == to {
                    Ok(a)
                } else {
                    self.convert(ConvOp::Bitcast, Policy::NONE, a, to)
                }
            }
            OpKind::IntToFloat(_) => self.convert(ConvOp::IntToFloat, Policy::NONE, a, rsc.ir()),
            OpKind::FloatToInt(_) => {
                let p = policy(op.policy, Policy::required_conversion(ConvOp::FloatToInt))
                    .ok_or_else(bad)?;
                self.convert(ConvOp::FloatToInt, p, a, rsc.ir())
            }
            OpKind::Bitcast(_) => {
                if aty == rsc.ir() {
                    Ok(a)
                } else {
                    self.convert(ConvOp::Bitcast, Policy::NONE, a, rsc.ir())
                }
            }
            OpKind::BoolToInt(_) => self.convert(ConvOp::BoolToInt, Policy::NONE, a, rsc.ir()),
            OpKind::F32ToF64 | OpKind::F64ToF32 => {
                self.convert(ConvOp::FloatCast, Policy::NONE, a, rsc.ir())
            }
            OpKind::CharFromU32 => self.convert(ConvOp::CharFromU32, Policy::NONE, a, Type::U32),
            _ => Err(bad()),
        }
    }

    /// OPS v2 makes a negative exponent `NegativeExponent` (E0006) under
    /// every overflow policy but `promote`, `trap` included; ir-lang's `pow`
    /// traps on it under `trap` (IR.md §4). Raise the error before the
    /// operation, so the native tier agrees with the others.
    fn negative_exponent_guard(&mut self, e: Value, ty: Type) -> Result<(), LowerError> {
        let zero = self.b.iconst(ty, 0)?;
        let neg = self.b.compare(CmpOp::Lt, e, zero)?;
        let raise = self.new_block()?;
        let cont = self.new_block()?;
        self.branch(neg, raise, cont)?;
        self.enter_sealed(raise)?;
        self.raise_const(ir_lang::OpError::NegativeExponent.code())?;
        self.enter_sealed(cont)
    }

    fn convert(&mut self, op: ConvOp, p: Policy, a: Value, to: Type) -> Result<Value, LowerError> {
        self.emit_op(
            InstData::Convert {
                op,
                policy: p,
                to,
                arg: a,
            },
            to,
        )
    }

    /// Emits an operation: a plain instruction, or a `check` when it can
    /// raise an error.
    fn emit_op(&mut self, data: InstData, result: Type) -> Result<Value, LowerError> {
        let can = match &data {
            InstData::Unary { op, policy, .. } => op.can_error(*policy),
            InstData::Binary { op, policy, args } => {
                let ty = self
                    .b
                    .func()
                    .value_type(args[0])
                    .ok_or_else(|| internal(self.node))?;
                op.can_error(*policy, ty)
            }
            InstData::Convert { op, policy, .. } => op.can_error(*policy),
            _ => false,
        };
        if !can {
            let inst = self.b.insert(data)?;
            return self
                .b
                .results(inst)
                .first()
                .ok_or_else(|| internal(self.node));
        }
        if self.dead {
            return self.zero(result);
        }
        let normal = self.new_block_with(&[result])?;
        let err = self.check_error_block()?;
        self.b.check(
            data,
            BlockCall::with_args(normal, &[BlockArg::Result(0)]),
            BlockCall::with_args(err, &[BlockArg::ErrorCode]),
        )?;
        self.mark(normal);
        self.mark(err);
        self.enter_sealed(normal)?;
        Ok(self.b.block_param(normal, 0)?)
    }

    /// A shift amount of another integer type, converted to the operand type
    /// `to` with the same outcome under the shift policy: OPS reads the
    /// amount in its own type, ir-lang in the operand's.
    fn shift_amount(
        &mut self,
        b: Value,
        bsc: Scalar,
        to: Type,
        hp: hir_lang::Policy,
    ) -> Result<Value, LowerError> {
        let from = bsc.ir();
        let low = self.int_resize(b, from, to)?;
        let Some(sh) = hp.shift else {
            return Err(internal(self.node));
        };
        if sh == hir_lang::Shift::Mask {
            // `n mod width` depends only on the low bits, which a resize keeps.
            return Ok(low);
        }
        let width = to.int_bits().unwrap_or(64);
        let w_from = self.b.iconst(from, i128::from(width))?;
        let big = self.b.compare(CmpOp::Ge, b, w_from)?;
        let w_to = self.b.iconst(to, i128::from(width))?;
        let mut amount = self.b.select(big, w_to, low)?;
        if from.is_signed_int() {
            let zero = self.b.iconst(from, 0)?;
            let neg = self.b.compare(CmpOp::Lt, b, zero)?;
            if to.is_signed_int() {
                let m1 = self.b.iconst(to, -1)?;
                amount = self.b.select(neg, m1, amount)?;
            } else if sh == hir_lang::Shift::Error {
                // Any amount at least the width raises ShiftOutOfRange too.
                amount = self.b.select(neg, w_to, amount)?;
            } else {
                // `saturate` on an unsigned operand cannot see a negative
                // amount in ir-lang: raise ShiftOutOfRange (E0003) here.
                let raise = self.new_block()?;
                let cont = self.new_block()?;
                self.branch(neg, raise, cont)?;
                self.enter_sealed(raise)?;
                self.raise_const(ir_lang::OpError::ShiftOutOfRange.code())?;
                self.enter_sealed(cont)?;
            }
        }
        Ok(amount)
    }

    /// `Task::Cast(e)`: `expr as T` with the cast's `overflow` and
    /// `float_to_int` policies.
    pub(super) fn cast_task(&mut self, e: ExprId) -> Result<(), LowerError> {
        let Expr::Cast { policy: hp, .. } = *self.cx.hir.expr(e) else {
            return Err(internal(self.node));
        };
        let v = self.pop_val()?;
        let target = self.repr_of(e)?;
        let (x, from) = match v {
            Val::Scalar(x, sc) => (x, sc),
            other => {
                // An aggregate cast is the identity (typeck unified the
                // types); dead code has no value.
                let out = match (other, target) {
                    (_, Repr::Scalar(sc)) => Val::Scalar(self.zero(sc.ir())?, sc),
                    (o, _) => o,
                };
                self.vals.push(out);
                return Ok(());
            }
        };
        let Repr::Scalar(to) = target else {
            return Err(unsupported(
                self.cx.hir,
                Construct::Coercion,
                NodeRef::Expr(e),
            ));
        };
        let (fty, tty) = (from.ir(), to.ir());
        let out = if fty == tty {
            x
        } else {
            let req = |c: ConvOp| policy(hp, Policy::required_conversion(c));
            match (from, to) {
                (Scalar::Int(_), Scalar::Int(_)) | (Scalar::Char, Scalar::Int(_)) => {
                    let p = req(ConvOp::IntCast).ok_or_else(|| internal(self.node))?;
                    self.convert(ConvOp::IntCast, p, x, tty)?
                }
                (Scalar::Int(_), Scalar::Float(_)) => {
                    self.convert(ConvOp::IntToFloat, Policy::NONE, x, tty)?
                }
                (Scalar::Float(_), Scalar::Int(_)) => {
                    let p = req(ConvOp::FloatToInt).ok_or_else(|| internal(self.node))?;
                    self.convert(ConvOp::FloatToInt, p, x, tty)?
                }
                (Scalar::Float(_), Scalar::Float(_)) => {
                    self.convert(ConvOp::FloatCast, Policy::NONE, x, tty)?
                }
                (Scalar::Bool, Scalar::Int(_)) => {
                    self.convert(ConvOp::BoolToInt, Policy::NONE, x, tty)?
                }
                (Scalar::Int(Prim::U8), Scalar::Char) => {
                    self.convert(ConvOp::Zext, Policy::NONE, x, tty)?
                }
                _ => {
                    return Err(unsupported(
                        self.cx.hir,
                        Construct::Coercion,
                        NodeRef::Expr(e),
                    ));
                }
            }
        };
        self.vals.push(Val::Scalar(out, to));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_policy_takes_exactly_the_required_fields() {
        let hp = hir_lang::Op::new(OpKind::Div).policy;
        let req = Policy::required_binary(BinaryOp::Div, Type::I32);
        let p = policy(hp, req).unwrap_or(Policy::NONE);
        assert!(req.matches(p));
        // A float division requires nothing; the HIR fields are dropped.
        let req = Policy::required_binary(BinaryOp::Div, Type::F64);
        assert_eq!(policy(hp, req), Some(Policy::NONE));
        // Promote has no ir-lang form.
        let hp = hir_lang::Op::new(OpKind::Add)
            .with_overflow(hir_lang::Overflow::Promote)
            .policy;
        assert_eq!(
            policy(hp, Policy::required_binary(BinaryOp::Add, Type::I64)),
            None
        );
    }
}
