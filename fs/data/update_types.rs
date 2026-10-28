// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::c_uint;
use core::sync::atomic::AtomicBool;
use cstruct_macros::{CStruct, bitfield, c_enum, c_xmacro};
use nestify::nest;

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
        pub ptrs_io_error: u16,
        pub ptrs_kill: u16,
        pub ptrs_kill_ec: u16,
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

/// An update's key in the in-flight table: where it is, and whether it's
/// copygc's - so one of each can be in flight at a position.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, CStruct)]
pub struct data_update_key {
    pub btree: c::btree_id,
    pub pos: c::bpos,
    pub copygc: bool,
}

#[repr(C)]
#[derive(CStruct)]
pub struct data_update {
    pub rcu: c::rcu_head,
    /* extent being updated: */
    pub btree_id: c::btree_id,
    pub k: c::bkey_buf,
    pub opts: c::data_update_opts,

    /* read by the in-flight listing, racily */
    #[c("bool read_done")]
    pub read_done: AtomicBool,
    /* its key in c->update_table, while it's in: Held's in_flight */
    pub in_flight_key: c::data_update_key,

    /* associated with @ctxt: its index in the context's updates */
    pub io_seq: u64,
    pub ctxt: *mut c::moving_context,
    /* the mover's, if it keeps any - which outlive its updates */
    #[c("struct bch_move_stats *stats")]
    pub stats: Option<crate::util::backref::BackRef<c::MoveStats>>,

    pub rbio: c::bch_read_bio,
    pub op: c::bch_write_op,
    pub bvecs: *mut c::bio_vec,
}

