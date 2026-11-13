// SPDX-License-Identifier: GPL-2.0

//! Reading btree nodes: a replica read, retried from the others, and its
//! bsets checked, decrypted and sorted down to one - read_done(); a bset's
//! checksum and encryption; and scrub, a node's replica read back and
//! checked.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bkey_val_to_text, types_with, AllTypes, BkeyI, BkeySC, FamilyVal, ValidateFlags, POS_MIN,
                         SPOS_MAX};
use crate::btree::async_ops::AsyncBtreeOp;
use crate::btree::bkey_buf::BtreeKeyBuf;
use crate::btree::cache::{bch2_btree_id_level_to_text, btree_pos_to_text, Bounce};
use crate::btree::iter::{commit_do, BtreeIter, BtreeIterFlags, CommitFlags};
use crate::btree::node::sort::{build_aux_trees, SortRuns};
use crate::btree::node::{BtreeNode, NodeContents};
use crate::btree::path::Paths;
use crate::btree::trans::BtreeTrans;
use crate::btree::validate::{btree_err_on, btree_node_bkey_val_validate, node_btree, validate_bset,
                             validate_bset_keys, ErrAt, NodeIo, NodeValidate};
use crate::btree::write::trans_submit_write_bios;
use crate::c;
use crate::data::checksum::{csum_err_msg, csum_vstruct, BCH_NONCE_BTREE, CHACHA_BLOCK_SIZE};
use crate::data::inline_data::InlineDataRef;
use crate::data::moving::{Moving, MovingIo};
use crate::data::ptrs::BkeyVRead;
use crate::data::update::dev_ptr_bit;
use crate::data_structures::array_vec::ArrayVec;
use crate::debug::{async_objs, trace::events};
use crate::errcode::{blk_status_to_bch_err, BchError};
use crate::format::{BkeyType, BkeyTypeFlags, Btree, BtreeFlags, DataType, Nonce};
use crate::fs::{Dev, Fs, WriteRef};
use crate::init::{damage::note_lost_extents, error::id, passes::RecoveryPass, recovery::btree_lost_data};
use crate::sb::{errors::FsckFlags, members::DevIoRef};
use crate::types::{Le32, Le64};
use crate::util::bio::{bch2_bio_map, buf_nr_bvecs, of_bio, Bio, BioBox, BioBuffersMapped, BioOwner};
#[cfg(CONFIG_BCACHEFS_DEBUG)]
use crate::os::kernel::random_u64_below;
use crate::os::kernel::{local_clock, mem_alloc_profiling_enabled, unlikely, READ};
use crate::util::log::{LogLevel, LogMsg};
use crate::util::task::{spawn_static, waiter, Resumable, Resume};
use crate::util::vstructs::{bytes_of, bytes_of_mut, cast_plain, cast_plain_mut, from_bytes, from_bytes_mut, Head,
                            Plain, SelfSized, Unsized, Unvalidated};
use crate::util::workqueue::{run_raw, RawWork};
use crate::util::{alloc::init_at, backref::BackRef, closure::StackClosure, printbuf::Msg, refcount::TryRefcount};
use crate::util::{time_stats::TimeStats, Printbuf, ToText};
use crate::{fs_fatal_error, log_msg_ratelimited, prt};
use core::ffi::c_uint;
use core::mem::offset_of;
use core::ops::{ControlFlow, Range};
use core::ptr::{self, NonNull};
use core::sync::atomic::{fence, Ordering::{Relaxed, SeqCst}};
use pin_init::stack_pin_init;

#[repr(C)]
pub struct BtreeReadBio {
    work:       c::work_struct,
    fs:         &'static Fs,
    ca:         Option<DevIoRef<c::DevReadRefKind>>,
    b:          NonNull<BtreeNode>,
    start_time: u64,
    /* its slot on its async object list */
    list_idx:   c_uint,
    pick:       c::extent_ptr_decoded,
    bio:        c::bio,
}
crate::has_bio!(BtreeReadBio, bio);

impl c::bset__head {
    /// The nonce of this bset, at byte @offset of its btree node: the offset,
    /// the bset's seq, and the low half of its journal seq, in the btree
    /// domain.
    pub fn nonce(&self, offset: u32) -> Nonce {
        let seq = self.seq.get();
        Nonce {
            d: [
                Le32::new(offset),
                Le32::new(seq as u32),
                Le32::new((seq >> 32) as u32),
                Le32::new(self.journal_seq.get() as u32 ^ BCH_NONCE_BTREE),
            ],
        }
    }

    fn checksum_type(&self) -> c::CsumType {
        c::CsumType(self.csum_type() as u32)
    }
}

/// What holds a bset: the node, for its first, or a node entry, for each one
/// after - checksummed and encrypted at their byte offset in the node, the
/// node's 0.
pub trait BsetHolder: Unsized<Head: Plain + SelfSized> {
    const BSET: usize;
    const KEYS: usize = Self::BSET + size_of::<Head<c::bset>>();

    fn node(&mut self) -> Option<&mut c::btree_node<Unvalidated>> {
        None
    }

    /// Or decrypted: ChaCha20 is its own inverse.
    fn encrypt(&mut self, fs: &Fs, offset: u32) -> Result<(), BchError>;

    fn bset(&self) -> &Head<c::bset> {
        Self::head(bytes_of(self))
    }

    fn bset_mut(&mut self) -> &mut c::bset<Unvalidated> {
        from_bytes_mut(&mut bytes_of_mut(self)[Self::BSET..]).expect("a bset, its holder's tail")
    }

    /// Its checksum as it's stored: its first bytes.
    fn csum(&self) -> c::bch_csum {
        *cast_plain(bytes_of(self)).expect("a csum, at the start of what holds a bset")
    }

    fn set_csum(&mut self, csum: c::bch_csum) {
        *cast_plain_mut(bytes_of_mut(self)).expect("a csum, at the start of what holds a bset") = csum;
    }

    /// The checksum of all of it past its csum.
    fn checksum(&self, fs: &Fs, offset: u32) -> c::bch_csum {
        let i = self.bset();
        csum_vstruct(Some(fs), i.checksum_type(), i.nonce(offset), self)
    }

    /// Its bset's header, in its @bytes - its count not yet checked against them.
    fn head(bytes: &[u8]) -> &Head<c::bset> {
        cast_plain(&bytes[Self::BSET..]).expect("a bset header, in its node's buffer")
    }

