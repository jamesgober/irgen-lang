//! Compile-time evaluation of constant expressions: global initializers (as
//! bytes in ir-lang's layout) and the constants patterns compare against.
//!
//! Constants are literals, tuples, arrays, repeats, records without a base,
//! blocks holding only a tail, and paths to constants of those. Anything else
//! is [`Construct::NonConstant`]. Iterative, budgeted, and cycle-checked.

use alloc::{collections::BTreeSet, vec, vec::Vec};

use hir_lang::{Def, Expr, ExprId, ItemId, ItemKind, Lit, NodeRef, Prim, Res};
use intern_lang::Lookup;

use crate::{
    error::{Construct, LowerError},
    types::{Repr, Scalar, Shape, TypeMap},
    unit::{Budget, Ctx, unsupported},
};

/// Size and alignment of a value of this representation in memory.
pub(crate) fn size_align(types: &TypeMap, r: Repr) -> (u64, u32) {
    match r {
        Repr::Scalar(s) => (u64::from(s.size()), s.size()),
        Repr::Agg(i) => types.agg(i).map_or((0, 1), |a| (a.size, a.align)),
        Repr::Unit | Repr::Never => (0, 1),
    }
}

enum Task {
    Eval(ExprId, u64, Repr),
    /// Copy the first element of a repeat over the rest.
    Replicate {
        off: u64,
        elem: u64,
        count: u64,
    },
    /// A constant's value is done: it may appear again (not on this path).
    Exit(ItemId),
}

/// Evaluates `root` (of representation `repr`) into `size` bytes.
pub(crate) fn eval<L: Lookup>(
    cx: &Ctx<'_, L>,
    types: &TypeMap,
    budget: &mut Budget,
    root: ExprId,
    repr: Repr,
    size: u64,
) -> Result<Vec<u8>, LowerError> {
    let hir = cx.hir;
    let len = usize::try_from(size).map_err(|_| LowerError::BudgetExceeded {
        limit: crate::error::Limit::GlobalBytes,
    })?;
    let mut buf = vec![0u8; len];
    let mut active: BTreeSet<ItemId> = BTreeSet::new();
    let mut tasks = vec![Task::Eval(root, 0, repr)];
    while let Some(task) = tasks.pop() {
        budget.tick()?;
        let (e, off, repr) = match task {
            Task::Eval(e, off, repr) => (e, off, repr),
            Task::Replicate { off, elem, count } => {
                replicate(&mut buf, off, elem, count);
                continue;
            }
            Task::Exit(item) => {
                let _ = active.remove(&item);
                continue;
            }
        };
        let at = NodeRef::Expr(e);
        let no = || unsupported(hir, Construct::NonConstant, at);
        if cx.typed.coercion(e).is_some() {
            return Err(no());
        }
        match *hir.expr(e) {
            Expr::Lit(lit) => write_lit(&mut buf, off, lit, repr).ok_or_else(no)?,
            Expr::Tuple(elems) | Expr::Array(elems) => {
                let elems = hir.list(elems);
                if elems.is_empty() && repr == Repr::Unit {
                    continue;
                }
                let Repr::Agg(i) = repr else {
                    return Err(no());
                };
                for (k, &el) in elems.iter().enumerate().rev() {
                    let k = u32::try_from(k).map_err(|_| no())?;
                    let (fo, fr) = field_at(types, i, k).ok_or_else(no)?;
                    tasks.push(Task::Eval(el, off.saturating_add(fo), fr));
                }
            }
            Expr::Repeat { elem, .. } => {
                let Repr::Agg(i) = repr else {
                    return Err(no());
                };
                let Some(info) = types.agg(i) else {
                    return Err(no());
                };
                let Shape::Array { elem: er, len } = info.shape else {
                    return Err(no());
                };
                if len == 0 {
                    continue;
                }
                let (es, _) = size_align(types, er);
                tasks.push(Task::Replicate {
                    off,
                    elem: es,
                    count: len,
                });
                tasks.push(Task::Eval(elem, off, er));
            }
            Expr::Record {
                fields, base: None, ..
            } => {
                let Repr::Agg(i) = repr else {
                    return Err(no());
                };
                for fi in hir.list(fields).iter().rev() {
                    let Some((k, _)) = types
                        .agg(i)
                        .and_then(|a| a.field_index_by_name(fi.name.sym))
                    else {
                        return Err(no());
                    };
                    let (fo, fr) = field_at(types, i, k).ok_or_else(no)?;
                    tasks.push(Task::Eval(fi.value, off.saturating_add(fo), fr));
                }
            }
            Expr::Block(b) if b.stmts.is_empty() && b.label.is_none() => match b.tail {
                Some(t) => tasks.push(Task::Eval(t, off, repr)),
                None if repr == Repr::Unit => {}
                None => return Err(no()),
            },
            Expr::Path(p) => {
                let path = hir.path(p);
                let Res::Def(d) = path.res else {
                    return Err(no());
                };
                if d.unit() != hir.unit() || path.unresolved != 0 {
                    return Err(no());
                }
                let Def::Item(item) = d.def() else {
                    return Err(no());
                };
                match hir.item(item).kind {
                    ItemKind::Const { value: Some(v), .. } => {
                        if !active.insert(item) {
                            return Err(LowerError::ConstCycle { item });
                        }
                        tasks.push(Task::Exit(item));
                        tasks.push(Task::Eval(v, off, repr));
                    }
                    // A unit record's only value has no bytes.
                    ItemKind::Record(def) if def.shape == hir_lang::Shape::Unit => {}
                    _ => return Err(no()),
                }
            }
            _ => return Err(no()),
        }
    }
    Ok(buf)
}

