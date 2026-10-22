// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{CStruct, bitfield, c_enum, c_xmacro};
use nestify::nest;

c_xmacro! {
    BCH_DATA_UPDATE_TYPES(x) {
        (other),
        (copygc),
        (reconcile),
        (rechecksum),
        (promote),
        (self_heal),
        (scrub),
        (scrub_no_repair),
    }
}

c_enum! {
    #[closed]
    pub enum bch_data_update_types: u32 {
        BCH_DATA_UPDATE_TYPES(n) => BCH_DATA_UPDATE_ ## n,
    }
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
nest! {
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
        #>[bitfield(u8)]
        pub bits: pub struct data_update_opts_bits {
            no_devs_have:      bool,
            checksum_paranoia: bool,
            __pad:             u6,
        },

        pub read_dev: c_uint,
        pub read_flags: c::bch_read_flags,
        pub write_flags: c::bch_write_flags,
        pub commit_flags: c::bch_trans_commit_flags,
    }
}
c_default!(data_update_opts);

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
    /* C's CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS, which bcachefs.h defines from these */
    #[cfg(all(__KERNEL__, CONFIG_DEBUG_FS))]
    pub list_idx: c_uint,
    pub cpu: c_int, /* for promote_limit */

    pub work: c::work_struct,
    pub write: c::data_update,
    pub bi_inline_vecs: [c::bio_vec; 0], /* must be last */
}
c_default!(promote_op);