    fn head_mut(bytes: &mut [u8]) -> &mut Head<c::bset> {
        cast_plain_mut(&mut bytes[Self::BSET..]).expect("a bset header, in its node's buffer")
    }

    /// Its header and its bset's keys, to the end of their block - by its
    /// count, not yet checked against @bytes.
    fn sectors(bytes: &[u8], block_bits: u32) -> usize {
        cast_plain::<Self::Head>(bytes).expect("a header, in its node's buffer").sectors(block_bits)
    }

    /// It, at the start of @bytes - its count checked against them.
    fn of(bytes: &[u8]) -> &Self {
        from_bytes(bytes).expect(FITS)
    }

    fn of_mut(bytes: &mut [u8]) -> &mut Self {
        from_bytes_mut(bytes).expect(FITS)
    }
}

/// A node's first bset - its encryption covers the node's header from flags
/// on too, ahead of the keys, their nonce starting at the next ChaCha20 block
/// after it.
impl BsetHolder for c::btree_node<Unvalidated> {
    const BSET: usize = offset_of!(Head<c::btree_node>, keys);

    fn node(&mut self) -> Option<&mut c::btree_node<Unvalidated>> {
        Some(self)
    }

    fn encrypt(&mut self, fs: &Fs, offset: u32) -> Result<(), BchError> {
        let (ty, nonce) = (self.keys.checksum_type(), self.keys.nonce(offset));
        let (flags, keys) = (offset_of!(Head<c::btree_node>, flags), Self::BSET);
        let bytes = self.as_bytes_mut();

        fs.encrypt(ty, nonce, &mut bytes[flags..keys])?;
        let nonce = nonce + ((keys - flags) as u32).next_multiple_of(CHACHA_BLOCK_SIZE);
        fs.encrypt(ty, nonce, &mut bytes[Self::KEYS..])
    }
}

impl BsetHolder for c::btree_node_entry<Unvalidated> {
    const BSET: usize = offset_of!(Head<c::btree_node_entry>, keys);

    fn encrypt(&mut self, fs: &Fs, offset: u32) -> Result<(), BchError> {
        let (ty, nonce) = (self.keys.checksum_type(), self.keys.nonce(offset));
        fs.encrypt(ty, nonce, &mut self.as_bytes_mut()[Self::KEYS..])
    }
}

/// Whether @buf, read from a replica of the node @k points to, is that node:
/// its magic, its seq if @k has one, and every bset - the first and those
/// appended since - checksummed, as far as @k says was written; if it doesn't
/// say, till the first bset of another node. If not, what's wrong, to @err.
pub fn node_replica_good(fs: &Fs, k: BkeySC<'_>, buf: &[u8], err: &mut Printbuf) -> bool {
    let p = NodePtr::new(fs, k);
    // A corrupt key's sectors_written can't take the walk past the buffer
    let end = p.end.min(buf.len() >> 9);

    let bn = cast_plain::<Head<c::btree_node>>(buf).expect("a btree node's worth of buffer");
    if bn.magic.get() != fs.bset_magic() {
        prt!(err, "bad magic: want {:x}, got {:x}", fs.bset_magic(), bn.magic.get());
        return false;
    }

    if let Some(seq) = p.seq.filter(|&seq| bn.keys.seq.get() != seq) {
        prt!(err, "wrong btree node: seq {:x}, want {seq:x}", bn.keys.seq.get());
        return false;
    }

    let mut written = 0;
    while written < end {
        let sectors = if written == 0 {
            replica_bset::<c::btree_node<Unvalidated>>(fs, buf, written, end, err)
        } else if c::btree_node_entry::<Unvalidated>::head(&buf[written << 9..]).seq != bn.keys.seq {
            break;
        } else {
            replica_bset::<c::btree_node_entry<Unvalidated>>(fs, buf, written, end, err)
        };
        let Some(sectors) = sectors else { return false };

        written += sectors;
    }

    if p.written != 0 && written < p.written {
        prt!(err, "btree node data missing: expected {} sectors, found {written}", p.written);
        return false;
    }

    true
}

/// The bset at sector @written of @buf, checked against @end and against its
/// checksum: its sectors, or None, and what's wrong in @err.
fn replica_bset<H: BsetHolder + ?Sized>(fs: &Fs, buf: &[u8], written: usize, end: usize,
                                        err: &mut Printbuf) -> Option<usize> {
    let offset = written << 9;
    let sectors = H::sectors(&buf[offset..], fs.block_bits());

    if written + sectors > end {
        prt!(err, "bset past end of btree node (offset {written} len {sectors} but written {end})");
        return None;
    }

    let n = H::of(&buf[offset..(written + sectors) << 9]);
    let ty = n.bset().checksum_type();
    if ty.valid(fs) {
        let (stored, csum) = (n.csum(), n.checksum(fs, offset as u32));
        if stored != csum {
            csum_err_msg(err, ty, stored, csum);
            return None;
        }
    }

    Some(sectors)
}

/// What's on disk of @b, read back from a replica: each bset, checked and
/// decrypted, and its keys.
#[cold]
pub fn node_ondisk_to_text(out: &mut Printbuf, fs: &Fs, b: &BtreeNode) {
    let contents = unsafe { b.contents() };
    let k = BkeySC::from(contents.key_i());

    let dev = match k.pick_read_device(fs, None, 0, c::ReadFlags::empty()) {
        Ok(Some(pick)) => pick.ptr.dev().into(),
        _ => {
            prt!(out, "error getting device to read from: invalid device\n");
            return;
        }
    };
    let Ok(Some(mut r)) = ReplicaRead::new(fs, k, dev) else {
        prt!(out, "error getting device to read from: not online\n");
        return;
    };

    if let Err(e) = r.bio.submit_wait() {
        prt!(out, "IO error reading btree node: {e}\n");
        return;
    }

    let buf = r.bio.buf_mut();
    let node_seq = node_header(buf).keys.seq;
    let mut offset = 0;

    while offset < fs.btree_sectors() as usize {
        let bytes = &mut buf[offset << 9..];
        let sectors = if offset == 0 {
            bset_ondisk_to_text::<c::btree_node<Unvalidated>>(out, fs, contents, bytes, offset)
        } else if c::btree_node_entry::<Unvalidated>::head(bytes).seq != node_seq {
            break;
        } else {
            bset_ondisk_to_text::<c::btree_node_entry<Unvalidated>>(out, fs, contents, bytes, offset)
        };
        let Some(sectors) = sectors else { return };

        offset += sectors;
    }
}

