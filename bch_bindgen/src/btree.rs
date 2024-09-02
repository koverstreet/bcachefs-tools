use crate::bkey::BkeySC;
use crate::c;
use crate::errcode::{bch_errcode, errptr_to_result_c};
use crate::fs::Fs;
use crate::printbuf_to_formatter;
use crate::SPOS_MAX;
use bitflags::bitflags;
use std::fmt;
use std::marker::PhantomData;
use std::mem::MaybeUninit;

pub struct BtreeTrans<'f> {
    raw: *mut c::btree_trans,
    fs:  PhantomData<&'f Fs>,
}

impl<'f> BtreeTrans<'f> {
    pub fn new(fs: &'f Fs) -> BtreeTrans<'f> {
        unsafe {
            BtreeTrans {
                raw: &mut *c::__bch2_trans_get(fs.raw, 0),
                fs:  PhantomData,
            }
        }
    }
}

impl<'f> Drop for BtreeTrans<'f> {
    fn drop(&mut self) {
        unsafe { c::bch2_trans_put(&mut *self.raw) }
    }
}

bitflags! {
    pub struct BtreeIterFlags: u16 {
        const SLOTS = c::btree_iter_update_trigger_flags::BTREE_ITER_slots as u16;
        const INTENT = c::btree_iter_update_trigger_flags::BTREE_ITER_intent as u16;
        const PREFETCH = c::btree_iter_update_trigger_flags::BTREE_ITER_prefetch as u16;
        const IS_EXTENTS = c::btree_iter_update_trigger_flags::BTREE_ITER_is_extents as u16;
        const NOT_EXTENTS = c::btree_iter_update_trigger_flags::BTREE_ITER_not_extents as u16;
        const CACHED = c::btree_iter_update_trigger_flags::BTREE_ITER_cached as u16;
        const KEY_CACHED = c::btree_iter_update_trigger_flags::BTREE_ITER_with_key_cache as u16;
        const WITH_UPDATES = c::btree_iter_update_trigger_flags::BTREE_ITER_with_updates as u16;
        const WITH_JOURNAL = c::btree_iter_update_trigger_flags::BTREE_ITER_with_journal as u16;
        const SNAPSHOT_FIELD = c::btree_iter_update_trigger_flags::BTREE_ITER_snapshot_field as u16;
        const ALL_SNAPSHOTS = c::btree_iter_update_trigger_flags::BTREE_ITER_all_snapshots as u16;
        const FILTER_SNAPSHOTS = c::btree_iter_update_trigger_flags::BTREE_ITER_filter_snapshots as u16;
        const NOPRESERVE = c::btree_iter_update_trigger_flags::BTREE_ITER_nopreserve as u16;
        const CACHED_NOFILL = c::btree_iter_update_trigger_flags::BTREE_ITER_cached_nofill as u16;
        const KEY_CACHE_FILL = c::btree_iter_update_trigger_flags::BTREE_ITER_key_cache_fill as u16;
    }
}

pub struct BtreeIter<'t> {
    raw:   c::btree_iter,
    trans: PhantomData<&'t BtreeTrans<'t>>,
}

impl<'t> BtreeIter<'t> {
    pub fn new(
        trans: &'t BtreeTrans<'t>,
        btree: c::btree_id,
        pos: c::bpos,
        flags: BtreeIterFlags,
    ) -> BtreeIter<'t> {
        unsafe {
            let mut iter: MaybeUninit<c::btree_iter> = MaybeUninit::uninit();

            c::bch2_trans_iter_init_outlined(
                trans.raw,
                iter.as_mut_ptr(),
                btree,
                pos,
                flags.bits().into(),
            );

            BtreeIter {
                raw:   iter.assume_init(),
                trans: PhantomData,
            }
        }
    }

    pub fn peek_upto<'i>(&'i mut self, end: c::bpos) -> Result<Option<BkeySC<'i>>, bch_errcode> {
        unsafe {
            let k = c::bch2_btree_iter_peek_upto(&mut self.raw, end);
            errptr_to_result_c(k.k).map(|_| {
                if !k.k.is_null() {
                    Some(BkeySC {
                        k:    &*k.k,
                        v:    &*k.v,
                        iter: PhantomData,
                    })
                } else {
                    None
                }
            })
        }
    }

    pub fn peek(&mut self) -> Result<Option<BkeySC>, bch_errcode> {
        self.peek_upto(SPOS_MAX)
    }

    pub fn peek_and_restart(&mut self) -> Result<Option<BkeySC>, bch_errcode> {
        unsafe {
            let k = c::bch2_btree_iter_peek_and_restart_outlined(&mut self.raw);

            errptr_to_result_c(k.k).map(|_| {
                if !k.k.is_null() {
                    Some(BkeySC {
                        k:    &*k.k,
                        v:    &*k.v,
                        iter: PhantomData,
                    })
                } else {
                    None
                }
            })
        }
    }

    pub fn advance(&mut self) {
        unsafe {
            c::bch2_btree_iter_advance(&mut self.raw);
        }
    }
}

