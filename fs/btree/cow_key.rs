// SPDX-License-Identifier: GPL-2.0

//! CowKey: a key being checked and repaired - read as it is in the btree until
//! the first repair, then a mutable copy queued as its update, as
//! std::borrow::Cow is borrowed until to_mut().
//!
//! One copy of the value, read by every check and written by every repair. The
//! C kept a copy to read and a mutable key, synced by hand after each repair,
//! and a repair that made its own mutable copy from the original key dropped
//! every repair before it.

use crate::btree::bkey::{BkeySC, BkeySCToText, TypedBkey};
use crate::btree::iter::{BtreeIter, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use core::marker::PhantomData;
use core::mem::size_of;

/// Key @k of type @K, at @iter.
pub struct CowKey<'i, 'k, 'a, 't, K: TypedBkey> {
    iter: &'i BtreeIter<'t>,
    k:    BkeySC<'k>,
    /// The value as read, zero padded - until there's @u.
    v:    K::Val,
    /// The key as it will be written, once anything has been repaired.
    u:    Option<TransBkey<'a, 't>>,
}

impl<'i, 'k, 'a, 't, K: TypedBkey> CowKey<'i, 'k, 'a, 't, K> {
    /// None if @k isn't of type @K.
    pub fn new(iter: &'i BtreeIter<'t>, k: BkeySC<'k>) -> Option<Self> {
        let v = K::val_copy_pad(k)?;
        Some(CowKey { iter, k, v, u: None })
    }

    pub fn pos(&self) -> c::bpos {
        self.k.k.p
    }

    /// The value as it is now, repairs and all.
    pub fn v(&self) -> &K::Val {
        match &self.u {
            Some(u) => K::val(u.k_i()).expect("CowKey: the update changed the key type"),
            None    => &self.v,
        }
    }

    /// Whether anything has been repaired: the key is queued as its update.
    pub fn has_update(&self) -> bool {
        self.u.is_some()
    }

    /// The key, made mutable and queued as its update if it isn't yet.
    pub fn u(&mut self, t: &TransAttempt<'a, 't>) -> Result<&mut TransBkey<'a, 't>, BchError> {
        if self.u.is_none() {
            self.u = Some(t.bkey_make_mut(self.iter, self.k, UpdateTriggerFlags::empty(),
                                          K::TYPE, size_of::<K>())?);
        }
        Ok(self.u.as_mut().expect("just made"))
    }

    /// The value, to repair.
    pub fn v_mut(&mut self, t: &TransAttempt<'a, 't>) -> Result<&mut K::Val, BchError> {
        Ok(K::val_mut(self.u(t)?.k_i_mut()).expect("CowKey: the update changed the key type"))
    }

    /// The key as it is now, to print.
    pub fn key(&self) -> BkeySC<'_> {
        match &self.u {
            Some(u) => BkeySC::from(u.k_i()),
            None    => BkeySC { k: self.k.k, v: self.k.v, iter: PhantomData },
        }
    }

    pub fn to_text<'f>(&self, fs: &'f Fs) -> BkeySCToText<'_, 'f> {
        self.key().to_text(fs)
    }
}