/// The bset at the start of @bytes, sector @offset of its node, checked,
/// decrypted and its keys printed: its sectors, or None if it's bad.
fn bset_ondisk_to_text<H: BsetHolder + ?Sized>(out: &mut Printbuf, fs: &Fs, contents: &NodeContents,
                                               bytes: &mut [u8], offset: usize) -> Option<usize> {
    let ty = H::head(bytes).checksum_type();
    if !ty.valid(fs) {
        prt!(out, "unknown checksum type at offset {}: {}\n", offset, H::head(bytes).csum_type());
        return None;
    }

    let sectors = H::sectors(bytes, fs.block_bits());
    let Some(bytes) = bytes.get_mut(..sectors << 9) else {
        prt!(out, "bset at offset {} past the end of the node\n", offset);
        return None;
    };
    let n = H::of_mut(bytes);

    if n.csum() != n.checksum(fs, (offset << 9) as u32) {
        prt!(out, "invalid checksum\n");
        return None;
    }

    let _ = n.encrypt(fs, (offset << 9) as u32);

    let i = n.bset_mut();
    prt!(out, "  offset {} version {}, journal seq {}\n", offset, i.version.get(), i.journal_seq.get());

    let mut out = out.indent(2);
    let mut at = 0;
    while let Some(Ok(k)) = i.key_at(at) {
        let k = k.key();
        if k.u64s() == 0 {
            prt!(out, "zero length key at byte {at}\n");
            break;
        }

        let mut u = c::bkey::default();
        bkey_val_to_text(&mut out, Some(fs), k.cast::<FamilyVal<AllTypes>>().disassemble(contents, &mut u));
        out.newline();
        at += k.u64s() * size_of::<u64>();
    }

    Some(sectors)
}

/*
 * When btree topology repair changes the start or end of a node, that might
 * mean we have to drop keys that are no longer inside the node:
 */
#[cold]
pub fn drop_keys_outside_node(b: &BtreeNode, contents: &mut NodeContents) {
    let (min_key, max_key) = (contents.data().min_key, contents.data().max_key);

    let mut keys = b.keys_mut(contents);
    keys.drop_keys_outside(min_key, max_key);

    /*
     * Always rebuild search trees: eytzinger search tree nodes directly
     * depend on the values of min/max key:
     */
    keys.sets.set_no_aux_tree(0);
    build_aux_trees(b, contents);
    contents.nr = b.keys(contents).btree_node_count_keys();

    let _ = b.for_each_key(contents, |k| {
        assert!(k.k.p >= min_key, "key {} before its node's min {min_key}", k.k.p);
        assert!(k.k.p <= max_key, "key {} past its node's max {max_key}", k.k.p);
        ControlFlow::Continue(())
    });
}

const NODE_BYTES: usize = size_of::<Head<c::btree_node>>();
const FITS: &str = "a bset inside the node, checked against its end";

fn node_header_to_text(out: &mut Printbuf, bn: &Head<c::btree_node>) {
    bch2_btree_id_level_to_text(out, Btree(node_btree(bn)), bn.level() as c_uint);
    prt!(out, " seq {:x} {}\n", bn.keys.seq.get(), bn.seq());
    prt!(out, "min: {}\n", bn.min_key);
    prt!(out, "max: {}", bn.max_key);
}

pub fn read_done(fs: &Fs, ca: &Dev, b: &BtreeNode, failed: Option<&mut c::bch_io_failures>,
                 err_msg: &mut Printbuf) -> Result<(), BchError> {
    fs.times[c::TimeStat::btree_node_read_done as usize]
        .time(|| node_read_done(&mut NodeValidate { fs, b, io: NodeIo::Read { ca, failed, err_msg } }))
}

struct NodePtr {
    /// Sectors written: 0 for a v1 pointer, which doesn't say.
    written:       usize,
    /// Where its bsets end: written, or the whole node.
    end:           usize,
    seq:           Option<u64>,
    range_updated: bool,
}

impl NodePtr {
    fn new(fs: &Fs, k: BkeySC<'_>) -> Self {
        let v2 = k.arm::<c::BtreePtrV2>();
        let v2 = v2.as_ref().map(|v2| v2.v());
        let written = v2.map_or(0, |v2| v2.sectors_written.get() as usize);
        Self {
            written,
            end:           if written != 0 { written } else { fs.btree_sectors() as usize },
            seq:           v2.map(|v2| v2.seq.get()),
            range_updated: v2.is_some_and(|v2| v2.btree_ptr_range_updated()),
        }
    }
}

enum BsetRead {
    /// Another node's: the end of this one.
    End,
    /// Blacklisted, and not the first: its keys aren't the node's.
    Skipped,
    /// By offset in the node, in u64s.
    Keys { keys: Range<usize>, journal_seq: u64 },
}

/// # Safety
/// The read has the node - read_in_flight - and nothing else has its buffer.
unsafe fn node_buf<'a>(b: &BtreeNode, contents: &NodeContents) -> &'a mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(contents.data.cast::<u8>(), b.buf_bytes()) }
}

fn node_header(buf: &[u8]) -> Head<c::btree_node> {
    *cast_plain(buf).expect(FITS)
}

fn node_read_done(v: &mut NodeValidate<'_>) -> Result<(), BchError> {
    let (fs, b) = (v.fs, v.b);
    let p = NodePtr::new(fs, unsafe { b.contents() }.key_i().into());

    unsafe { b.contents_mut() }.version_ondisk = u16::MAX;
    /* We might get called multiple times on read retry: */
    b.written.set(0);

    let mut runs = SortRuns::new(fs);
    let buf = unsafe { node_buf(b, b.contents()) };

    let magic = node_header(buf).magic.get();
    btree_err_on!(v, magic != fs.bset_magic(),
                  FsckFlags::empty(),
                  ErrAt::NODE,
                  id::btree_node_bad_magic,
                  "bad magic: want {:x}, got {:x}",
                  fs.bset_magic(), magic)?;

    let mut max_journal_seq = 0;
    while (b.written.get() as usize) < p.end {
        let read = if b.written.get() == 0 {
            read_bset::<c::btree_node<Unvalidated>>(v, &p, buf)?
        } else {
            read_bset::<c::btree_node_entry<Unvalidated>>(v, &p, buf)?
        };
        match read {
            BsetRead::End                        => break,
            BsetRead::Skipped                    => {}
            BsetRead::Keys { keys, journal_seq } => {
                runs.add(keys);
                max_journal_seq = max_journal_seq.max(journal_seq);
            }
        }
    }

    read_done_end(v, &p, buf)?;
    sort_bsets(fs, b, &mut runs, max_journal_seq);
    validate_vals(v)?;

    let contents = unsafe { b.contents_mut() };
    b.keys_mut(contents).bset_build_aux_tree(0, false);

    contents.reset_sib_u64s();

    if p.range_updated {
        drop_keys_outside_node(b, contents);
    }

    if p.written == 0 {
        b.flags.set_need_rewrite(true);
        b.flags.set_need_rewrite_ptr_written_zero(true);
    }

    Ok(())
}

