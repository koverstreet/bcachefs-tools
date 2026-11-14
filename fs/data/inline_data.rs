// SPDX-License-Identifier: GPL-2.0

//! Inline data extents: a file's data in its extent key - inline_data, or
//! reflinked, indirect_inline_data - the Inline family, BkeyTypeFlags::inline_data,
//! and where in each type's value the data is. From inline_data_csum (1.40)
//! their _v2 types carry a checksum of their own over data[]: a node's bset
//! checksum can't say which of its keys went bad.

use crate::alloc::capacity::DiskReservation;
use crate::btree::bkey::{BkeyFamily, BkeyS, BkeySC, BkeyV, BkeyVMut, ByFlag, FamilyVal, FieldOffsets};
use crate::btree::bkey_buf::BtreeKeyBuf;
use crate::btree::bkey::BkeyOps;
use crate::btree::iter::{commit_do, BtreeIter, BtreeIterFlags, CommitFlags, UpdateTriggerFlags};
use crate::c;
use crate::data::checksum::csum_err_msg;
use crate::errcode::BchError;
use crate::format::{Bpos, Btree, BkeyTypeFlags, BkeyTypeFlags_bits};
use crate::fs::{Fs, WriteRef};
use crate::init::damage;
use crate::init::error::{id, BkeyValidate};
use crate::util::backref::BackRef;
use crate::util::refcount::TryRefcount;
use crate::util::task::spawn_static;
use crate::util::vstructs::{cast_plain, cast_plain_mut};
use crate::util::workqueue::Queue;
use crate::util::Printbuf;
use crate::{bch_err_ratelimited, bkey_fsck_err_on, btree_trans, prt};

/// The data itself, inline in the value, direct or indirect,
/// BkeyTypeFlags::inline_data: a key armed to it is a BkeySC<'_, Inline>,
/// BkeyS<'_, Inline>, &BkeyI<Inline>.
pub type Inline = FamilyVal<ByFlag<{ BkeyTypeFlags_bits::inline_data }>>;

pub static DATA: FieldOffsets = FieldOffsets::new(BkeyTypeFlags::inline_data, "data");
pub static CSUM: FieldOffsets = FieldOffsets::optional(BkeyTypeFlags::inline_data, "csum");

impl c::bkey {
    pub fn extent_is_inline_data(&self) -> bool {
        <Inline as BkeyFamily>::contains(self)
    }
}

/// An inline data key's data[], and the checksum over all of it, if it has
/// one. A key with one starts @offset sectors into data[]: a cut moves it,
/// and never touches data[], which the checksum is of as written.
pub struct InlineDataRef<'a> {
    pub csum: Option<&'a c::InlineDataCsum>,
    pub data: &'a [u8],
}

impl<'a> InlineDataRef<'a> {
    /// None if it isn't inline data.
    pub fn of(k: BkeySC<'a>) -> Option<Self> {
        let val = k.val_bytes();
        let csum = match CSUM.of(k.k) {
            Some(at) => Some(val.get(at..).and_then(cast_plain)?),
            None     => None,
        };
        Some(Self { csum, data: val.get(DATA.of(k.k)?..)? })
    }

    /// Where the key's start is in data[], in bytes.
    pub fn start(&self) -> usize {
        self.csum.map_or(0, |c| (c.offset.get() as usize) << 9)
    }

    /// For a read @skip bytes past the key's start: its data from there -
    /// none past data[], which reads as zeroes.
    pub fn read_from(&self, skip: usize) -> &'a [u8] {
        self.data.get(self.start() + skip..).unwrap_or_default()
    }

    pub fn poisoned(&self) -> bool {
        self.csum.is_some_and(|h| h.poisoned())
    }

    /// Ok if it has no checksum or its data matches it.
    pub fn verify(&self) -> Result<(), CsumMismatch> {
        let Some(h) = self.csum else { return Ok(()) };
        let ty = h.csum_type.get();
        let got = ty.checksum_unkeyed(self.data);
        if got == h.csum { Ok(()) } else { Err(CsumMismatch { ty, want: h.csum, got }) }
    }

    /// Its checksum, if it has one, its length, and the first 32 bytes of its
    /// data, in hex.
    pub fn to_text(&self, out: &mut Printbuf) {
        if let Some(h) = self.csum {
            let ty = h.csum_type.get();
            prt!(out, "csum {ty} ");
            h.csum.to_text(out, ty);
            prt!(out, " offset {} ", h.offset.get());
            if h.poisoned() {
                prt!(out, "poisoned ");
            }
        }
        prt!(out, "datalen {}: ", self.data.len());
        for b in &self.data[..self.data.len().min(32)] {
            prt!(out, "{b:02x}");
        }
    }
}

/// The same, to change: a cut's, or a sanitized dump's.
pub struct InlineDataMut<'a> {
    pub csum: Option<&'a mut c::InlineDataCsum>,
    pub data: &'a mut [u8],
}