impl<'t> Drop for BtreeIter<'t> {
    fn drop(&mut self) {
        unsafe { c::bch2_trans_iter_exit(self.raw.trans, &mut self.raw) }
    }
}

pub struct BtreeNodeIter<'t> {
    raw:   c::btree_iter,
    trans: PhantomData<&'t BtreeTrans<'t>>,
}

impl<'t> BtreeNodeIter<'t> {
    pub fn new(
        trans: &'t BtreeTrans<'t>,
        btree: c::btree_id,
        pos: c::bpos,
        locks_want: u32,
        depth: u32,
        flags: BtreeIterFlags,
    ) -> BtreeNodeIter<'t> {
        unsafe {
            let mut iter: MaybeUninit<c::btree_iter> = MaybeUninit::uninit();
            c::bch2_trans_node_iter_init(
                trans.raw,
                iter.as_mut_ptr(),
                btree,
                pos,
                locks_want,
                depth,
                flags.bits().into(),
            );

            BtreeNodeIter {
                raw:   iter.assume_init(),
                trans: PhantomData,
            }
        }
    }

    pub fn peek<'i>(&'i mut self) -> Result<Option<&'i c::btree>, bch_errcode> {
        unsafe {
            let b = c::bch2_btree_iter_peek_node(&mut self.raw);
            errptr_to_result_c(b).map(|b| if !b.is_null() { Some(&*b) } else { None })
        }
    }

    pub fn peek_and_restart<'i>(&'i mut self) -> Result<Option<&'i c::btree>, bch_errcode> {
        unsafe {
            let b = c::bch2_btree_iter_peek_node_and_restart(&mut self.raw);
            errptr_to_result_c(b).map(|b| if !b.is_null() { Some(&*b) } else { None })
        }
    }

    pub fn advance<'i>(&'i mut self) {
        unsafe {
            c::bch2_btree_iter_next_node(&mut self.raw);
        }
    }

    pub fn next<'i>(&'i mut self) -> Result<Option<&'i c::btree>, bch_errcode> {
        unsafe {
            let b = c::bch2_btree_iter_next_node(&mut self.raw);
            errptr_to_result_c(b).map(|b| if !b.is_null() { Some(&*b) } else { None })
        }
    }
}

impl<'t> Drop for BtreeNodeIter<'t> {
    fn drop(&mut self) {
        unsafe { c::bch2_trans_iter_exit(self.raw.trans, &mut self.raw) }
    }
}

impl<'b, 'f> c::btree {
    pub fn to_text(&'b self, fs: &'f Fs) -> BtreeNodeToText<'b, 'f> {
        BtreeNodeToText { b: &self, fs }
    }

    pub fn ondisk_to_text(&'b self, fs: &'f Fs) -> BtreeNodeOndiskToText<'b, 'f> {
        BtreeNodeOndiskToText { b: &self, fs }
    }
}

pub struct BtreeNodeToText<'b, 'f> {
    b:  &'b c::btree,
    fs: &'f Fs,
}

impl<'b, 'f> fmt::Display for BtreeNodeToText<'b, 'f> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        printbuf_to_formatter(f, |buf| unsafe {
            c::bch2_btree_node_to_text(buf, self.fs.raw, self.b)
        })
    }
}

pub struct BtreeNodeOndiskToText<'b, 'f> {
    b:  &'b c::btree,
    fs: &'f Fs,
}

impl<'b, 'f> fmt::Display for BtreeNodeOndiskToText<'b, 'f> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        printbuf_to_formatter(f, |buf| unsafe {
            c::bch2_btree_node_ondisk_to_text(buf, self.fs.raw, self.b)
        })
    }
}