fn read_bset<H: BsetHolder + ?Sized>(v: &mut NodeValidate<'_>, p: &NodePtr, buf: &mut [u8])
    -> Result<BsetRead, BchError>
{
    let (fs, b) = (v.fs, v.b);
    let written = b.written.get() as usize;
    let first = written == 0;
    let offset = written << 9;
    let node_seq = node_header(buf).keys.seq;
    let bytes = &mut buf[offset..];
    let i = *H::head(bytes);

    if !first && i.seq != node_seq {
        return Ok(BsetRead::End);
    }

    let ty = i.checksum_type();
    let good_csum_type = ty.valid(fs);

    btree_err_on!(v, !good_csum_type,
                  if ty.is_encryption() { FsckFlags::empty() } else { FsckFlags::CAN_FIX },
                  ErrAt::bset(&i),
                  id::bset_unknown_csum,
                  "unknown checksum type {}", i.csum_type())?;

    let sectors = H::sectors(bytes, fs.block_bits());

    if btree_err_on!(v, written + sectors > p.end,
                     FsckFlags::CAN_FIX,
                     ErrAt::bset(&i),
                     id::bset_past_end_of_btree_node,
                     "bset past end of btree node (offset {} len {} but written {})",
                     written, sectors, p.end)? {
        H::head_mut(bytes).set_u64s(0);
        v.keys_lost();
    }

    let mut n = H::of_mut(bytes);

    if good_csum_type {
        let (stored, csum) = (n.csum(), n.checksum(fs, offset as u32));
        let csum_bad = stored != csum;
        if csum_bad {
            v.csum_bad();
        }

        btree_err_on!(v, csum_bad,
                      FsckFlags::CAN_FIX,
                      ErrAt::bset(n.bset()),
                      id::bset_bad_csum,
                      "{}", Msg(|out: &mut Printbuf| csum_err_msg(out, ty, stored, csum)))?;

        if let Err(err) = n.encrypt(fs, offset as u32) {
            fs_fatal_error!(fs, "decrypting btree node: {}", err.msg());
            return Err(err);
        }
    }

    if let Some(bn) = n.node() {
        let bn = node_header(bn.as_bytes());

        match p.seq {
            Some(seq) => btree_err_on!(v, bn.keys.seq.get() != seq,
                                       FsckFlags::empty(),
                                       ErrAt::NODE,
                                       id::btree_node_bad_seq,
                                       "got wrong btree node: got\n{}",
                                       Msg(|out: &mut Printbuf| node_header_to_text(&mut out.indent(2), &bn)))?,
            None      => btree_err_on!(v, bn.keys.seq.get() == 0,
                                       FsckFlags::empty(),
                                       ErrAt::NODE,
                                       id::btree_node_bad_seq,
                                       "bad btree header: seq 0\n{}",
                                       Msg(|out: &mut Printbuf| node_header_to_text(out, &bn)))?,
        };

        btree_err_on!(v, b.c.level == 0 &&
                         Btree(b.c.btree_id.into()).flags().contains(BtreeFlags::extents) &&
                         !bn.new_extent_overwrite(),
                      FsckFlags::empty(),
                      ErrAt::NODE,
                      id::btree_node_unsupported_version,
                      "btree node does not have NEW_EXTENT_OVERWRITE set")?;
    }

    /*
     * The bset code addresses keys by u16 offsets from b->data, and a
     * 512k node's end is one past the last: no bset may end there
     */
    let keys_at = (offset + H::KEYS) / 8;

    if btree_err_on!(v, keys_at + n.bset().u64s() == u16::MAX as usize + 1,
                     FsckFlags::CAN_FIX,
                     ErrAt::bset(n.bset()),
                     id::bset_past_end_of_btree_node,
                     "bset ends at the end of the btree node, u64 {}: past the last u16 offset",
                     u16::MAX as u32 + 1)? {
        n.bset_mut().truncate(0);
        n = H::of_mut(bytes);
        v.keys_lost();
    }

    let contents = unsafe { b.contents_mut() };
    contents.version_ondisk = contents.version_ondisk.min(n.bset().version.get());

    validate_bset(v, n, written as u32)?;

    if let Some(bn) = n.node() {
        unsafe { b.contents_mut() }.set_format(bn.format);
    }

    validate_bset_keys(v, n)?;

    let i = n.bset_mut();
    i.set_big_endian(cfg!(target_endian = "big"));

    let seq = i.journal_seq.get();
    let blacklisted = fs.journal_seq_is_blacklisted(seq, true);

    btree_err_on!(v, blacklisted && first,
                  FsckFlags::CAN_FIX,
                  ErrAt::bset(i),
                  id::first_bset_blacklisted_journal_seq,
                  "first btree node bset has blacklisted journal seq ({})",
                  seq)?;

    btree_err_on!(v, blacklisted && p.written != 0,
                  FsckFlags::CAN_FIX,
                  ErrAt::bset(i),
                  id::bset_blacklisted_journal_seq,
                  "found blacklisted bset (journal seq {}) in btree node at offset {}-{}/{}",
                  seq, written, written + sectors, p.written)?;

    b.written.set((written + sectors).min(fs.btree_sectors() as usize) as u16);

    if blacklisted && !first {
        return Ok(BsetRead::Skipped);
    }

    Ok(BsetRead::Keys { keys: keys_at..keys_at + i.u64s(), journal_seq: seq })
}

/// The node's bsets read to the end: as many sectors as its pointer says, or
/// none after the last that look like its.
fn read_done_end(v: &mut NodeValidate<'_>, p: &NodePtr, buf: &[u8]) -> Result<(), BchError> {
    let (fs, b) = (v.fs, v.b);

    if p.written != 0 {
        if btree_err_on!(v, (b.written.get() as usize) < p.written,
                         FsckFlags::CAN_FIX,
                         ErrAt::NODE,
                         id::btree_node_data_missing,
                         "btree node data missing: expected {} sectors, found {}",
                         p.written, b.written.get())? {
            v.keys_lost();
        }
    } else {
        let data_seq = node_header(buf).keys.seq;
        let mut bne = (b.written.get() as usize) << 9;

        while bne < b.buf_bytes() {
            let e = *cast_plain::<Head<c::btree_node_entry>>(&buf[bne..]).expect(FITS);

            if btree_err_on!(v, e.keys.seq == data_seq &&
                                !fs.journal_seq_is_blacklisted(e.keys.journal_seq.get(), true),
                             FsckFlags::CAN_FIX,
                             ErrAt::NODE,
                             id::btree_node_bset_after_end,
                             "found bset signature after last bset")? {
                v.keys_lost();
            }
            bne += fs.block_bytes() as usize;
        }
    }

    Ok(())
}

fn sort_bsets(fs: &Fs, b: &BtreeNode, runs: &mut SortRuns<'_>, journal_seq: u64) {
    let contents = unsafe { b.contents_mut() };
    let buf = unsafe { node_buf(b, contents) };
    let mut sorted = Bounce::new(fs, b.buf_bytes());

    let (u64s, nr) = runs.key_sort_fix_overlapping(&b.keys(contents), &mut sorted.u64s()[NODE_BYTES / 8..]);
    contents.nr = nr;
    sorted.u64s()[NODE_BYTES / 8 + nr.live_u64s as usize..].fill(0);

    cast_plain_mut::<Head<c::btree_node>>(&mut *buf).expect(FITS).keys.set_u64s(u64s);
    sorted.as_bytes_mut()[..NODE_BYTES].copy_from_slice(&buf[..NODE_BYTES]);

    if !mem_alloc_profiling_enabled() {
        sorted.swap(b, contents);
    } else {
        buf[..NODE_BYTES + u64s * 8].copy_from_slice(&sorted.as_bytes()[..NODE_BYTES + u64s * 8]);
    }

    contents.nsets = 1;
    b.keys_mut(contents).set_bset(0, (offset_of!(Head<c::btree_node>, keys) / 8) as u16);

    let n = c::btree_node::<Unvalidated>::from_bytes_mut(unsafe { node_buf(b, contents) }).expect(FITS);
    n.bset_mut().journal_seq = Le64::new(journal_seq);

    assert!(contents.nr.live_u64s as usize == n.keys.u64s(),
            "btree node read: {} live u64s sorted, its bset {}", contents.nr.live_u64s, n.keys.u64s());
}

fn validate_vals(v: &mut NodeValidate<'_>) -> Result<(), BchError> {
    let (fs, b) = (v.fs, v.b);
    let n = c::btree_node::<Unvalidated>::from_bytes_mut(unsafe { node_buf(b, b.contents()) }).expect(FITS);
    let mut i = n.bset_mut();
    let mut at = 0;

    while let Some(k) = i.key_at_mut(at) {
        let k = k.expect("a sorted key").into_key_mut().cast_mut::<FamilyVal<AllTypes>>();

        let drop_key = {
            let contents = unsafe { b.contents() };
            let mut tmp = c::bkey::default();
            let u = k.disassemble(contents, &mut tmp);

            match btree_node_bkey_val_validate(fs, b, u, ValidateFlags::empty()) {
                Err(err) if err == BchError::from(c::Errcode::BCH_ERR_fsck_delete_bkey) => true,
                _ if crate::debug::params::inject_invalid_keys() && u.k.bversion == c::Bversion::MAX => true,
                r => { r?; false }
            }
        };

        if drop_key {
            unsafe { b.contents_mut() }.nr.account_key_drop(0, k);

            let bytes = k.bytes();
            i = i.remove(at, bytes);
            b.flags.set_need_rewrite(true);
            b.flags.set_need_rewrite_error(true);
            v.keys_lost();
            continue;
        }

        if k.key_type() == BkeyType::btree_ptr_v2 {
            k.val_mut(&unsafe { b.contents() }.format)[0] = 0;
        }

        at += k.bytes();
    }

    b.keys_mut(unsafe { b.contents_mut() }).set_bset_end(0);
    Ok(())
}


#[cfg(CONFIG_BCACHEFS_DEBUG)]
crate::module_param! {
    pub(crate) static btree_read_corrupt_ratio: u32 = 0;
    perm = 0o644, desc = "";
}

#[cfg(CONFIG_BCACHEFS_DEBUG)]
crate::module_param! {
    pub(crate) static btree_read_corrupt_device: i32 = 0;
    perm = 0o644, desc = "";
}

pub fn bch2_btree_node_wait_on_read(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, b: &BtreeNode) {
    if unlikely(!trans.queued_write_bios.get().is_null()) {
        trans_submit_write_bios(trans);
    }

    trans.wait_on_node_flag(paths, b, c::BtreeNodeFlags::read_in_flight);
}

pub fn bch2_btree_node_wait_on_write(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, b: &BtreeNode) {
    /*
     * The write we're about to wait on may be sitting unsubmitted on
     * our own queued_write_bios (e.g. the btree cache cannibalize and
     * evict paths write a node and immediately wait on it) — submit
     * before sleeping, or we'd wait on ourselves forever:
     */
    if unlikely(!trans.queued_write_bios.get().is_null()) {
        trans_submit_write_bios(trans);
    }

    trans.wait_on_node_flag(paths, b, c::BtreeNodeFlags::write_in_flight);
}

fn read_done_wake(b: &BtreeNode) {
    b.flags.set_read_in_flight(false);
    fence(SeqCst);
    b.flags.wake();
}

impl RawWork for BtreeReadBio {
    const WORK: usize = offset_of!(Self, work);

    unsafe fn run(rb: NonNull<Self>) {
        unsafe { read_work(rb) }
    }
}

/// # Safety
/// @rb's first read is done - waited on, or its endio queued it: it's put here.
unsafe fn read_work(rb: NonNull<BtreeReadBio>) {
    let rb = unsafe { &mut *rb.as_ptr() };
    let fs = rb.fs;
    let b = unsafe { rb.b.as_ref() };
    let btree = Btree(b.c.btree_id.into());

    let mut failed = c::bch_io_failures::default();

    let mut buf = LogMsg::new(fs);

    prt!(&mut buf, "btree node read error at btree {}\n", b.pos_to_text(fs));

    let ret = loop {
        let ca = rb.ca.take();

        let ret = if rb.bio.bi_status == 0 {
            rb.bio.bi_iter = Default::default();
            rb.bio.bi_iter.bi_size = b.buf_bytes() as u32;

            #[cfg(CONFIG_BCACHEFS_DEBUG)]
            {
                let (dev, ratio) = (btree_read_corrupt_device.get(), btree_read_corrupt_ratio.get());
                if (dev == rb.pick.ptr.dev() as i32 || dev < 0) &&
                   ratio != 0 && random_u64_below(ratio.into()) == 0 {
                    unsafe { Bio::from_raw_mut(&raw mut rb.bio) }.corrupt();
                }
            }

            let ca = ca.as_ref().expect("a read that completed, from a device it had a ref on");
            read_done(fs, ca, b, Some(&mut failed), &mut buf)
        } else {
            let err = fs.err(blk_status_to_bch_err(rb.bio.bi_status));
            failed.mark(&rb.pick, err);
            Err(err)
        };

        if ret.is_ok() {
            break ret;
        }
        match BkeySC::from(unsafe { b.contents() }.key_i())
            .pick_read_device(fs, Some(&failed), 0, c::ReadFlags::empty()) {
            Ok(Some(pick)) => rb.pick = pick,
            _ => break ret,
        }

        rb.ca = fs.dev_get_ioref(rb.pick.ptr.dev().into());
        rb.start_time = local_clock();
        unsafe { c::bio_reset(&raw mut rb.bio, ptr::null_mut(), c::req_op_REQ_OP_READ | c::REQ_SYNC | c::REQ_META) };
        rb.bio.bi_iter.bi_sector = rb.pick.ptr.offset();
        rb.bio.bi_iter.bi_size = b.buf_bytes() as u32;

        match &rb.ca {
            Some(ca) => {
                unsafe { Bio::from_raw_mut(&raw mut rb.bio) }.set_dev(ca.disk_sb.bdev);
                unsafe { c::submit_bio_wait(&raw mut rb.bio) };
                ca.account_io_completion(c::MemberErrorType::read, rb.start_time, rb.bio.bi_status == 0);
            }
            None => rb.bio.bi_status = c::BLK_STS_REMOVED,
        }
    };

    failed.to_text(&mut buf, fs);

    /*
     * only print retry success if we read from a replica with no errors
     */
    if let Err(err) = ret {
        /*
         * Initialize buf.suppress before btree_lost_data(); that will
         * clear it if it did any work (scheduling recovery passes,
         * marking superblock
         */
        buf.set_suppressed(fs.ratelimit_suppress(&fs.btree.read_errors_hard, crate::here!()));

        b.flags.set_read_error(true);
        let _ = btree_lost_data(fs, &mut buf, btree);
        note_lost_extents(fs, btree, unsafe { b.contents() }.key_i().into());
        prt!(&mut buf, "error {}\n", err.msg());
    } else if !failed.is_empty() {
        /* Separate ratelimit states for soft vs. hard errors */
        buf.set_suppressed(fs.ratelimit_suppress(&fs.btree.read_errors_soft, crate::here!()));

        let dev = rb.pick.ptr.dev().into();
        let used = failed.dev(dev).is_some();
        let used_err = |code| failed.dev_err(dev).is_some_and(|err| err.matches(code));

        if !used {
            prt!(&mut buf, "retry success");
        } else if used_err(c::Errcode::BCH_ERR_btree_node_validate_err_csum) {
            prt!(&mut buf, "checksum error, no good replica: read as is, contents unverified");
        } else {
            prt!(&mut buf, "repair success");
        }

        if (!failed.is_empty() || b.flags.need_rewrite()) &&
           fs.recovery.runner.current_pass.get() != RecoveryPass::scan_for_btree_nodes {
            prt!(&mut buf, " (rewriting node)");
            fs.async_btree_op(b, unsafe { b.contents() }, AsyncBtreeOp::Rewrite);
        }

        buf.newline();

        /* the replica we're using lost keys: the btree lost data */
        if used && used_err(c::Errcode::BCH_ERR_btree_node_validate_err_keys_lost) {
            let _ = btree_lost_data(fs, &mut buf, btree);
        }
    } else {
        buf.set_suppressed(true);
    }

    buf.set_level(if ret.is_err() { LogLevel::ERR } else { LogLevel::NOTICE });
    drop(buf);

    unsafe { async_objs::list_del(fs, c::AsyncObjKind::btree_read_bio, &mut rb.list_idx) };
    fs.times[c::TimeStat::btree_node_read as usize].update(rb.start_time);

    let contents = unsafe { b.contents() };
    if ret.is_ok() &&
       !crate::debug::params::btree_node_merging_disabled() &&
       (contents.data().min_key != POS_MIN || contents.key.k.p != SPOS_MAX) &&
       contents.nr.live_u64s < fs.btree.foreground_merge_threshold / 2 {
        /* Ensure very empty btree nodes get merged, in case they're
         * being read but never written to - but with a very low
         * threshold, or __bch2_foreground_maybe_merge() will
         * recursively kick off more merge attempts as it reads sibling
         * nodes:
         */
        fs.async_btree_op(b, contents, AsyncBtreeOp::MergeNoRead);
    }

    unsafe { c::bio_put(&raw mut rb.bio) };
    read_done_wake(b);
}

unsafe extern "C" fn read_endio(bio: *mut c::bio) {
    let rb = unsafe { &mut *of_bio::<BtreeReadBio>(bio) };

    if let Some(ca) = &rb.ca {
        ca.account_io_completion(c::MemberErrorType::read, rb.start_time, rb.bio.bi_status == 0);
    }

    unsafe { rb.fs.btree.read_complete_wq.queue_work(&raw mut rb.work) };
}

#[cold]
pub fn read_bio_to_text(out: &mut Printbuf, rb: &BtreeReadBio) {
    unsafe { Bio::from_raw(&rb.bio) }.to_text(out)
}

pub fn node_read(trans: &BtreeTrans<'_>, b: &BtreeNode, sync: bool) {
    let fs = trans.fs();
    let btree = Btree(b.c.btree_id.into());
    let key = BkeySC::from(unsafe { b.contents() }.key_i());

    fs.event_inc_trace(events::btree_node_read, |out| prt!(out, "{}", b.pos_to_text(fs)));

    let Ok(Some(pick)) = key.pick_read_device(fs, None, 0, c::ReadFlags::empty()) else {
        let mut msg = log_msg_ratelimited!(fs);

        prt!(&mut msg, "btree node read error: no device to read from\n at {}\n", b.pos_to_text(fs));
        let _ = btree_lost_data(fs, &mut msg, btree);
        note_lost_extents(fs, btree, key);

        if fs.recovery.runner.passes_complete.get() & (1 << RecoveryPass::check_topology as u32) != 0 {
            fs.emergency_read_only(&mut msg);
        }

        b.flags.set_read_error(true);
        read_done_wake(b);
        return;
    };

    let ca = fs.dev_get_ioref::<c::DevReadRefKind>(pick.ptr.dev().into());

    let (data, bytes) = (unsafe { b.contents() }.data.cast::<u8>(), b.buf_bytes());
    let bio = unsafe {
        c::bio_alloc_bioset(ptr::null_mut(),
                            buf_nr_bvecs(data, bytes) as _,
                            c::req_op_REQ_OP_READ | c::REQ_SYNC | c::REQ_META,
                            c::GFP_NOIO,
                            fs.btree.bio.as_ptr())
    };
    let rb = unsafe { of_bio::<BtreeReadBio>(bio) };
    unsafe {
        (&raw mut (*rb).fs).write(Fs::borrow_raw(fs.as_ptr()));
        (&raw mut (*rb).ca).write(ca);
        (&raw mut (*rb).b).write(NonNull::from(b));
    }
    let rb = unsafe { &mut *rb };
    rb.start_time       = local_clock();
    rb.pick             = pick;
    let Ok(()) = unsafe { init_at(crate::new_work!(run_raw::<BtreeReadBio>, "&rb->work"), &raw mut rb.work) };
    rb.bio.bi_iter.bi_sector = pick.ptr.offset();
    rb.bio.bi_end_io    = Some(read_endio);
    unsafe { bch2_bio_map(bio, data.cast(), bytes) };

    let list = &fs.async_objs[c::AsyncObjKind::btree_read_bio as usize].list;
    rb.list_idx = list.add(NonNull::from(&mut *rb).cast()).unwrap_or(0);

    if let Some(ca) = &rb.ca {
        ca.io_done_add(READ, DataType::btree, (rb.bio.bi_iter.bi_size >> 9).into());
        unsafe { Bio::from_raw_mut(bio) }.set_dev(ca.disk_sb.bdev);

        /*
         * Callers drop btree locks before reading a node but not srcu
         * (bch2_trans_unlock(), not unlock_long()), so the read happens
         * under the srcu read lock and submit_bio() can block for an
         * unbounded time on a congested device. Flag it so
         * bch2_trans_unlock_long() doesn't report legitimate IO as a
         * stuck codepath; cleared on the next relock/trans_begin().
         */
        trans.set_srcu_io_submitted(true);

        if sync {
            unsafe { c::submit_bio_wait(bio) };
            ca.latency_acct(rb.start_time, READ);
            unsafe { read_work(NonNull::from(rb)) };
        } else {
            unsafe { c::submit_bio(bio) };
        }
    } else {
        rb.bio.bi_status = c::BLK_STS_REMOVED;

        if sync {
            unsafe { read_work(NonNull::from(rb)) };
        } else {
            unsafe { fs.btree.read_complete_wq.queue_work(&raw mut rb.work) };
        }
    }
}

impl Fs {
    pub fn btree_root_read(&self, id: Btree, k: &BkeyI, level: u32) -> Result<(), BchError> {
        let trans = crate::btree_trans!(self);

        stack_pin_init!(let cl = StackClosure::new());
        loop {
            let ret = trans.btree_cache_cannibalize_lock(NonNull::new(cl.as_ptr()));
            cl.sync();
            if ret.is_ok() {
                break;
            }
        }

        let b = trans.btree_node_mem_alloc(&mut trans.paths(), level != 0);
        trans.btree_cache_cannibalize_unlock();

        let mut n = b.unwrap_or_else(|e| panic!("allocating a btree root's node: {e}"));
        unsafe { n.contents_mut() }.set_key(k);
        n.set_id(id, level as usize);
        let (b, held) = n.hash(c::BtreeNodeCacheState::CLEAN)
            .unwrap_or_else(|e| panic!("hashing a btree root's node: {e}"));

        b.flags.set_read_in_flight(true);

        /* we can't pass the trans to read_done() for fsck errors, so it must be unlocked */
        trans.unlock();
        node_read(&trans, b, true);

        let ret = if b.flags.read_error() {
            let _ = self.btree_node_transition_state(b, c::BtreeNodeCacheState::FREEABLE);
            Err(self.err(c::Errcode::BCH_ERR_btree_node_read_error))
        } else {
            self.btree_set_root_for_read(b);
            Ok(())
        };

        drop(held);
        ret
    }
}

// ── Scrub ────────────────────────────────────────────────────────────────

/// One of something per pointer of a key.
pub type Replicas<T> = ArrayVec<T, { c::BCH_BKEY_PTRS_MAX as usize }>;

/// A replica of a btree node, set up to be read back and checked against its
/// pointer: its buffer and bio, and a ref on its device.
pub struct ReplicaRead<'f> {
    fs:      &'f Fs,
    ca:      DevIoRef<c::DevReadRefKind>,
    bio:     BioBox<Bio, BioBuffersMapped<Bounce<'f>>>,
    /// Its pointer's, by position in the key, as ptrs_io_error's bits are
    pub ptr_bit: u32,
}

