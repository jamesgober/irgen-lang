//! The run-time interface lowered code relies on.
//!
//! Lowered code is self-contained except for one host function, which
//! allocates exception records:
//!
//! ```text
//! import @__irgen_exception (u32 tag, u64 bits) -> (ptr) [nounwind, nogc]
//! ```
//!
//! It returns a pointer to a fresh [`RECORD_SIZE`]-byte record, aligned to
//! [`RECORD_ALIGN`], holding `tag` (a `u32` at [`TAG_OFFSET`]) and `bits` (a
//! `u64` at [`BITS_OFFSET`]), little-endian. The record must outlive the
//! frames it unwinds through, so it lives on the host's heap; it is declared
//! `nogc` (not a safepoint: allocating it never moves a reference) and
//! `nounwind`. A module declares it only when some function may throw or
//! raise.
//!
//! `throw v` stores the thrown value's [`tag`] and its bits (zero-extended to
//! 64); a run-time error under [`ErrorMode::Raise`](crate::ErrorMode::Raise)
//! stores [`TAG_ERROR`] and the error's code. An exception that leaves the
//! outermost function is a `resume` with the record's address as payload.
//!
//! # Examples
//!
//! ```
//! use hir_lang::Prim;
//! use irgen_lang::rt;
//!
//! assert_eq!(rt::EXCEPTION, "__irgen_exception");
//! assert_eq!(rt::tag(Prim::I64), Some(5));
//! assert_eq!(rt::E_NO_MATCH, 200);
//! ```

use hir_lang::Prim;

/// The symbol of the host function that allocates exception records.
pub const EXCEPTION: &str = "__irgen_exception";

/// Size of an exception record in bytes.
pub const RECORD_SIZE: u64 = 16;

/// Alignment of an exception record in bytes.
pub const RECORD_ALIGN: u64 = 8;

/// Offset of the `u32` tag in an exception record.
pub const TAG_OFFSET: u64 = 0;

/// Offset of the `u64` value bits in an exception record.
pub const BITS_OFFSET: u64 = 8;

/// The tag of a run-time error; the bits are its code (`E_*`, or an OPS code
/// 1–6).
pub const TAG_ERROR: u32 = 0;

/// `IndexOutOfBounds` (the family's E0102): an array index outside `0..len`.
pub const E_INDEX_OUT_OF_BOUNDS: u32 = 102;

/// `NoMatch` (HIR's E0200): a `match` none of whose arms matches.
pub const E_NO_MATCH: u32 = 200;

/// The tag of a thrown value of primitive type `prim`, or `None` for `str`
/// (strings cannot be thrown in the static subset).
///
/// Tags are distinct per primitive, so a `catch` arm testing `u32` does not
/// catch a thrown `char`, nor `i64` a thrown `isize`.
///
/// # Examples
///
/// ```
/// use hir_lang::Prim;
/// use irgen_lang::rt::tag;
///
/// assert_eq!(tag(Prim::Bool), Some(1));
/// assert_eq!(tag(Prim::Char), Some(14));
/// assert_eq!(tag(Prim::Str), None);
/// ```
#[must_use]
pub const fn tag(prim: Prim) -> Option<u32> {
    Some(match prim {
        Prim::Bool => 1,
        Prim::I8 => 2,
        Prim::I16 => 3,
        Prim::I32 => 4,
        Prim::I64 => 5,
        Prim::Isize => 6,
        Prim::U8 => 7,
        Prim::U16 => 8,
        Prim::U32 => 9,
        Prim::U64 => 10,
        Prim::Usize => 11,
        Prim::F32 => 12,
        Prim::F64 => 13,
        Prim::Char => 14,
        Prim::Str => return None,
    })
}

/// The primitive a thrown-value tag stands for: the inverse of [`tag`].
///
/// # Examples
///
/// ```
/// use hir_lang::Prim;
/// use irgen_lang::rt::{prim_of_tag, tag, TAG_ERROR};
///
/// assert_eq!(prim_of_tag(tag(Prim::U16).unwrap()), Some(Prim::U16));
/// assert_eq!(prim_of_tag(TAG_ERROR), None);
/// ```
#[must_use]
pub const fn prim_of_tag(tag: u32) -> Option<Prim> {
    Some(match tag {
        1 => Prim::Bool,
        2 => Prim::I8,
        3 => Prim::I16,
        4 => Prim::I32,
        5 => Prim::I64,
        6 => Prim::Isize,
        7 => Prim::U8,
        8 => Prim::U16,
        9 => Prim::U32,
        10 => Prim::U64,
        11 => Prim::Usize,
        12 => Prim::F32,
        13 => Prim::F64,
        14 => Prim::Char,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tags_round_trip_and_are_distinct() {
        let prims = [
            Prim::Bool,
            Prim::I8,
            Prim::I16,
            Prim::I32,
            Prim::I64,
            Prim::Isize,
            Prim::U8,
            Prim::U16,
            Prim::U32,
            Prim::U64,
            Prim::Usize,
            Prim::F32,
            Prim::F64,
            Prim::Char,
        ];
        for p in prims {
            let t = tag(p).unwrap_or(TAG_ERROR);
            assert_ne!(t, TAG_ERROR);
            assert_eq!(prim_of_tag(t), Some(p));
        }
        assert_eq!(tag(Prim::Str), None);
        assert_eq!(prim_of_tag(15), None);
    }
}