/// The value of a scalar constant (a pattern's literal or constant), as bits
/// zero-extended to 64.
pub(crate) fn scalar_bits<L: Lookup>(
    cx: &Ctx<'_, L>,
    types: &TypeMap,
    budget: &mut Budget,
    e: ExprId,
    sc: Scalar,
) -> Result<u64, LowerError> {
    let size = u64::from(sc.size());
    let bytes = eval(cx, types, budget, e, Repr::Scalar(sc), size)?;
    Ok(le_bits(&bytes))
}

/// The bits of a literal, as a constant of scalar `sc` (zero-extended to 64),
/// or `None` if the literal is not of that class.
pub(crate) fn lit_bits(lit: Lit, sc: Scalar) -> Option<u64> {
    let mut buf = [0u8; 8];
    write_lit(&mut buf, 0, lit, Repr::Scalar(sc))?;
    let n = sc.size() as usize;
    Some(le_bits(buf.get(..n)?))
}

fn le_bits(bytes: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, &b) in bytes.iter().take(8).enumerate() {
        v |= u64::from(b) << (8 * i);
    }
    v
}

fn field_at(types: &TypeMap, agg: u32, k: u32) -> Option<(u64, Repr)> {
    let info = types.agg(agg)?;
    let fr = info.field(k)?;
    let off = info.offset(k, size_align(types, fr).0)?;
    Some((off, fr))
}

fn replicate(buf: &mut [u8], off: u64, elem: u64, count: u64) {
    let (Ok(off), Ok(elem)) = (usize::try_from(off), usize::try_from(elem)) else {
        return;
    };
    if elem == 0 {
        return;
    }
    for k in 1..count {
        let Ok(k) = usize::try_from(k) else {
            return;
        };
        let Some(dst) = k.checked_mul(elem).and_then(|d| d.checked_add(off)) else {
            return;
        };
        let Some(end) = dst.checked_add(elem) else {
            return;
        };
        if end > buf.len() {
            return;
        }
        buf.copy_within(off..off + elem, dst);
    }
}

/// The integer value of an integer literal.
pub(crate) fn int_value(i: hir_lang::IntLit) -> i128 {
    if i.negative {
        -i128::from(i.value)
    } else {
        i128::from(i.value)
    }
}

