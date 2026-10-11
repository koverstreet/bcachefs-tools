// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::cstructs::c::BCH_INODE_FIELDS_v3;
use crate::types::c_default;
use crate::util::ffi::c_char;
use core::ffi::{c_int, c_uint};
use zerocopy::byteorder::little_endian as le;
use cstruct_macros::{CStruct, c_bitmask, c_extern, c_typedef, c_verbatim, rust_c_extern};
use crate::inode::{
    bch2_inode_alloc_cursor_to_text, bch2_inode_alloc_cursor_validate,
    bch2_inode_generation_to_text, bch2_inode_generation_validate,
    bch2_inode_to_text, bch2_inode_v2_validate, bch2_inode_v3_validate,
    bch2_inode_validate, bch2_trigger_inode,
};
use typeinfo_macros::TypeInfo;

c_typedef! {
    pub type u96 = u64;
}

macro_rules! __bch_inode_unpacked_0 {
    ([$($acc:tt)*] $(($_name:tt, $_bits:tt)),* $(,)?) => { ::paste::paste! {
        #[repr(C)]
        #[derive(Clone, Copy, Debug, Default, CStruct, TypeInfo)]
        pub struct bch_inode_unpacked {
            $($acc)*
            $(pub $_name: [<u $_bits>],)*
        }
    } };
}
BCH_INODE_FIELDS_v3!(__bch_inode_unpacked_0 [
    pub bi_inum: u64,
    pub bi_snapshot: u32,
    pub bi_journal_seq: u64,
    pub bi_hash_seed: le::U64,
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
    pub inode: c::bkey_i_inode_v3,
    #[c("#define x(_name, _bits)		+ 8 + _bits / 8
u8 _pad[0 + BCH_INODE_FIELDS_v3()];
#undef x")]
    pub _pad: [u8; BKEY_INODE_BUF_XSUM_0],
}
c_default!(bkey_inode_buf);

// bkey_ops' methods, Rust's: defined with C's signatures - see rust_c_extern!.
rust_c_extern! {
    pub fn bch2_inode_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_inode_v2_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_inode_v3_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_inode_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
    pub fn bch2_trigger_inode(arg1: *mut c::btree_trans, arg2: c::btree_trigger_op) -> c_int;
    pub fn bch2_inode_generation_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_inode_generation_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
    pub fn bch2_inode_alloc_cursor_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_inode_alloc_cursor_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
}

c_extern! {
    pub fn bch2_inode_pack(arg1: *mut c::bch_fs, arg2: *mut c::bkey_inode_buf, arg3: *const c::bch_inode_unpacked);
    pub fn bch2_inode_unpack(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *mut c::bch_inode_unpacked);
    pub fn bch2_inode_to_v3(arg1: *mut c::btree_trans, arg2: *mut c::bkey_i) -> *mut c::bkey_i;
    pub fn bch2_inode_unpacked_to_text(arg1: *mut c::printbuf, arg2: *const c::bch_inode_unpacked);
    pub fn __bch2_inode_peek(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: *mut c::bch_inode_unpacked, arg4: c::subvol_inum, arg5: c_uint, arg6: *const c_char) -> c_int;
    pub fn bch2_inode_find_by_inum_snapshot(arg1: *mut c::btree_trans, arg2: u64, arg3: u32, arg4: *mut c::bch_inode_unpacked, arg5: c_uint) -> c_int;
    pub fn bch2_inode_find_by_inum_snapshot2(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: u32, arg4: *mut c::bch_inode_unpacked, arg5: c_uint, arg6: *const c_char) -> c_int;
    pub fn __bch2_inode_find_by_inum_trans(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: *mut c::bch_inode_unpacked, arg4: *const c_char) -> c_int;
    pub fn bch2_inode_find_by_inum(arg1: *mut c::bch_fs, arg2: c::subvol_inum, arg3: *mut c::bch_inode_unpacked) -> c_int;
    pub fn bch2_inode_write_flags(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: *mut c::bch_inode_unpacked, arg4: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn __bch2_fsck_write_inode(arg1: *mut c::btree_trans, arg2: *mut c::bch_inode_unpacked) -> c_int;
    pub fn bch2_inode_init_early(arg1: *mut c::bch_fs, arg2: *mut c::bch_inode_unpacked);
    pub fn bch2_inode_init_late(arg1: *mut c::bch_fs, arg2: *mut c::bch_inode_unpacked, arg3: u64, arg4: c::uid_t, arg5: c::gid_t, arg6: c::umode_t, arg7: c::dev_t, arg8: *mut c::bch_inode_unpacked);
    pub fn bch2_inode_init(arg1: *mut c::bch_fs, arg2: *mut c::bch_inode_unpacked, arg3: c::uid_t, arg4: c::gid_t, arg5: c::umode_t, arg6: c::dev_t, arg7: *mut c::bch_inode_unpacked);
    pub fn bch2_fs_inode_shard_cpu_init(arg1: *mut c::bch_fs);
    pub fn bch2_shard_inode_numbers_bits_default(nr_cpus: c_uint, fs_size: u64, btree_node_bytes: u64) -> c_uint;
    pub fn bch2_inode_rm(arg1: *mut c::bch_fs, arg2: c::subvol_inum) -> c_int;
    pub fn bch2_inode_set_casefold(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: *mut c::bch_inode_unpacked, arg4: c_uint) -> c_int;
    pub fn bch2_delete_dead_inodes(arg1: *mut c::bch_fs) -> c_int;
    pub fn bch2_kill_i_generation_keys(arg1: *mut c::bch_fs) -> c_int;
}
