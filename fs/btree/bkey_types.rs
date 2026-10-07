// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/bkey_types.h. Rust's own key types, btree/bkey.rs's
//! BkeySC and BkeyS, are checked to have the layouts of C's bkey_s_c and
//! bkey_s, which convert below. The typed keys C generates for each key type
//! from BCH_BKEY_TYPES() - bkey_i_<type>, bkey_s_c_<type>, bkey_s_<type> -
//! stay C's; Rust's are BkeyI, BkeySC and BkeyS of the value type, checked
//! against them.

#![allow(non_camel_case_types)]

use crate::btree::bkey::{BkeyI, BkeyS, BkeySC};
use crate::cstructs::c as cs;
use crate::cstructs::c::BCH_BKEY_TYPES;
use cstruct_macros::c_same;

c_same! { struct bkey_s_c == BkeySC<'static> { k, v } }
c_same! { struct bkey_s   == BkeyS<'static> { k, v } }

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
// ---- the data types of btree/bkey_types.h, which is generated from this file: see
// fs/types/lib.rs. Translated from the C by c2rs.

use crate::types::c_default;
use cstruct_macros::{CStruct, bitfield, c_enum, c_extern, c_verbatim, c_xmacro};
use nestify::nest;

c_verbatim!(r#"
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

#define bkey_p_next(_k)		vstruct_next(_k)

#define bkey_val_u64s(_k)	((_k)->u64s - BKEY_U64s)

#define bkey_val_end(_k)	((void *) (((u64 *) (_k).v) + bkey_val_u64s((_k).k)))

/*
 * Was this key written with @field?
 *
 * Values grow: a key written by an older version stops short of the fields
 * added since, and the length is the only record of which ones it had. Reading
 * such a field off a raw bkey reads the next key in the bset, and reading it
 * off a padded copy reads 0 - which is indistinguishable from a stored 0.
 */
#define bkey_has_field(_k, _type, _field)				\
	(bkey_val_bytes(_k) >= offsetof(struct bch_##_type, _field) +	\
			       sizeof(((struct bch_##_type *) NULL)->_field))

#define bkey_deleted(_k)	((_k)->type == KEY_TYPE_deleted)

#define bkey_whiteout(_k)				\
	((_k)->type == KEY_TYPE_deleted || (_k)->type == KEY_TYPE_whiteout)

#define bkey_extent_whiteout(_k)				\
	((_k)->type == KEY_TYPE_deleted ||			\
	 (_k)->type == KEY_TYPE_whiteout ||			\
	 (_k)->type == KEY_TYPE_extent_whiteout)
"#);

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

c_verbatim!(r#"
#define bkey_s_null		((struct bkey_s)   { .k = NULL })
#define bkey_s_c_null		((struct bkey_s_c) { .k = NULL })

#define bkey_s_err(err)		((struct bkey_s)   { .k = ERR_PTR(err) })
#define bkey_s_c_err(err)	((struct bkey_s_c) { .k = ERR_PTR(err) })

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

macro_rules! __bkey_validate_from_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum bkey_validate_from: u32 {
                $($acc)*
                $([<BKEY_VALIDATE_ $n>],)*
            }
        }
    } };
}
BKEY_VALIDATE_CONTEXTS!(__bkey_validate_from_0 []);

#[bitfield(u16, repr = zerocopy::byteorder::native_endian::U16, from = zerocopy::byteorder::native_endian::U16::new, into = zerocopy::byteorder::native_endian::U16::get)]
pub struct bkey_validate_context_from_bits {
    #[bits(8)]
    pub from: u32,
    #[bits(8)]
    pub flags: u32,
}

#[bitfield(u8)]
pub struct bkey_validate_context_root_bits {
    #[bits(1)]
    pub root: bool,
    #[bits(7)]
    pub __pad: u8,
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct bkey_validate_context {
    #[c_bitfield]
    pub from_bits: bkey_validate_context_from_bits,
    pub level: u8,
    pub btree: cs::btree_id,
    #[c_bitfield]
    pub root_bits: bkey_validate_context_root_bits,
    pub journal_offset: core::ffi::c_uint,
    pub journal_seq: u64,
}
c_default!(bkey_validate_context);
impl bkey_validate_context {
    pub fn from(&self) -> u32 { let b = self.from_bits; b.from() }
    pub fn set_from(&mut self, v: u32) { let mut b = self.from_bits; b.set_from(v); self.from_bits = b; }
    pub fn flags(&self) -> u32 { let b = self.from_bits; b.flags() }
    pub fn set_flags(&mut self, v: u32) { let mut b = self.from_bits; b.set_flags(v); self.from_bits = b; }
}
impl bkey_validate_context {
    pub fn root(&self) -> bool { let b = self.root_bits; b.root() }
    pub fn set_root(&mut self, v: bool) { let mut b = self.root_bits; b.set_root(v); self.root_bits = b; }
}

// What Rust calls of btree/bkey.h: C gets these as prototypes, in btree/bkey_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_bpos_swab(arg1: *mut cs::bpos);
    pub fn __bch2_bkey_unpack_key(arg1: *const cs::bkey_format, arg2: *mut cs::bkey, arg3: *const cs::bkey_packed);
}
