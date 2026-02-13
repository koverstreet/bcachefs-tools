// LE64 bitmask getter/setter — pure Rust equivalent of the C LE64_BITMASK macro.
//
// The C macro generates static inline getter/setter functions that bindgen can't
// export. The constants (NAME_OFFSET, NAME_BITS) ARE exported. This module provides
// the same operations in pure Rust using those constants, eliminating the C shim
// functions in rust_shims.c.

use crate::c;

/// Get a bitmask field from a little-endian u64.
#[inline]
pub fn le64_bitmask_get(field: c::__le64, offset: u32, bits: u32) -> u64 {
    (u64::from_le(field) >> offset) & !(!0u64 << bits)
}

/// Set a bitmask field in a little-endian u64.
#[inline]
pub fn le64_bitmask_set(field: &mut c::__le64, offset: u32, bits: u32, v: u64) {
    let mask = !(!0u64 << bits);
    let mut val = u64::from_le(*field);
    val &= !(mask << offset);
    val |= (v & mask) << offset;
    *field = val.to_le();
}

/// Generate getter and setter methods for LE64_BITMASK fields.
///
/// Uses the `paste` crate to construct constant names from the bitmask name prefix.
/// The constants `{NAME}_OFFSET` and `{NAME}_BITS` must exist in `crate::c`.
///
/// Usage:
/// ```ignore
/// bitmask_accessors! {
///     // Array field: struct_type, field[idx], NAME => (getter, setter), ...
///     bch_sb, flags[1],
///         BCH_SB_ENCRYPTION_TYPE => (encryption_type, set_encryption_type),
///         BCH_SB_PROMOTE_TARGET  => (promote_target, set_promote_target);
///
///     // Plain field: struct_type, field, NAME => (getter, setter), ...
///     bch_member, flags,
///         BCH_MEMBER_GROUP => (group, set_group);
/// }
/// ```
#[macro_export]
macro_rules! bitmask_accessors {
    // Array field variant
    ( $struct_ty:ident, $field:ident [ $idx:expr ],
      $( $name:ident => ( $getter:ident, $setter:ident ) ),+ $(,)?
      $(; $($rest:tt)* )?
    ) => {
        impl $crate::c::$struct_ty {
            $(
                #[inline]
                pub fn $getter(&self) -> u64 {
                    $crate::paste! {
                        $crate::bitmask::le64_bitmask_get(
                            self.$field[$idx],
                            $crate::c::[< $name _OFFSET >],
                            $crate::c::[< $name _BITS >],
                        )
                    }
                }

                #[inline]
                pub fn $setter(&mut self, v: u64) {
                    $crate::paste! {
                        $crate::bitmask::le64_bitmask_set(
                            &mut self.$field[$idx],
                            $crate::c::[< $name _OFFSET >],
                            $crate::c::[< $name _BITS >],
                            v,
                        )
                    }
                }
            )+
        }

        $( $crate::bitmask_accessors!{ $($rest)* } )?
    };

    // Plain field variant
    ( $struct_ty:ident, $field:ident,
      $( $name:ident => ( $getter:ident, $setter:ident ) ),+ $(,)?
      $(; $($rest:tt)* )?
    ) => {
        impl $crate::c::$struct_ty {
            $(
                #[inline]
                pub fn $getter(&self) -> u64 {
                    $crate::paste! {
                        $crate::bitmask::le64_bitmask_get(
                            self.$field,
                            $crate::c::[< $name _OFFSET >],
                            $crate::c::[< $name _BITS >],
                        )
                    }
                }

                #[inline]
                pub fn $setter(&mut self, v: u64) {
                    $crate::paste! {
                        $crate::bitmask::le64_bitmask_set(
                            &mut self.$field,
                            $crate::c::[< $name _OFFSET >],
                            $crate::c::[< $name _BITS >],
                            v,
                        )
                    }
                }
            )+
        }

        $( $crate::bitmask_accessors!{ $($rest)* } )?
    };

    // Base case — empty
    () => {};
}
