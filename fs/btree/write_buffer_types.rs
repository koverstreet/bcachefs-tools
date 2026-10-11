// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use core::ffi::c_int;
use cstruct_macros::{CStruct, c_const, c_enum, c_extern, c_xmacro};
use nestify::nest;
use typeinfo_macros::TypeInfo;

c_xmacro! {
    /*
     * Subset of BCH_BTREE_IDS() that are marked BTREE_IS_write_buffer, enumerated
     * densely so we can size per-btree write buffer state as a compact array. Must
     * be kept in sync with BCH_BTREE_IDS() — adding BTREE_IS_write_buffer to a new
     * btree requires a matching entry here.
     */
    BCH_WRITE_BUFFER_BTREES(x) {
        (accounting),
        (lru),
        (need_discard),
        (backpointers),
        (deleted_inodes),
        (reconcile_work),
        (reconcile_hipri),
        (reconcile_pending),
        (reconcile_work_phys),
        (reconcile_hipri_phys),
        (stripe_backpointers),
    }
}

c_enum! {
    #[closed]
    pub enum bch_wb_btree: u32 {
        BCH_WRITE_BUFFER_BTREES(name) => BCH_WB_BTREE_ ## name,
        BCH_WB_BTREE_NR,
    }
}

c_const! {
    #[c_int]
    pub const BTREE_WRITE_BUFERED_VAL_U64s_MAX: u32 = 4;
}

/*
 * Each bch_fs_btree_write_buffer is per-btree, so individual key entries don't
 * need to carry a btree id — it's implicit in the containing buffer.
 */
#[cfg(target_endian = "little")]
nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]
    pub struct wb_key_ref {
        #[c_anon]
        #>[repr(C)]
        pub key: pub union wb_key_ref_key {
            #[c_anon]
            #>[repr(C, packed)]
            pub pos_idx: pub struct wb_key_ref_pos_idx {
                pub idx: u32,
                #[c("u8 pos[sizeof(struct bpos)]")]
                pub pos: [u8; size_of::<c::bpos>()],
            },
            #[c_anon]
            #>[repr(C)]
            pub words: pub struct wb_key_ref_words {
                pub lo: u64,
                pub mi: u64,
                pub hi: u64,
            },
        },
    }
}

#[cfg(target_endian = "big")]
nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]
    pub struct wb_key_ref {
        #[c_anon]
        #>[repr(C)]
        pub key: pub union wb_key_ref_key {
            #[c_anon]
            #>[repr(C, packed)]
            pub pos_idx: pub struct wb_key_ref_pos_idx {
                #[c("u8 pos[sizeof(struct bpos)]")]
                pub pos: [u8; size_of::<c::bpos>()],
                pub idx: u32,
            },
            #[c_anon]
            #>[repr(C)]
            pub words: pub struct wb_key_ref_words {
                pub hi: u64,
                pub mi: u64,
                pub lo: u64,
            },
        },
    }
}
c_default!(wb_key_ref);

#[repr(C)]
#[derive(Default, CStruct)]
pub struct btree_write_buffered_key {
    pub journal_seq: u64,

    /* BTREE_WRITE_BUFERED_VAL_U64s_MAX only applies to accounting keys */
    pub k: c::bkey_i,
    pub k_pad: [u64; c::BTREE_WRITE_BUFERED_VAL_U64s_MAX as usize],
}

#[repr(C)]
#[derive(CStruct)]
pub struct btree_write_buffer_keys {
    pub keys: c::darray_u64,
    pub pin: c::journal_entry_pin,
    pub lock: c::mutex,
    /*
     * Back-references set at init so the journal-pin callback can recover
     * which btree (and which of inc/flushing) a firing pin belongs to.
     */
    pub wb_btree: c::bch_wb_btree,
    pub is_flushing: bool,
}
c_default!(btree_write_buffer_keys);

c_xmacro! {
    WB_FLUSH_CALLERS(x) {
        (thread),
        (journal_pin),
        (sync),
        (maybe),
        (tryflush),
    }
}

c_enum! {
    #[closed]
    pub enum wb_flush_caller: u32 {
        WB_FLUSH_CALLERS(n) => WB_FLUSH_ ## n,
        WB_FLUSH_NR,
    }
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree_write_buffer {
    /* Back-refs set at init so the flush thread can find c and its idx. */
    pub c: *mut c::bch_fs,
    pub idx: c::bch_wb_btree,

    pub sorted: DArray<c::wb_key_ref>,
    pub inc: c::btree_write_buffer_keys,
    pub flushing: c::btree_write_buffer_keys,

    pub flush_work: c::work_struct,
    /*
     * Set by the dispatcher (or auto-flush wakeup) before queue_work(),
     * read by the worker on each flush_locked call. Racy under concurrent
     * dispatchers — accepted; this only feeds the diagnostic
     * nr_flushes_caller[] histogram.
     */
    pub flush_work_caller: c::wb_flush_caller,

    pub nr_flushes: u64,
    pub nr_flushes_caller: [u64; c::WB_FLUSH_NR as usize],
    pub nr_keys_flushed: u64,
    pub nr_keys_fast: u64,
    pub nr_keys_slowpath: u64,
    pub nr_shards_total: u64,

    pub accounting: DArray<c::btree_write_buffered_key>,
}
c_default!(bch_fs_btree_write_buffer);

c_extern! {
    pub fn bch2_btree_write_buffer_flush_sync(arg1: *mut c::btree_trans) -> c_int;
}
