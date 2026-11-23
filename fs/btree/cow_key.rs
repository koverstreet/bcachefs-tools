// SPDX-License-Identifier: GPL-2.0

//! CowKey: a key being checked and repaired - read as it is in the btree until
//! the first repair, then a mutable copy, as std::borrow::Cow is borrowed
//! until to_mut(); finish() queues the copy as the key's update.
//!
//! One copy of the value, read by every check and written by every repair. The
//! C kept a copy to read and a mutable key, synced by hand after each repair,
//! and a repair that made its own mutable copy from the original key dropped
//! every repair before it.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{BkeySC, BkeySCToText, MutBkey, BkeyWhole};
use crate::btree::iter::{BtreeIter, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::errcode::BchError;
use crate::fs::Fs;
use core::marker::PhantomData;
use crate::format::Bpos;

/// Key @k, of the type whose value is @K, at @iter.
pub struct CowKey<'i, 'k, 'a, 't, K: BkeyWhole> {
    iter: &'i BtreeIter<'t>,
    k:    BkeySC<'k, K::Validated>,
    /// The value as read, zero padded - until there's @u.
    v:    K,
    /// The key as it will be written, once anything has been repaired.
    u:    Option<TransBkey<'a, 't, K>>,
}

impl<'i, 'k, 'a, 't, K: BkeyWhole> CowKey<'i, 'k, 'a, 't, K>
where
    K::Validated: MutBkey<Mut = K>,
{
    /// None if @k isn't of type @K.
    pub fn new(iter: &'i BtreeIter<'t>, k: BkeySC<'k>) -> Option<Self> {
        let v = K::val_copy_pad(k)?;
        Some(CowKey { iter, k: k.arm::<K>()?, v, u: None })
    }

    pub fn pos(&self) -> Bpos {
        self.k.k.p
    }

    /// The value as it is now, repairs and all.
    pub fn v(&self) -> &K {
        match &self.u {
            Some(u) => u.v(),
            None    => &self.v,
        }
    }

    /// Whether anything has been repaired: finish() queues the key.
    pub fn has_update(&self) -> bool {
        self.u.is_some()
    }

    /// The key, made mutable if it isn't yet.
    pub fn u(&mut self, t: &TransAttempt<'a, 't>) -> Result<&mut TransBkey<'a, 't, K>, BchError> {
        if self.u.is_none() {
            self.u = Some(t.bkey_make_mut_noupdate(self.k)?);
        }
        Ok(self.u.as_mut().expect("just made"))
    }

    /// The repairs, if there were any, queued as the key's update.
    pub fn finish(mut self, t: &TransAttempt<'a, 't>) -> Result<(), BchError> {
        self.queue(t)
    }

    /// The repairs so far queued, for a commit before the checks are done -
    /// one that restarts them: a repair after it is of the key as read.
    pub fn queue(&mut self, t: &TransAttempt<'a, 't>) -> Result<(), BchError> {
        match self.u.take() {
            Some(u) => t.update(self.iter, u, UpdateTriggerFlags::empty()),
            None    => Ok(()),
        }
    }

    /// The value, to repair.
    pub fn v_mut(&mut self, t: &TransAttempt<'a, 't>) -> Result<&mut K, BchError> {
        Ok(self.u(t)?.v_mut())
    }

    /// The key as it is now, to print.
    pub fn key(&self) -> BkeySC<'_> {
        match &self.u {
            Some(u) => BkeySC::from(u.as_ref()),
            None    => BkeySC { k: self.k.k, v: self.k.v, iter: PhantomData, val: PhantomData },
        }
    }

    pub fn to_text<'f>(&self, fs: &'f Fs) -> BkeySCToText<'_, 'f> {
        BkeySCToText { k: self.key().untyped(), fs }
    }
}