impl<'f> ReplicaRead<'f> {
    /// The replica on @dev of the node @k points to - None if there's none
    /// there to read.
    pub fn new(fs: &'f Fs, k: BkeySC<'_>, dev: u32) -> Result<Option<Self>, BchError> {
        let pick = k.pick_read_device(fs, None, dev, c::ReadFlags::hard_require_read_device)?;
        let Some(pick) = pick else { return Ok(None) };
        let ptr_bit = dev_ptr_bit(fs, k, pick.ptr.dev().into());

        let ca = fs.dev_get_ioref::<c::DevReadRefKind>(pick.ptr.dev().into())
            .ok_or_else(|| fs.err(c::Errcode::BCH_ERR_device_offline))?;

        let bytes = fs.opts.btree_node_size.get() as usize;
        let mut bio = BioBox::<Bio, BioBuffersMapped<_>>::new_with::<BchError, _>(
            ca.disk_sb.bdev, c::req_op_REQ_OP_READ, Bounce::new(fs, bytes), |b| b.init())?;
        bio.bio_mut().set_sector(pick.ptr.offset());

        Ok(Some(Self { fs, ca, bio, ptr_bit }))
    }

    /// Whether the replica, read by read_replicas() - @read, how that went - is
    /// the node @key points to. If not it's said, with @action - what's done
    /// about it.
    pub fn good(&self, read: Result<(), BchError>, key: &BtreeKeyBuf, action: &str) -> bool {
        let (fs, k) = (self.fs, key.key());
        let mut err = Printbuf::new();
        let good = match read {
            Ok(()) => node_replica_good(fs, k, self.bio.buf(), &mut err),
            Err(e) => {
                prt!(&mut err, "read error: {e}");
                false
            }
        };

        if !good {
            let ca: &Dev = &self.ca;
            let mut msg = Printbuf::new();
            prt!(&mut msg, "btree node scrub: bad replica on {ca}{action}:\n");
            btree_pos_to_text(&mut msg, fs, key.btree, key.level, k);
            prt!(&mut msg, "\n{}", err.as_str());
            crate::bch_err_ratelimited!(fs, "{}", msg.as_str());
        }
        good
    }
}

