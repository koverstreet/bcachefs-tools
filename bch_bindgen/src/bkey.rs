#![allow(non_camel_case_types)]

use crate::btree::BtreeIter;
use crate::c;
use crate::fs::Fs;
use crate::printbuf_to_formatter;
use std::fmt;
use std::marker::PhantomData;
use std::mem::transmute;

pub struct BkeySC<'a> {
    pub k:           &'a c::bkey,
    pub v:           &'a c::bch_val,
    pub(crate) iter: PhantomData<&'a mut BtreeIter<'a>>,
}

pub enum BkeyValC<'a> {
    deleted,
    whiteout,
    error,
    cookie(&'a c::bch_cookie),
    hash_whiteout(&'a c::bch_hash_whiteout),
    btree_ptr(&'a c::bch_btree_ptr),
    extent(&'a c::bch_extent),
    reservation(&'a c::bch_reservation),
    inode(&'a c::bch_inode),
    inode_generation(&'a c::bch_inode_generation),
    dirent(&'a c::bch_dirent),
    xattr(&'a c::bch_xattr),
    alloc(&'a c::bch_alloc),
    quota(&'a c::bch_quota),
    stripe(&'a c::bch_stripe),
    reflink_p(&'a c::bch_reflink_p),
    reflink_v(&'a c::bch_reflink_v),
    inline_data(&'a c::bch_inline_data),
    btree_ptr_v2(&'a c::bch_btree_ptr_v2),
    indirect_inline_data(&'a c::bch_indirect_inline_data),
    alloc_v2(&'a c::bch_alloc_v2),
    subvolume(&'a c::bch_subvolume),
    snapshot(&'a c::bch_snapshot),
    inode_v2(&'a c::bch_inode_v2),
    alloc_v3(&'a c::bch_alloc_v3),
    set,
    lru(&'a c::bch_lru),
    alloc_v4(&'a c::bch_alloc_v4),
    backpointer(&'a c::bch_backpointer),
    inode_v3(&'a c::bch_inode_v3),
    bucket_gens(&'a c::bch_bucket_gens),
    snapshot_tree(&'a c::bch_snapshot_tree),
    logged_op_truncate(&'a c::bch_logged_op_truncate),
    logged_op_finsert(&'a c::bch_logged_op_finsert),
}

impl<'a, 'b> BkeySC<'a> {
    unsafe fn to_raw(&self) -> c::bkey_s_c {
        c::bkey_s_c {
            k: self.k,
            v: self.v,
        }
    }

    pub fn to_text(&'a self, fs: &'b Fs) -> BkeySCToText<'a, 'b> {
        BkeySCToText { k: self, fs }
    }

    pub fn v(&'a self) -> BkeyValC {
        unsafe {
            let ty: c::bch_bkey_type = transmute(self.k.type_ as u32);

            use c::bch_bkey_type::*;
            use BkeyValC::*;
            match ty {
                KEY_TYPE_deleted => deleted,
                KEY_TYPE_whiteout => whiteout,
                KEY_TYPE_error => error,
                KEY_TYPE_cookie => cookie(transmute(self.v)),
                KEY_TYPE_hash_whiteout => hash_whiteout(transmute(self.v)),
                KEY_TYPE_btree_ptr => btree_ptr(transmute(self.v)),
                KEY_TYPE_extent => extent(transmute(self.v)),
                KEY_TYPE_reservation => reservation(transmute(self.v)),
                KEY_TYPE_inode => inode(transmute(self.v)),
                KEY_TYPE_inode_generation => inode_generation(transmute(self.v)),
                KEY_TYPE_dirent => dirent(transmute(self.v)),
                KEY_TYPE_xattr => xattr(transmute(self.v)),
                KEY_TYPE_alloc => alloc(transmute(self.v)),
                KEY_TYPE_quota => quota(transmute(self.v)),
                KEY_TYPE_stripe => stripe(transmute(self.v)),
                KEY_TYPE_reflink_p => reflink_p(transmute(self.v)),
                KEY_TYPE_reflink_v => reflink_v(transmute(self.v)),
                KEY_TYPE_inline_data => inline_data(transmute(self.v)),
                KEY_TYPE_btree_ptr_v2 => btree_ptr_v2(transmute(self.v)),
                KEY_TYPE_indirect_inline_data => indirect_inline_data(transmute(self.v)),
                KEY_TYPE_alloc_v2 => alloc_v2(transmute(self.v)),
                KEY_TYPE_subvolume => subvolume(transmute(self.v)),
                KEY_TYPE_snapshot => snapshot(transmute(self.v)),
                KEY_TYPE_inode_v2 => inode_v2(transmute(self.v)),
                KEY_TYPE_alloc_v3 => inode_v3(transmute(self.v)),
                KEY_TYPE_set => set,
                KEY_TYPE_lru => lru(transmute(self.v)),
                KEY_TYPE_alloc_v4 => alloc_v4(transmute(self.v)),
                KEY_TYPE_backpointer => backpointer(transmute(self.v)),
                KEY_TYPE_inode_v3 => inode_v3(transmute(self.v)),
                KEY_TYPE_bucket_gens => bucket_gens(transmute(self.v)),
                KEY_TYPE_snapshot_tree => snapshot_tree(transmute(self.v)),
                KEY_TYPE_logged_op_truncate => logged_op_truncate(transmute(self.v)),
                KEY_TYPE_logged_op_finsert => logged_op_finsert(transmute(self.v)),
                KEY_TYPE_MAX => unreachable!(),
            }
        }
    }
}

impl<'a> From<&'a c::bkey_i> for BkeySC<'a> {
    fn from(k: &'a c::bkey_i) -> Self {
        BkeySC {
            k:    &k.k,
            v:    &k.v,
            iter: PhantomData,
        }
    }
}

pub struct BkeySCToText<'a, 'b> {
    k:  &'a BkeySC<'a>,
    fs: &'b Fs,
}

impl<'a, 'b> fmt::Display for BkeySCToText<'a, 'b> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        unsafe {
            printbuf_to_formatter(f, |buf| {
                c::bch2_bkey_val_to_text(buf, self.fs.raw, self.k.to_raw())
            })
        }
    }
}