impl<'a> InlineDataMut<'a> {
    /// None if it isn't inline data.
    pub fn of(k: BkeyS<'a>) -> Option<Self> {
        let (data_at, csum_at) = (DATA.of(k.k())?, CSUM.of(k.k()));
        let (head, data) = k.into_val_bytes_mut().split_at_mut_checked(data_at)?;
        let csum = match csum_at {
            Some(at) => Some(head.get_mut(at..).and_then(cast_plain_mut)?),
            None     => None,
        };
        Some(Self { csum, data })
    }
}

/// An inline data key's data that doesn't match its checksum.
pub struct CsumMismatch {
    pub ty:   c::CsumType,
    pub want: c::bch_csum,
    pub got:  c::bch_csum,
}

impl CsumMismatch {
    pub fn to_text(&self, out: &mut Printbuf) {
        csum_err_msg(out, self.ty, self.want, self.got)
    }
}

impl c::InlineDataCsum {
    /// For @data, written now with @opts: offset 0, the checksum @opts'
    /// data_checksum says - none for none, or for nocow - never an encryption
    /// type, whatever the filesystem is.
    pub fn new(fs: &Fs, opts: &c::bch_inode_opts, data: &[u8]) -> Self {
        let ty = if opts.nocow != 0 {
            c::CsumType::none
        } else {
            c::CsumOpt(opts.data_checksum.into()).to_type(fs, true)
        };
        Self { csum_type: ty.into(), csum: ty.checksum_unkeyed(data), ..Default::default() }
    }

    pub fn poisoned(&self) -> bool {
        self.flags & c::BCH_INLINE_DATA_POISONED != 0
    }
}

pub(crate) fn csum_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let h = InlineDataRef::of(v.k).and_then(|d| d.csum).expect("an inline data key with a checksum");
    let ty = h.csum_type.get();
    bkey_fsck_err_on!(v, ty.0 >= c::CsumType::NR || ty.is_encryption(),
                      id::inline_data_csum_type_bad, "inline data with checksum type {}", ty.0)
}

impl BkeyOps for c::InlineData {
    #[cold]
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, k: BkeySC<'_>) {
        InlineDataRef::of(k).expect("inline data").to_text(out)
    }
}

impl BkeyOps for c::InlineDataV2 {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        csum_validate(v)
    }

    #[cold]
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, k: BkeySC<'_>) {
        InlineDataRef::of(k).expect("inline data").to_text(out)
    }
}

/// @k, in @btree, failed its checksum, read at @read_pos: poisoned, and with
/// @record its damage recorded at @read_pos, in the one transaction - so a
/// key's damage is recorded once, as it's poisoned, however many reads fail
/// on it before that's committed. In a task: a read may hold page locks a
/// commit mustn't wait under. Nothing on a read-only filesystem, as a
/// pointer extent's poisoning.
pub(crate) fn poison(fs: &Fs, btree: Btree, k: BkeySC<'_>, read_pos: Bpos, record: bool) {
    let Some(write) = fs.writes.tryget_live() else { return };
    // the task holds it, and its write ref: going read-only waits for that
    let fs_ref = unsafe { BackRef::new(fs) };
    let key = BtreeKeyBuf::new(btree, 0, k);

    if let Err(e) = spawn_static(Queue::system_dfl(), move || poison_task(write, fs_ref, key, read_pos, record)) {
        bch_err_ratelimited!(fs, "poisoning inline data that failed its checksum: {}", e.msg());
    }
}

async fn poison_task(_write: WriteRef, fs: BackRef<Fs>, key: BtreeKeyBuf, read_pos: Bpos, record: bool) {
    let fs = &*fs;
    let trans = btree_trans!(fs);
    // an extents update commits with a reservation: an empty one, the key's
    // size unchanged
    let res = DiskReservation::new(fs);
    let ret = commit_do(&trans, Some(&res), CommitFlags::empty(), |t| {
        let read = key.key();
        let mut iter = BtreeIter::new(t, key.btree, read.k.start_pos(), BtreeIterFlags::INTENT);

        // the key read, not one written since - and not poisoned already: by
        // another read's task, or before a no_poison_check read
        let Some(k) = iter.peek_slot(t)?.filter(|k| k.and_val_eq(read)) else { return Ok(()) };

        let mut n = t.bkey_make_mut_noupdate(k)?;
        let h = InlineDataMut::of(n.as_s()).and_then(|d| d.csum).expect("inline data with a checksum");
        if h.poisoned() {
            return Ok(());
        }
        h.flags |= c::BCH_INLINE_DATA_POISONED;
        t.update(&iter, &n, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;

        if record {
            damage::record(t, read_pos, id::data_read_csum_err)?;
        }
        Ok(())
    });

    if let Some(e) = ret.err().filter(|e| !e.matches(c::EROFS)) {
        bch_err_ratelimited!(fs, "poisoning inline data that failed its checksum: {}\n  {}",
                             e.msg(), key.key().to_text(fs));
    }
}
