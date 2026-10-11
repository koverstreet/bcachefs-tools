// SPDX-License-Identifier: GPL-2.0

//! Rust's own key types, btree/bkey.rs's BkeySC and BkeyS, are checked to
//! have the layouts of C's bkey_s_c and bkey_s. The typed keys C generates
//! for each key type from BCH_BKEY_TYPES() - bkey_i_<type>, bkey_s_c_<type>,
//! bkey_s_<type> - stay C's; Rust's are BkeyI, BkeySC and BkeyS of the value
//! type, checked against them.

#![allow(non_camel_case_types)]

use crate::btree::bkey::{BkeyI, BkeyS, BkeySC};
use crate::cstructs::c as cs;
use crate::cstructs::c::BCH_BKEY_TYPES;
use crate::types::c_default;
use core::ffi::c_uint;
use cstruct_macros::{CStruct, bitfield, c_enum, c_extern, c_same, c_verbatim, c_xmacro};
use nestify::nest;

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
#[derive(Clone, Copy, CStruct)]
pub struct bkey_s_c {
    pub k: *const cs::bkey,
    pub v: *const cs::bch_val,
}
c_default!(bkey_s_c);

/* bkey with split value */
nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct bkey_s {
        #[c_anon]
        #>[repr(C)]
        #>[derive(Clone, Copy, CStruct)]
        pub split: pub union bkey_s_split {
            #[c_anon]
            #>[repr(C)]
            #>[derive(Clone, Copy, CStruct)]
            pub kv: pub struct bkey_s_kv {
                pub k: *mut cs::bkey,
                pub v: *mut cs::bch_val,
            },
            pub s_c: cs::bkey_s_c,
        },
    }
}
c_default!(bkey_s);

c_same! { struct bkey_s_c == BkeySC<'static> { k, v } }
c_same! { struct bkey_s   == BkeyS<'static> { k, v } }

/*
 * C's typed keys: bkey_gen.h defines them, for its checks of Rust's - the
 * c_same!s after them.
 */
c_verbatim!(r#"
/*
 * For a given type of value (e.g. struct bch_extent), generates the types for
 * bkey + bch_extent - inline, split, split const. The conversion functions,
 * which also check that the value is of the correct type, are in
 * bkey_types_inline.h.
 *
 * We use anonymous unions for upcasting - e.g. converting from e.g. a
 * bkey_i_extent to a bkey_i - since that's always safe, instead of conversion
 * functions.
 */
#define x(name, ...)					\
struct bkey_i_##name {							\
	union {								\
		struct bkey		k;				\
		struct bkey_i		k_i;				\
	};								\
	struct bch_##name		v;				\
};									\
									\
struct bkey_s_c_##name {						\
	union {								\
	struct {							\
		const struct bkey	*k;				\
		const struct bch_##name	*v;				\
	};								\
	struct bkey_s_c			s_c;				\
	};								\
};									\
									\
struct bkey_s_##name {							\
	union {								\
	struct {							\
		struct bkey		*k;				\
		struct bch_##name	*v;				\
	};								\
	struct bkey_s_c_##name		c;				\
	struct bkey_s			s;				\
	struct bkey_s_c			s_c;				\
	};								\
};

BCH_BKEY_TYPES();
#undef x
"#);

macro_rules! __bkey_typed {
    ($( ($name:tt $(, $($rest:tt)*)?) ),* $(,)?) => { ::paste::paste! { $(
        pub type [<bkey_i_ $name>]   = BkeyI<cs::[<bch_ $name>]>;
        pub type [<bkey_s_c_ $name>] = BkeySC<'static, cs::[<bch_ $name>]>;
        pub type [<bkey_s_ $name>]   = BkeyS<'static, cs::[<bch_ $name>]>;

        c_same! { struct [<bkey_i_ $name>]   == BkeyI<cs::[<bch_ $name>]> { k, v } }
        c_same! { struct [<bkey_s_c_ $name>] == BkeySC<'static, cs::[<bch_ $name>]> { k, v } }
        c_same! { struct [<bkey_s_ $name>]   == BkeyS<'static, cs::[<bch_ $name>]> { k, v } }
    )* } };
}
BCH_BKEY_TYPES!(__bkey_typed);

c_enum! {
    #[flags]
    pub enum bch_validate_flags: u32 {
        BCH_VALIDATE_write = 1 << 0,
        BCH_VALIDATE_commit = 1 << 1,
        BCH_VALIDATE_silent = 1 << 2,
    }
}

c_xmacro! {
    BKEY_VALIDATE_CONTEXTS(x) {
        (unknown),
        (superblock),
        (journal),
        (btree_root),
        (btree_node),
        (commit),
    }
}

c_enum! {
    #[closed]
    pub enum bkey_validate_from: u32 {
        BKEY_VALIDATE_CONTEXTS(n) => BKEY_VALIDATE_ ## n,
    }
}

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, CStruct)]
    pub struct bkey_validate_context {
        #[c_bitfield]
        #>[bitfield(u16)]
        pub bits: pub struct bkey_validate_context_bits {
            #[bits(8)]
            pub from: u32,
            #[bits(8, from = cs::bch_validate_flags::from_bits_retain)]
            pub flags: cs::bch_validate_flags,
        },
        pub level: u8,
        pub btree: cs::btree_id,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub root_bits: pub struct bkey_validate_context_root_bits {
            #[bits(1)]
            pub root: bool,
            #[bits(7)]
            pub __pad: u8,
        },
        pub journal_offset: c_uint,
        pub journal_seq: u64,
    }
}
c_default!(bkey_validate_context);

c_extern! {
    pub fn bch2_bpos_swab(arg1: *mut cs::bpos);
    pub fn __bch2_bkey_unpack_key(arg1: *const cs::bkey_format, arg2: *mut cs::bkey, arg3: *const cs::bkey_packed);
}
