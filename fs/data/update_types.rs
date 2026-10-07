// SPDX-License-Identifier: GPL-2.0

//! The data types of data/update_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{CStruct, bitfield, c_enum, c_extern, c_verbatim, c_xmacro};

c_verbatim!(r#"
struct moving_context;
"#);

c_xmacro! {
    BCH_DATA_UPDATE_TYPES(x) {
        (other),
        (copygc),
        (reconcile),
        (promote),
        (self_heal),
        (scrub),
        (scrub_no_repair),
    }
}

macro_rules! __bch_data_update_types_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum bch_data_update_types: u32 {
                $($acc)*
                $([<BCH_DATA_UPDATE_ $n>],)*
            }
        }
    } };
}
BCH_DATA_UPDATE_TYPES!(__bch_data_update_types_0 []);

#[bitfield(u8)]
pub struct data_update_opts_no_devs_have_bits {
    #[bits(1)]
    pub no_devs_have: bool,
    #[bits(1)]
    pub checksum_paranoia: bool,
    #[bits(6)]
    pub __pad: u8,
}

/*
 * @target: where the new copy goes. Movers rewriting existing data want the
 * extent's background_target here: unset doesn't mean "no preference", it means
 * "anywhere", which drags data off its tier.
 *
 * It also decides the disk label of any erasure coded stripe the write creates
 * (bch2_ec_stripe_head_get()), and that label is stamped into the stripe on
 * disk permanently, where 0 means every device in the filesystem - so a mover
 * that leaves this unset doesn't just misplace one extent, it creates a stripe
 * that will keep widening onto devices the data was never supposed to touch.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct data_update_opts {
    pub type_: c::bch_data_update_types,
    pub ptrs_io_error: u8,
    pub ptrs_kill: u8,
    pub ptrs_kill_ec: u8,
    pub extra_replicas: u8,
    pub target: u16,
    #[c_bitfield]
    pub no_devs_have_bits: data_update_opts_no_devs_have_bits,

    pub read_dev: core::ffi::c_uint,
    pub read_flags: c::bch_read_flags,
    pub write_flags: c::bch_write_flags,
    pub commit_flags: c::bch_trans_commit_flags,
}
c_default!(data_update_opts);
impl data_update_opts {
    pub fn no_devs_have(&self) -> bool { let b = self.no_devs_have_bits; b.no_devs_have() }
    pub fn set_no_devs_have(&mut self, v: bool) { let mut b = self.no_devs_have_bits; b.set_no_devs_have(v); self.no_devs_have_bits = b; }
    pub fn checksum_paranoia(&self) -> bool { let b = self.no_devs_have_bits; b.checksum_paranoia() }
    pub fn set_checksum_paranoia(&mut self, v: bool) { let mut b = self.no_devs_have_bits; b.set_checksum_paranoia(v); self.no_devs_have_bits = b; }
}

#[repr(C)]
#[derive(CStruct)]
pub struct data_update {
    pub rcu: c::rcu_head,
    /* extent being updated: */
    pub btree_id: c::btree_id,
    pub k: c::bkey_buf,
    pub opts: c::data_update_opts,

    pub on_hashtable: bool,
    pub read_done: bool,
    /*
     * cas[i] is the bch_dev * for which we hold a ref (taken in
     * bkey_get_dev_refs), parallel to the ptrs in @k.  Stashed so the
     * exit path doesn't have to re-derive ca via c->devs[idx], which
     * dev_remove may have cleared while our ref still pins the dev.
     * NULL = no ref held for that ptr position; also serves as the
     * "we locked this bucket" indicator for nocow lock/unlock.
     */
    pub cas: [*mut c::bch_dev; c::BCH_BKEY_PTRS_MAX as usize],

    pub hash: c::rhlist_head,
    pub pos: c::bbpos,

    /* associated with @ctxt */
    pub read_list: c::list_head,
    pub io_list: c::list_head,
    pub io_seq: u64,
    pub b: *mut c::move_bucket,
    pub ctxt: *mut c::moving_context,
    pub stats: *mut c::bch_move_stats,

    pub rbio: c::bch_read_bio,
    pub op: c::bch_write_op,
    pub bvecs: *mut c::bio_vec,
}
c_default!(data_update);

#[repr(C)]
#[derive(CStruct)]
pub struct promote_op {
    pub start_time: u64,
    /* C's CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS: see types.rs */
    #[cfg(all(__KERNEL__, CONFIG_DEBUG_FS))]
    pub list_idx: core::ffi::c_uint,
    pub cpu: core::ffi::c_int, /* for promote_limit */

    pub work: c::work_struct,
    pub write: c::data_update,
    pub bi_inline_vecs: [c::bio_vec; 0], /* must be last */
}
c_default!(promote_op);

// What Rust calls of data/update.h: C gets these as prototypes, in data/update_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_data_update_init(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: *mut c::moving_context, arg4: *mut c::data_update, arg5: c::write_point_specifier, arg6: *mut c::bch_inode_opts, arg7: c::data_update_opts, arg8: c::btree_id, arg9: c::bkey_s_c) -> core::ffi::c_int;
}
