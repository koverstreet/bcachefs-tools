// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::{c_int, c_uint};
use crate::btree::bkey::BkeySC;
use crate::btree::iter::TransAttempt;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::util::{Printbuf, ToText};
use cstruct_macros::{CStruct, bitfield, c_enum, c_extern, c_xmacro};
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
    /* the mover's, if it keeps any - which outlive its updates */
    #[c("struct bch_move_stats *stats")]
    pub stats: Option<&'static c::MoveStats>,

    pub rbio: c::bch_read_bio,
    pub op: c::bch_write_op,
    pub bvecs: *mut c::bio_vec,
}

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

c_extern! {
    pub fn bch2_data_update_to_text(out: *mut c::printbuf, m: *mut c::data_update);
    pub fn bch2_data_update_opts_to_text(out: *mut c::printbuf, c: *mut c::bch_fs,
                                         io_opts: *mut c::bch_inode_opts, data_opts: *mut c::data_update_opts);
    pub fn bch2_data_update_in_flight(c: *mut c::bch_fs, pos: *mut c::bbpos,
                                      type_: c::bch_data_update_types) -> bool;
    pub fn bch2_can_do_data_update(trans: *mut c::btree_trans, opts: *mut c::bch_inode_opts,
                                   data_opts: *mut c::data_update_opts, k: c::bkey_s_c,
                                   trace: *mut c::printbuf) -> c_int;
}

impl ToText for c::data_update {
    fn to_text(&self, out: &mut Printbuf) {
        // SAFETY: read only
        unsafe { bch2_data_update_to_text(out.as_raw(), core::ptr::from_ref(self).cast_mut()) }
    }
}

impl Fs {
    /// Whether a data update at @pos keeps out one of @type: any update there,
    /// but copygc's only by copygc's - a promote mustn't hold up an evacuation.
    pub fn data_update_in_flight(&self, pos: c::bbpos, type_: c::bch_data_update_types) -> bool {
        // SAFETY: @pos only read, under its own RCU lock
        unsafe { bch2_data_update_in_flight(self.as_ptr(), core::ptr::from_ref(&pos).cast_mut(), type_) }
    }

    pub fn data_update_opts_to_text(&self, out: &mut Printbuf, io_opts: &c::bch_inode_opts,
                                    data_opts: &c::data_update_opts) {
        // SAFETY: read only
        unsafe {
            bch2_data_update_opts_to_text(out.as_raw(), self.as_ptr(), core::ptr::from_ref(io_opts).cast_mut(),
                                          core::ptr::from_ref(data_opts).cast_mut())
        }
    }
}

impl TransAttempt<'_, '_> {
    /// Whether the update @data_opts says can be done to @k now: the
    /// durability it would keep, devices to write it to, and for must_ec a
    /// stripe head. If not, why - a data_update_fail_* or ec_alloc_failed
    /// error, and into @trace. Getting the stripe head takes mutexes, which
    /// can restart the attempt.
    pub fn can_do_data_update(&self, opts: &c::bch_inode_opts, data_opts: &c::data_update_opts,
                              k: BkeySC<'_>, trace: Option<&mut Printbuf>) -> Result<(), BchError> {
        let trace = trace.map_or(core::ptr::null_mut(), |t| core::ptr::from_mut(t.as_raw()));
        // SAFETY: in the attempt; @opts and @data_opts only read, @trace NULL
        // or ours
        ret_to_result_void(unsafe {
            bch2_can_do_data_update(self.raw(), core::ptr::from_ref(opts).cast_mut(),
                                    core::ptr::from_ref(data_opts).cast_mut(), k.into(), trace)
        })
    }
}
