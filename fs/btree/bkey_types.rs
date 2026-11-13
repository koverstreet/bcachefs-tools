// SPDX-License-Identifier: GPL-2.0

//! Rust's own key types, btree/bkey.rs's BkeySC and BkeyS, are checked to
//! have the layouts of C's bkey_s_c and bkey_s. The typed keys C generates
//! for each key type from BCH_BKEY_TYPES() - bkey_i_<type>, bkey_s_c_<type>,
//! bkey_s_<type> - stay C's; Rust's are BkeyI, BkeySC and BkeyS of the value
//! type, checked against them.

#![allow(non_camel_case_types)]

use crate::btree::bkey::{BkeyI, BkeyS, BkeySC};
use crate::cstructs::c as cs;
use crate::cstructs::c::BkeyType_arms;
use crate::types::c_default;
use core::ffi::c_uint;
use cstruct_macros::{MaybeZeroable, bitfield, bch_enum, BitfieldMembers};
use nestify::nest;
use crate::format::Btree;

/* DOC_LATEX(bkey-structures)
 *
 * \paragraph{Search keys and bkeys}
 *
 * The btree separates the search key (\texttt{struct bpos}) from the outer
 * container that holds a key and value (\texttt{struct bkey}).
 *
 * \begin{verbatim}
 * struct bpos {
 *     u64  inode;      // high bits of the search key
 *     u64  offset;     // middle bits
 *     u32  snapshot;   // low bits
 * };
 *
 * struct bkey {
 *     u8          u64s;    // size of key + value in u64s
 *     u8          format;  // internal: packed bkey format
 *     u8          type;    // value type (KEY_TYPE_extent, etc.)
 *     u8          pad;
 *     bversion    bversion;
 *     u32         size;    // extent size in sectors (0 for non-extents)
 *     struct bpos p;       // position (for extents: end position)
 * };
 * \end{verbatim}
 *
 * The three fields of \texttt{bpos} form a single large integer for
 * comparison. Not all code uses all fields---the inode field generally
 * corresponds to an inode number, and for extents the offset field
 * is the file offset. The snapshot field enables snapshot-aware lookups.
 *
 * The \texttt{type} field determines how the value is interpreted. Use
 * \texttt{bkey\_val\_u64s()} or \texttt{bkey\_val\_bytes()} to get the value
 * size---the \texttt{u64s} field includes the key header.
 *
 * For extents, \texttt{p.offset} points to the \emph{end} of the extent, not
 * the start. A key with offset 8 and size 8 covers sectors 0--7. This makes
 * ascending iteration over extent ranges more natural.
 *
 * \paragraph{Wrapper types}
 *
 * Values are stored inline with keys on disk, but due to packing they are
 * typically accessed via wrapper types that hold pointers:
 *
 * \begin{description}
 * \item[\texttt{bkey\_i}] Key with inline value (for allocation/insertion)
 * \item[\texttt{bkey\_s}] Key with split value (pointers to key and value)
 * \item[\texttt{bkey\_s\_c}] Constant key with split value (for lookups)
 * \end{description}
 *
 * Each value type generates corresponding typed wrappers. For example,
 * \texttt{struct bch\_xattr} generates:
 *
 * \begin{itemize}
 * \item \texttt{bkey\_i\_xattr} -- inline xattr key
 * \item \texttt{bkey\_s\_xattr} -- split xattr key
 * \item \texttt{bkey\_s\_c\_xattr} -- const split xattr key
 * \end{itemize}
 *
 * To convert from a generic \texttt{bkey\_s\_c} to a typed wrapper, use
 * \texttt{bkey\_s\_c\_to\_xattr(k)}. These accessors assert that the type
 * field matches, so always check \texttt{k.k->type} first:
 *
 * \begin{verbatim}
 * struct bkey_s_c k = bch2_btree_iter_peek(&iter);
 *
 * switch (k.k->type) {
 * case KEY_TYPE_xattr: {
 *     struct bkey_s_c_xattr xattr = bkey_s_c_to_xattr(k);
 *     // access xattr.v->x_name, etc.
 *     break;
 * }
 * }
 * \end{verbatim}
 *
 * See \S\ref{bkey-type-list} for the complete list of key types.
 */

/*
 * bkey_i	- bkey with inline value
 * bkey_s	- bkey with split value
 * bkey_s_c	- bkey with split value, const
 */

/* bkey with split value, const */
#[repr(C)]
#[derive(Clone, Copy, MaybeZeroable)]
pub struct bkey_s_c {
    pub k: *const cs::bkey,
    pub v: *const cs::bch_val,
}
c_default!(bkey_s_c);

/* bkey with split value */
nest! {
    #[repr(C)]
    #[derive(MaybeZeroable)]
    pub struct bkey_s {
        #>[repr(C)]
        #>[derive(Clone, Copy, MaybeZeroable)]
        pub split: pub union bkey_s_split {
            #>[repr(C)]
            #>[derive(Clone, Copy, MaybeZeroable)]
            pub kv: pub struct bkey_s_kv {
                pub k: *mut cs::bkey,
                pub v: *mut cs::bch_val,
            },
            pub s_c: cs::bkey_s_c,
        },
    }
}
c_default!(bkey_s);

macro_rules! __bkey_typed {
    ($( ($name:ident, $nr:literal, $($ty:ident)::+, $flags:tt) ),* $(,)?) => { ::paste::paste! { $(
        pub type [<bkey_i_ $name>]   = BkeyI<$($ty)::+>;
        pub type [<bkey_s_c_ $name>] = BkeySC<'static, $($ty)::+>;
        pub type [<bkey_s_ $name>]   = BkeyS<'static, $($ty)::+>;
    )* } };
}
BkeyType_arms!(__bkey_typed);

#[bch_enum(flags)]
#[prefix(BCH_VALIDATE_)]
#[repr(u32)]
pub enum ValidateFlags {
    write = 1 << 0,
    commit = 1 << 1,
    silent = 1 << 2,
}

#[bch_enum(closed)]
#[prefix(BKEY_VALIDATE_)]
#[repr(u32)]
pub enum BkeyValidateFrom {
    unknown,
    superblock,
    journal,
    btree_root,
    btree_node,
    commit,
}

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, MaybeZeroable, BitfieldMembers)]
    pub struct bkey_validate_context {
        #[members]
        #>[bitfield(u16)]
        pub bits: pub struct bkey_validate_context_bits {
            #[bits(8)] pub from: cs::BkeyValidateFrom,
            #[bits(8, from = ValidateFlags::from_bits_retain)] pub flags: ValidateFlags,
        },
        pub level: u8,
        pub btree: Btree,
        #[members]
        #>[bitfield(u8)]
        pub root_bits: pub struct bkey_validate_context_root_bits {
            root:  bool,
            __pad: u7,
        },
        pub journal_offset: c_uint,
        pub journal_seq: u64,
    }
}