fn write_lit(buf: &mut [u8], off: u64, lit: Lit, repr: Repr) -> Option<()> {
    let Repr::Scalar(sc) = repr else {
        return None;
    };
    let bits: u64 = match (lit, sc) {
        (Lit::Int(i), Scalar::Int(p)) => {
            let v = int_value(i);
            if !fits(v, p) {
                return None;
            }
            v as u64
        }
        (Lit::Int(i), Scalar::Float(p)) => {
            let v = int_value(i) as f64;
            if p == Prim::F32 {
                u64::from((v as f32).to_bits())
            } else {
                v.to_bits()
            }
        }
        (Lit::Float(f), Scalar::Float(p)) => {
            if p == Prim::F32 {
                u64::from((f.value() as f32).to_bits())
            } else {
                f.bits
            }
        }
        (Lit::Bool(b), Scalar::Bool) => u64::from(b),
        (Lit::Char(c), Scalar::Char) => u64::from(u32::from(c)),
        _ => return None,
    };
    let n = sc.size() as usize;
    let start = usize::try_from(off).ok()?;
    let dst = buf.get_mut(start..start.checked_add(n)?)?;
    for (i, b) in dst.iter_mut().enumerate() {
        *b = (bits >> (8 * i)) as u8;
    }
    Some(())
}

/// Whether `v` is representable in integer primitive `p` (pointer width 64).
pub(crate) fn fits(v: i128, p: Prim) -> bool {
    let (min, max): (i128, i128) = match p {
        Prim::I8 => (i8::MIN.into(), i8::MAX.into()),
        Prim::I16 => (i16::MIN.into(), i16::MAX.into()),
        Prim::I32 => (i32::MIN.into(), i32::MAX.into()),
        Prim::I64 | Prim::Isize => (i64::MIN.into(), i64::MAX.into()),
        Prim::U8 => (0, u8::MAX.into()),
        Prim::U16 => (0, u16::MAX.into()),
        Prim::U32 => (0, u32::MAX.into()),
        Prim::U64 | Prim::Usize => (0, u64::MAX.into()),
        _ => return false,
    };
    (min..=max).contains(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lit_bits_encodes_two_complement_at_width() {
        let lit = Lit::Int(hir_lang::IntLit::signed(-1));
        assert_eq!(lit_bits(lit, Scalar::Int(Prim::I8)), Some(0xFF));
        assert_eq!(lit_bits(lit, Scalar::Int(Prim::I64)), Some(u64::MAX));
        assert_eq!(lit_bits(lit, Scalar::Int(Prim::U8)), None);
        let big = Lit::Int(hir_lang::IntLit::new(300));
        assert_eq!(lit_bits(big, Scalar::Int(Prim::U8)), None);
        assert_eq!(lit_bits(Lit::Bool(true), Scalar::Bool), Some(1));
        assert_eq!(lit_bits(Lit::Char('A'), Scalar::Char), Some(65));
        let f = Lit::Float(hir_lang::FloatLit::new(1.5));
        assert_eq!(
            lit_bits(f, Scalar::Float(Prim::F32)),
            Some(u64::from(1.5f32.to_bits()))
        );
    }

    #[test]
    fn test_replicate_copies_first_element() {
        let mut buf = [1u8, 2, 0, 0, 0, 0];
        replicate(&mut buf, 0, 2, 3);
        assert_eq!(buf, [1, 2, 1, 2, 1, 2]);
        // Out-of-range requests stop instead of panicking.
        replicate(&mut buf, 4, 2, 9);
        assert_eq!(buf, [1, 2, 1, 2, 1, 2]);
    }

    #[test]
    fn test_fits_boundaries() {
        assert!(fits(127, Prim::I8));
        assert!(!fits(128, Prim::I8));
        assert!(fits(-128, Prim::I8));
        assert!(fits(i128::from(u64::MAX), Prim::Usize));
        assert!(!fits(-1, Prim::U64));
    }
}
