// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::cstructs::c::BCH_INODE_FIELDS_v3;
use crate::types::{c_default, Le64};
use crate::btree::bkey::BkeyI;
use crate::util::projections::Volatile;
use core::mem::{align_of, offset_of, size_of};
use cstruct_macros::{CStruct, c_bitmask, c_typedef, c_verbatim, TypeInfo};

c_typedef! {
    pub type u96 = u64;
}

macro_rules! __bch_inode_unpacked_0 {
    ([$(pub $f:ident: $ft:ty),* $(,)?] $(($_name:tt, $_bits:tt)),* $(,)?) => { ::paste::paste! {
        #[repr(C)]
        #[derive(Clone, Copy, Debug, Default, CStruct, TypeInfo)]
        pub struct bch_inode_unpacked {
            $(pub $f: $ft,)*
            $(pub $_name: [<u $_bits>],)*
        }

        /// A bch_inode_unpacked shared - a VFS inode's ei_inode: written by
        /// one writer at a time, read without a lock, field by field - and
        /// laid out as it is, so C has it as one.
        #[repr(C)]
        #[derive(TypeInfo)]
        pub struct VolatileInodeUnpacked {
            $(pub $f: Volatile<$ft>,)*
            $(pub $_name: Volatile<[<u $_bits>]>,)*
        }

        impl VolatileInodeUnpacked {
            /// Each field as it is: not all of them as of one moment, if
            /// it's written meanwhile.
            pub fn get(&self) -> bch_inode_unpacked {
                bch_inode_unpacked {
                    $($f: self.$f.get(),)*
                    $($_name: self.$_name.get(),)*
                }
            }

            /// By its one writer.
            pub fn set(&self, v: &bch_inode_unpacked) {
                $(self.$f.set(v.$f);)*
                $(self.$_name.set(v.$_name);)*
            }
        }

        const _: () = {
            assert!(size_of::<VolatileInodeUnpacked>() == size_of::<bch_inode_unpacked>());
            assert!(align_of::<VolatileInodeUnpacked>() == align_of::<bch_inode_unpacked>());
            $(assert!(offset_of!(VolatileInodeUnpacked, $f) == offset_of!(bch_inode_unpacked, $f));)*
            $(assert!(offset_of!(VolatileInodeUnpacked, $_name) == offset_of!(bch_inode_unpacked, $_name));)*
        };
    } };
}
BCH_INODE_FIELDS_v3!(__bch_inode_unpacked_0 [
    pub bi_inum: u64,
    pub bi_snapshot: u32,
    pub bi_journal_seq: u64,
    pub bi_hash_seed: Le64,
    pub bi_size: u64,
    pub bi_sectors: u64,
    pub bi_version: u64,
    pub bi_flags: u32,
    pub bi_mode: u16,
]);

c_bitmask! {
    BITMASK(struct bch_inode_unpacked, bi_flags) {
        INODE_STR_HASH(20, 24),
    }
}

c_verbatim!(r#"
#include "fs/inode_opts.h"
"#);

macro_rules! __BKEY_INODE_BUF_XSUM_0 {
    ($(($_name:tt, $_bits:tt)),* $(,)?) => {
        #[allow(non_upper_case_globals)]
        const BKEY_INODE_BUF_XSUM_0: usize = 0 + (0 $(+ 8 + (($_bits) as usize) / 8)*);
    };
}
BCH_INODE_FIELDS_v3!(__BKEY_INODE_BUF_XSUM_0);

#[repr(C)]
#[derive(CStruct)]
pub struct bkey_inode_buf {
    /* C's typed key: a v3 inode's header, its fields in _pad */
    #[c("struct bkey_i_inode_v3 inode")]
    pub inode: BkeyI<c::InodeV3>,
    #[c("#define x(_name, _bits)		+ 8 + _bits / 8
u8 _pad[0 + BCH_INODE_FIELDS_v3()];
#undef x")]
    pub _pad: [u8; BKEY_INODE_BUF_XSUM_0],
}
c_default!(bkey_inode_buf);