/// @reads read, all of them in flight at once - submitted before any is
/// waited on, as a task waits on one thing at a time: how each went.
pub async fn read_replicas(reads: &mut [ReplicaRead<'_>]) -> Replicas<Result<(), BchError>> {
    let waiter = waiter::Async::current().await;
    let mut submitted = Replicas::new();
    for r in reads.iter_mut() {
        submitted.push(r.bio.submit(waiter));
    }

    let mut ret = Replicas::new();
    for s in submitted {
        ret.push(s.resume_on(Resume::SPAWNED).await);
    }
    ret
}

/// One replica of the node @k points to, at @level of @btree, on @dev: read
/// asynchronously, and if it's bad, the node rewritten from another. In
/// flight in @m till it's done, and counted in its stats: the node's sectors
/// corrected, or uncorrected if the rewrite fails.
pub fn btree_node_scrub(m: &Moving<'_>, btree: Btree, level: u32, k: BkeySC<'_>, dev: u32)
    -> Result<(), BchError>
{
    // A v1 pointer carries nothing to check a replica against
    if k.arm::<c::BtreePtrV2>().is_none() {
        return Ok(());
    }

    let fs = m.trans().fs();
    let write = fs.writes.tryget_live().ok_or_else(|| fs.err(c::Errcode::BCH_ERR_erofs_no_writes))?;
    // the task holds it, and its write ref: going read-only waits for that
    let fs_ref = unsafe { BackRef::new(fs) };
    let fs = unsafe { fs_ref.get() };
    let Some(r) = ReplicaRead::new(fs, k, dev)? else { return Ok(()) };
    let key = BtreeKeyBuf::new(btree, level, k);
    let io = m.io_start(false, fs.btree_sectors());

    spawn_static(&fs.btree.read_complete_wq, move || rewrite_if_bad(write, fs_ref, key, r, io))
}

/// The inline data keys of the leaf @key points to checked against their own
/// checksums, through the btree: a node rewritten from a bset that failed its
/// checksum has good checksums after, whatever its keys hold - and every
/// replica the same keys. Each that doesn't match is said, and its sectors
/// are what's returned: data lost, with no copy to repair it from.
fn inline_data_bad(fs: &Fs, key: &BtreeKeyBuf) -> u64 {
    let k = key.key();
    let Some(min_key) = k.arm::<c::BtreePtrV2>().map(|v2| v2.v().min_key) else { return 0 };

    let trans = crate::btree_trans!(fs);
    let (mut bad, mut seen) = (0u64, None);
    let ret = BtreeIter::new(&trans, key.btree, min_key, BtreeIterFlags::ALL_SNAPSHOTS)
        .for_each_max(&trans, k.k.p, |_, _, k| {
            // a restart runs a key again
            if seen >= Some(k.k.p) {
                return Ok(());
            }
            seen = Some(k.k.p);

            let Some(Err(e)) = InlineDataRef::of(k).map(|d| d.verify()) else { return Ok(()) };
            bad += u64::from(k.k.size);

            let mut msg = Printbuf::new();
            prt!(&mut msg, "btree node scrub: inline data doesn't match its checksum, and there's no \
                            other copy of it:\n  ");
            e.to_text(&mut msg);
            prt!(&mut msg, "\n  {}", k.to_text(fs));
            crate::bch_err_ratelimited!(fs, "{}", msg.as_str());
            Ok(())
        });
    if let Err(e) = ret {
        crate::bch_err_ratelimited!(fs, "btree node scrub: checking inline data: {}", e.msg());
    }
    bad
}

async fn rewrite_if_bad(_write: WriteRef, fs: BackRef<Fs>, key: BtreeKeyBuf, mut r: ReplicaRead<'static>,
                        io: MovingIo) {
    let fs = &*fs;
    let read = read_replicas(core::slice::from_mut(&mut r)).await[0];
    let good = r.good(read, &key, ", rewriting node");
    drop(r);

    // the btree's keys, whichever replica they were read from
    if key.level == 1 && key.btree.key_types() & types_with(BkeyTypeFlags::inline_data) != 0 {
        let bad = inline_data_bad(fs, &key);
        io.stats().sectors_error_uncorrected.fetch_add(bad as i64, Relaxed);
    }

    if good {
        return;
    }

    let trans = crate::btree_trans!(fs);
    let ret = commit_do(&trans, None, CommitFlags::empty(), |t| {
        t.btree_node_rewrite_key(key.btree, key.level - 1, key.key(), CommitFlags::empty())
    });

    // ENOENT: the node was rewritten since, its bad replica with it
    if !ret.as_ref().is_err_and(|e| e.matches(c::ENOENT)) {
        let stats = io.stats();
        let n = if ret.is_ok() { &stats.sectors_error_corrected } else { &stats.sectors_error_uncorrected };
        n.fetch_add(fs.btree_sectors().into(), Relaxed);
    }

    if let Some(e) = ret.err().filter(|e| !e.matches(c::ENOENT) && !e.matches(c::EROFS)) {
        crate::bch_err_ratelimited!(fs, "btree node scrub: error rewriting node: {}", e.msg());
    }
}

impl BtreeNode {

}
