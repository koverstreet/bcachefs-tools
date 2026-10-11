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
    pub fn bch2_inode_validate(c: *mut c::bch_fs, k: c::bkey_s_c, from: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_inode_v2_validate(c: *mut c::bch_fs, k: c::bkey_s_c, from: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_inode_v3_validate(c: *mut c::bch_fs, k: c::bkey_s_c, from: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_inode_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, k: c::bkey_s_c);
    pub fn bch2_trigger_inode(trans: *mut c::btree_trans, op: c::btree_trigger_op) -> c_int;
    pub fn bch2_inode_generation_validate(c: *mut c::bch_fs, k: c::bkey_s_c, from: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_inode_generation_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, k: c::bkey_s_c);
    pub fn bch2_inode_alloc_cursor_validate(c: *mut c::bch_fs, k: c::bkey_s_c, from: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_inode_alloc_cursor_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, k: c::bkey_s_c);
}

c_extern! {
    pub fn bch2_inode_pack(c: *mut c::bch_fs, packed: *mut c::bkey_inode_buf, inode: *const c::bch_inode_unpacked);
    pub fn bch2_inode_unpack(c: *mut c::bch_fs, k: c::bkey_s_c, unpacked: *mut c::bch_inode_unpacked);
    pub fn bch2_inode_to_v3(trans: *mut c::btree_trans, k: *mut c::bkey_i) -> *mut c::bkey_i;
    pub fn bch2_inode_unpacked_to_text(out: *mut c::printbuf, inode: *const c::bch_inode_unpacked);
    pub fn __bch2_inode_peek(trans: *mut c::btree_trans, iter: *mut c::btree_iter, inode: *mut c::bch_inode_unpacked, inum: c::subvol_inum, flags: c_uint, warn: *const c_char) -> c_int;
    pub fn bch2_inode_find_by_inum_snapshot(trans: *mut c::btree_trans, inum: u64, snapshot: u32, inode: *mut c::bch_inode_unpacked, flags: c_uint) -> c_int;
    pub fn bch2_inode_find_by_inum_snapshot2(trans: *mut c::btree_trans, inum: c::subvol_inum, snapshot: u32, inode: *mut c::bch_inode_unpacked, flags: c_uint, warn: *const c_char) -> c_int;
    pub fn __bch2_inode_find_by_inum_trans(trans: *mut c::btree_trans, inum: c::subvol_inum, inode: *mut c::bch_inode_unpacked, warn: *const c_char) -> c_int;
    pub fn bch2_inode_find_by_inum(c: *mut c::bch_fs, inum: c::subvol_inum, inode: *mut c::bch_inode_unpacked) -> c_int;
    pub fn bch2_inode_write_flags(trans: *mut c::btree_trans, iter: *mut c::btree_iter, inode: *mut c::bch_inode_unpacked, flags: c::btree_iter_update_trigger_flags) -> c_int;
    pub fn __bch2_fsck_write_inode(trans: *mut c::btree_trans, inode: *mut c::bch_inode_unpacked) -> c_int;
    pub fn bch2_inode_init_early(c: *mut c::bch_fs, inode: *mut c::bch_inode_unpacked);
    pub fn bch2_inode_init_late(c: *mut c::bch_fs, inode: *mut c::bch_inode_unpacked, now: u64, uid: c::uid_t, gid: c::gid_t, mode: c::umode_t, rdev: c::dev_t, parent: *mut c::bch_inode_unpacked);
    pub fn bch2_inode_init(c: *mut c::bch_fs, inode: *mut c::bch_inode_unpacked, uid: c::uid_t, gid: c::gid_t, mode: c::umode_t, rdev: c::dev_t, parent: *mut c::bch_inode_unpacked);
    pub fn bch2_fs_inode_shard_cpu_init(c: *mut c::bch_fs);
    pub fn bch2_shard_inode_numbers_bits_default(nr_cpus: c_uint, fs_size: u64, btree_node_bytes: u64) -> c_uint;
    pub fn bch2_inode_rm(c: *mut c::bch_fs, inum: c::subvol_inum) -> c_int;
    pub fn bch2_inode_set_casefold(trans: *mut c::btree_trans, inum: c::subvol_inum, inode: *mut c::bch_inode_unpacked, v: c_uint) -> c_int;
    pub fn bch2_delete_dead_inodes(c: *mut c::bch_fs) -> c_int;
    pub fn bch2_kill_i_generation_keys(c: *mut c::bch_fs) -> c_int;
}
