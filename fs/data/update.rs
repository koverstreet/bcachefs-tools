// SPDX-License-Identifier: GPL-2.0

//! A data update reads an extent's data and writes it somewhere new - to move
//! it, add replicas, recompress or scrub it - then replaces the old pointers
//! in the extent with the new ones (update_index.rs).
//!
//! init(), in the caller's transaction, takes everything an update holds: its
//! in-flight table entry, device refs, nocow locks, a disk reservation, and
//! the pages its bios share. exit() gives all of it back. Between them the
//! caller reads into rbio, and read_done() decides whether there's a write.
//!
//! C holds pointers into the update - the read path into rbio, the write path
//! into op - so it doesn't move after init(). It's freed an RCU grace period
//! after exit(), as in-flight table lookups may still be reading its key.
//!
//! The in-flight table allows one update per position, except that copygc
//! conflicts only with copygc: promotes and reconcile mustn't hold up bucket
//! evacuation. Inserting fails if the key is already there, so two updates
//! can't both pass the check.

use crate::alloc::capacity::DiskReservation;
use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{BkeyI, BkeyS, BkeySC, BkeyV};
use crate::btree::iter::{is_restart, lockrestart_do, trans_commit_do, BtreeIter, BtreeIterFlags, BtreeTrans,
                         CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::c::bch_errcode;
use crate::data::durability::{drop_extra_durability, drop_extra_ec_durability, extent_ptr_durability,
                               BkeyVDurability};
use crate::data::moving::Moving;
use crate::data::nocow_locking::{self, NocowKind};
use crate::data::ptrs::{brand, BkeyVPtrs, Ptrs, PtrsMut};
use crate::errcode::{ret_to_c, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::util::bio::pages;
use crate::util::backref::BackRef;
use crate::util::closure::StackClosure;
use crate::sb::members::DevRef;
use crate::util::cuckoo::{Cuckoo, Entry, RegisterError, Registration};
use crate::util::ffi::{c_str, Opaque};
use crate::util::rcu::read_lock;
use crate::util::{Printbuf, ToText};
use crate::util::log::should_print_err;
use crate::{bch_err_ratelimited, function_name, prt};
use crate::sb::counters::events;
use core::ffi::{c_int, c_uint, CStr};
use pin_init::stack_pin_init;
use core::ptr::NonNull;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{c_export, c_extern};

use c::bch_data_update_types as Type;

macro_rules! type_names {
    ($(($name:tt)),* $(,)?) => {
        /// Indexed by update type; C strings, for the journal's log entry.
        pub(crate) const TYPE_NAMES: &[&CStr] = &[$(c_str(concat!(stringify!($name), "\0")),)*];
    };
}
c::BCH_DATA_UPDATE_TYPES!(type_names);

c_extern! {
    fn __bch2_write_op_to_text(out: *mut c::printbuf, op: *mut c::bch_write_op);
    pub fn bch2_read_bio_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, rbio: *mut c::bch_read_bio);
}

/// The read failed only because the extent changed under it - the key was
/// gone on retry, or the bucket's gen moved - so no data was lost.
pub fn read_err_benign(e: BchError) -> bool {
    e.matches(bch_errcode::BCH_ERR_data_read_key_overwritten) ||
        e.matches(bch_errcode::BCH_ERR_data_read_ptr_stale_race)
}

/// Whether a failure is traced as a data_update_fail event. Routine failures
/// aren't: in-flight collisions, need_copygc, blocked and would_block, and
/// no_rw_devs for reconcile and promote, which wait on the pending list.
/// ec_alloc_failed waits there too, but its transient subtypes and
/// ec_alloc_failed_pending_race don't, and are traced.
pub fn fail_should_trace(type_: Type, e: BchError) -> bool {
    let parks = matches!(type_, Type::BCH_DATA_UPDATE_reconcile | Type::BCH_DATA_UPDATE_promote) &&
        e.matches(bch_errcode::BCH_ERR_data_update_fail_no_rw_devs);

    !(e.matches(bch_errcode::BCH_ERR_data_update_fail_in_flight) ||
      e.matches(bch_errcode::BCH_ERR_data_update_fail_need_copygc) ||
      e.matches(bch_errcode::BCH_ERR_data_update_fail_would_block) ||
      e.matches(bch_errcode::BCH_ERR_operation_blocked) ||
      (e.matches(bch_errcode::BCH_ERR_ec_alloc_failed) &&
       !e.matches(bch_errcode::BCH_ERR_ec_alloc_failed_transient)) ||
      parks)
}

#[c_export]
pub fn bch2_data_update_fail_should_trace(type_: c::bch_data_update_types, ret: c_int) -> bool {
    ret_to_result_void(ret).err().is_none_or(|e| fail_should_trace(type_, e))
}

// ── In flight ────────────────────────────────────────────────────────────

pub(crate) type InFlightTable = Cuckoo<Entry<c::data_update_key>>;

/// An update's place in the in-flight table, at its in_flight_key.
pub(crate) type InFlightReg = Registration<c::data_update_key, c::data_update_key>;

impl c::data_update_key {
    fn new(pos: c::bbpos, type_: Type) -> Self {
        c::data_update_key { btree: pos.btree, pos: pos.pos, copygc: type_ == Type::BCH_DATA_UPDATE_copygc }
    }
}

impl Fs {
    /// Whether an update in flight at @pos conflicts with a new one of @type_:
    /// a new copygc update conflicts only with copygc, anything else with any.
    pub fn data_update_in_flight(&self, pos: c::bbpos, type_: Type) -> bool {
        let key = c::data_update_key::new(pos, type_);
        self.update_table.contains(&key) ||
            (!key.copygc && self.update_table.contains(&c::data_update_key { copygc: true, ..key }))
    }

    /// Adds @m to the in-flight table, or fails with data_update_fail_in_flight.
    /// If the table can't grow, @m goes ahead untracked rather than failing.
    pub fn data_update_in_flight_add(&self, m: &mut c::data_update, pos: c::bbpos)
        -> Result<Option<InFlightReg>, BchError>
    {
        let type_ = m.opts.type_;
        if !self.data_update_in_flight(pos, type_) {
            m.in_flight_key = c::data_update_key::new(pos, type_);
            // SAFETY: an update is freed a grace period after it's done, and
            // in_flight_key is written only here, before it's inserted; the
            // table is the filesystem's, which outlives its updates
            match unsafe { Registration::register(&self.update_table, m.in_flight_key, NonNull::from(&m.in_flight_key)) } {
                Ok(r) => return Ok(Some(r)),
                Err(RegisterError::Exists) => {} // raced: another of its kind just went in
                Err(RegisterError::Alloc) => return Ok(None),
            }
        }
        self.event_inc(events::data_update_in_flight);
        self.throw(bch_errcode::BCH_ERR_data_update_fail_in_flight)
    }
}

/// What an update holds from init() to exit(), released in this order as
/// it's dropped: its in-flight entry, its nocow locks, a ref on each pointer's
/// device - what the nocow locks were worked out from, not c->devs[], which a
/// device removal may clear while the ref pins the device.
pub(crate) struct Held {
    in_flight: Option<InFlightReg>,
    nocow:     Option<nocow_locking::Locked>,
    devs:      [Option<DevRef>; c::BCH_BKEY_PTRS_MAX as usize],
}

// ── To text ──────────────────────────────────────────────────────────────

pub(crate) fn ptr_bits_to_text(out: &mut Printbuf, bits: u16, name: &str) {
    if bits != 0 {
        prt!(out, "{name} ptrs:\t{bits:b}");
        out.newline();
    }
}

impl Fs {
    #[cold]
    pub fn data_update_opts_to_text(&self, out: &mut Printbuf, io_opts: &c::bch_inode_opts,
                                    data_opts: &c::data_update_opts) {
        if out.nr_tabstops() == 0 {
            out.tabstop_push(20);
        }

        prt!(out, "{}", TYPE_NAMES[data_opts.type_ as usize].to_str().unwrap());
        out.newline();

        ptr_bits_to_text(out, data_opts.ptrs_io_error, "io error");
        ptr_bits_to_text(out, data_opts.ptrs_kill, "kill");
        ptr_bits_to_text(out, data_opts.ptrs_kill_ec, "kill ec");

        prt!(out, "target:\t");
        self.target_to_text(out, Target::decode(data_opts.target.into()));
        out.newline();

        prt!(out, "extra replicas:\t{}", data_opts.extra_replicas);
        out.newline();

        // an unset read_dev is -1
        prt!(out, "read_dev:\t{}\n", data_opts.read_dev as i32);
        prt!(out, "checksum_paranoia:\t{}\n", u8::from(data_opts.bits.checksum_paranoia()));

        prt!(out, "io path options:\t");
        crate::inode_opts::opts_to_text(out, self, io_opts);
        out.newline();
    }
}

#[cold]
#[c_export]
pub fn bch2_data_update_opts_to_text(out: &mut Printbuf, c: &Opaque<c::bch_fs>, io_opts: &c::bch_inode_opts,
                                     data_opts: &c::data_update_opts) {
    Fs::from_c(c).data_update_opts_to_text(out, io_opts, data_opts)
}

fn write_op_to_text(out: &mut Printbuf, op: &c::bch_write_op) {
    // SAFETY: read only
    unsafe { __bch2_write_op_to_text(out.as_raw(), core::ptr::from_ref(op).cast_mut()) }
}

impl ToText for c::data_update {
    #[cold]
    fn to_text(&self, out: &mut Printbuf) {
        let fs = self.op.fs();
        fs.data_update_opts_to_text(out, &self.op.opts, &self.opts);
        out.newline();

        prt!(out, "old key:\t{}", BkeySC::from(self.k.key()).to_text(fs));
        out.newline();

        prt!(out, "write: ");
        write_op_to_text(out, &self.op);
    }
}

#[cold]
#[c_export]
pub fn bch2_data_update_inflight_to_text(out: &mut Printbuf, m: &c::data_update) {
    let fs = m.op.fs();
    prt!(out, "{}", BkeySC::from(m.k.key()).to_text(fs));
    out.newline();

    let mut out = out.indent(2);
    fs.data_update_opts_to_text(&mut out, &m.op.opts, &m.opts);

    if !m.read_done.load(Relaxed) {
        prt!(out, "read:\n");
        let mut out = out.indent(2);
        // SAFETY: read only
        unsafe {
            bch2_read_bio_to_text(out.as_raw(), fs.as_ptr(), core::ptr::from_ref(&m.rbio).cast_mut())
        };
    } else {
        prt!(out, "write:\n");
        let mut out = out.indent(2);
        write_op_to_text(&mut out, &m.op);
    }
}

// ── init, read_done, exit ────────────────────────────────────────────────

c_extern! {
    fn bch2_scrub_journal_queue(c: *mut c::bch_fs, btree: c::btree_id, level: c_uint, k: *mut c::bkey_i,
                                ptr_bits: c_uint, err: c_int);
    fn bch2_data_read_sb_err(err: c_int) -> c::bch_sb_error_id;
    fn bch2_write(ws: *mut c::work_struct);
}

/// IOPRIO_PRIO_VALUE(IOPRIO_CLASS_IDLE, 0): background IO yields to everyone else's.
pub(crate) const IOPRIO_IDLE: u16 = 3 << 13;

/// Both scrub types read to check, and must never rewrite what they're checking.
pub(crate) fn is_scrub(t: Type) -> bool {
    matches!(t, Type::BCH_DATA_UPDATE_scrub | Type::BCH_DATA_UPDATE_scrub_no_repair)
}

impl c::data_update_opts {
    /// A checksummed extent with unchecksummed pointers: we mustn't apply the
    /// existing checksum to unchecksummed data, or lose the ability to tell
    /// that replicas differ. So read from the replica being rewritten, and
    /// rewrite only that one.
    fn checksummed_and_non_checksummed_handling(&mut self, fs: &Fs, k: BkeySC<'_>) {
        let Some(kp) = k.arm::<Ptrs>() else { return };

        let paranoia = self.bits.checksum_paranoia() || kp.ptrs_decoded(fs).any(|d| d.p.crc.csum_type == 0);
        self.bits.set_checksum_paranoia(paranoia);
        if !paranoia {
            return;
        }

        let mut rewrite_found = false;
        for (i, ptr) in kp.ptrs(fs).enumerate() {
            let bit = 1u16 << i;
            if bit & self.ptrs_kill != 0 && ptr.cached() == 0 {
                if !rewrite_found {
                    rewrite_found = true;
                    self.read_dev = ptr.dev().into();
                } else {
                    self.ptrs_kill &= !bit;
                }
            }
        }
    }
}

fn dev_ptr_bit(fs: &Fs, k: BkeySC<'_>, dev: u32) -> u32 {
    k.arm::<Ptrs>()
        .and_then(|k| k.ptrs(fs).position(|ptr| u32::from(ptr.dev()) == dev))
        .map_or(0, |i| 1 << i)
}

/// For an update with nothing to write: drops @k's killed pointers and extra
/// cached pointers, and commits.
fn extent_drop_ptrs(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, k: BkeySC<'_>,
                    io_opts: &c::bch_inode_opts, data_opts: &c::data_update_opts) -> Result<(), BchError> {
    let fs = t.fs();
    // an extents btree update always wants a reservation passed in
    let res = DiskReservation::new(fs);
    let mut n = t.bkey_make_mut_noupdate(k)?;

    if let Some(kp) = BkeyS::from(&mut n).arm::<PtrsMut>() {
        brand(kp, |mut kp| -> Result<(), BchError> {
            if data_opts.ptrs_kill_ec != 0 {
                let m = kp.assume(fs, data_opts.ptrs_kill_ec.into());
                drop_extra_ec_durability(t, io_opts, &mut kp, m)?;
            }
            if data_opts.ptrs_kill != 0 {
                let m = kp.assume(fs, data_opts.ptrs_kill.into());
                drop_extra_durability(t, io_opts, kp, m, false)?;
            }
            Ok(())
        })?;
    }

    // An extent left with no pointers is normalized into what it should be -
    // KEY_TYPE_error, or a discard for a cached one
    if let Some(mut kp) = BkeyS::from(&mut n).arm::<Ptrs>() {
        kp.drop_extra_cached_ptrs(fs, io_opts);
    }

    // We're not inserting through an extent iterator - an all_snapshots
    // iterator isn't one - so a deletion takes the plain key deletion path,
    // not the extent overwrite path
    if n.k().is_deleted() && !iter.iter_flags().contains(BtreeIterFlags::IS_EXTENTS) {
        n.k_mut().size = 0;
    }

    t.relock()?;
    t.update(iter, &n, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    t.commit(Some(&res), CommitFlags::NO_ENOSPC)
}

impl c::data_update {
    fn fs(&self) -> &'static Fs {
        self.op.fs()
    }

    fn key(&self) -> BkeySC<'_> {
        BkeySC::from(self.k.key())
    }

    /// The read and write bios share bvecs and pages: the write writes what
    /// the read filled in.
    fn bios_init(&mut self, io_opts: &c::bch_inode_opts, buf_bytes: u32) -> Result<(), BchError> {
        let fs = self.fs();
        let block_size = u32::from(fs.opts().block_size);
        let buf_bytes = buf_bytes.next_multiple_of(block_size);
        let nr_vecs = buf_bytes.div_ceil(crate::util::kernel::PAGE_SIZE as u32);

        let layout = core::alloc::Layout::array::<c::bio_vec>(nr_vecs as usize).map_err(|_| BchError::from(c::ENOMEM))?;
        self.bvecs = crate::util::alloc::kzalloc(layout).map_err(|_| BchError::from(c::ENOMEM))?.as_ptr().cast();

        // SAFETY: its bios, and the bvecs just made for them
        unsafe {
            c::bio_init(&mut self.rbio.bio, core::ptr::null_mut(), self.bvecs, nr_vecs as u16, c::req_op_REQ_OP_READ as _);
            c::bio_init(&mut self.op.wbio.bio, core::ptr::null_mut(), self.bvecs, nr_vecs as u16, 0);
        }
        if pages::alloc_pages(&mut self.op.wbio.bio, block_size, buf_bytes as usize, c::GFP_KERNEL).is_err() {
            self.bios_free();
            return Err(c::ENOMEM.into());
        }

        self.rbio.init(fs, *io_opts, None);
        self.rbio.bits_mut().set_data_update(1);
        self.rbio.bio.bi_iter.bi_size = buf_bytes;
        self.rbio.bio.bi_iter.bi_sector = self.key().k.start_offset();
        self.op.wbio.bio.bi_ioprio = IOPRIO_IDLE;
        Ok(())
    }

    /// The write bio's pages and the bvecs bios_init() made, freed.
    fn bios_free(&mut self) {
        // SAFETY: the pages are alloc_pages()'s, the bvecs bios_init()'s
        // kzalloc(); neither bio is in flight
        unsafe {
            pages::free_pages_pool(self.fs(), &mut self.op.wbio.bio);
            c::rust_kfree(self.bvecs.cast());
        }
        self.bvecs = core::ptr::null_mut();
    }

    fn trace(&self, ret: c_int) {
        let fs = self.fs();
        let size = u64::from(self.key().k.size);
        match ret_to_result_void(ret) {
            Ok(()) => fs.event_add_trace(events::data_update, size, |out| self.to_text(out)),
            Err(e) if e.matches(bch_errcode::BCH_ERR_data_update_done) =>
                fs.event_add_trace(events::data_update_no_io, size, |out| {
                    self.to_text(out);
                    prt!(out, "\nret:\t{e}\n");
                }),
            Err(e) if fail_should_trace(self.opts.type_, e) =>
                fs.event_add_trace(events::data_update_fail, size, |out| {
                    self.to_text(out);
                    prt!(out, "\nret:\t{e}\n");
                }),
            Err(_) => {}
        }
    }

    /// An unwritten extent has no data to move: unwritten space is allocated
    /// at the destination a piece at a time, each piece's index update done as
    /// it's allocated. Runs transactions of its own.
    fn update_unwritten_extent(&mut self, trans: &BtreeTrans<'_>) -> Result<(), BchError> {
        let fs = self.fs();
        stack_pin_init!(let cl = StackClosure::new());
        // SAFETY: the op's own buffer, which the keys stay in
        unsafe { self.op.insert_keys.init(self.op.inline_keys.as_mut_ptr()) };

        let end = self.key().k.p;
        let ret = (|| {
            while self.op.pos < end {
                let t = trans.begin();
                let mut iter = BtreeIter::new(&t, self.btree_id, self.op.pos, BtreeIterFlags::SLOTS);
                let ours = BkeySC::from(self.k.key());
                if !lockrestart_do(trans, |t| {
                    let k = iter.peek_slot(t)?.expect("a slot always has a key");
                    Ok(crate::data::ptrs::extents_match(fs, k, ours))
                })? {
                    break;
                }
                drop(iter);

                let e = self.op.insert_keys.top();
                // SAFETY: the keylist's room for the next key
                unsafe {
                    (*e).k = c::bkey { type_: c::bch_bkey_type::KEY_TYPE_extent.0 as u8, p: self.op.pos, ..c::bkey::new() }
                };

                let t = trans.begin();
                let op = &mut self.op;
                let nr = op.bits.nr_replicas().into();
                let r = t.alloc_request(op.target.into(), false, &mut op.devs_have, nr, nr,
                                        c::bch_watermark::from_bits(op.bits.watermark().into()),
                                        c::bch_write_flags::from_bits_retain(op.flags.into()), cl.as_ptr())
                    .and_then(|mut req| t.alloc_sectors(&mut req, op.write_point));
                let mut wp = match r {
                    Ok(wp) => wp,
                    Err(e) if e.matches(bch_errcode::BCH_ERR_operation_blocked) => {
                        trans.unlock();
                        cl.sync();
                        continue;
                    }
                    Err(e) => {
                        if should_print_err(&e) {
                            bch_err_ratelimited!(fs, "{}(): error allocating for an unwritten extent: {e}",
                                                 function_name!());
                        }
                        return Err(e);
                    }
                };
                let sectors = ((end.offset - op.pos.offset) as u32).min(wp.alloc.sectors_free);
                // SAFETY: the keylist's room for the key, at top()
                unsafe { (*e).k.resize(sectors) };
                wp.open_buckets_get(&mut op.open_buckets);
                // SAFETY: the op's inline keys hold an extent of any width
                unsafe { wp.append_ptrs(e, sectors) };
                drop(wp);

                op.pos.offset += u64::from(sectors);

                // SAFETY: the whole key at top()
                let ek = unsafe { &mut *BkeyI::from_raw_mut(e) };
                if let Some(mut ek) = BkeyS::from(ek).arm::<PtrsMut>() {
                    ek.ptrs_mut(fs).for_each(|ptr| ptr.set_unwritten(1));
                }
                // SAFETY: as above
                unsafe { op.insert_keys.push() };

                let r = crate::data::update_index::index_update(trans, self);
                drop(self.op.open_buckets.drain(fs));
                r?;
            }
            Ok(())
        })();

        if cl.nr_remaining() != 1 {
            trans.unlock();
            cl.sync();
        }
        ret
    }

    /// Sets the update up against @k, in @t's attempt; @iter points at @k, and
    /// a promote has none. On error everything taken has been given back, and
    /// data_update_done_* means there's nothing to read.
    ///
    /// # Safety
    /// The update is zeroed, and doesn't move until exit().
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn init(&mut self, t: &TransAttempt<'_, '_>, iter: Option<&BtreeIter<'_>>, moving: Option<&Moving<'_>>,
                   wp: c::write_point_specifier, io_opts: &c::bch_inode_opts, data_opts: c::data_update_opts,
                   btree_id: c::btree_id, k: BkeySC<'_>) -> Result<Held, BchError> {
        let fs = t.fs();
        let pos = c::bbpos {
            btree: iter.map_or(c::btree_id::extents, |i| i.btree()),
            pos:   k.k.p,
        };

        // SAFETY: the caller's: it stays where it is
        unsafe { self.k.init() };
        self.k.reassemble(k);

        self.btree_id = btree_id;
        self.opts = data_opts;
        self.ctxt = moving.map_or(core::ptr::null_mut(), Moving::raw);
        // SAFETY: a context's exit waits for what's in flight in it, and its
        // stats outlive it
        self.stats = moving.and_then(Moving::stats_opt).map(|s| unsafe { BackRef::new(s) });

        // SAFETY: an update is done before the filesystem goes
        unsafe { self.op.init(fs, *io_opts) };
        self.op.pos = self.k.key().k().start_pos();
        self.op.version = self.k.key().k().bversion;
        self.op.target = data_opts.target;
        self.op.write_point = wp;
        use c::bch_write_flags as w;
        self.op.flags |= (w::BCH_WRITE_pages_stable | w::BCH_WRITE_pages_owned | w::BCH_WRITE_data_encoded |
                          w::BCH_WRITE_move | data_opts.write_flags).bits() as u16;
        self.op.bits.set_compression_opt(io_opts.background_compression);
        let watermark = (data_opts.commit_flags.bits() & c::BCH_WATERMARK_MASK)
            .max(c::bch_watermark::BCH_WATERMARK_normal as u32);
        self.op.bits.set_watermark(watermark as u8);

        let ret = self.init_body(t, iter, io_opts, pos);

        if let Err(e) = &ret {
            if !is_restart(e) {
                self.trace(ret_to_c(Err::<(), _>(*e)));
            }
            self.op.res.put();
            self.k.exit();
        }
        ret
    }

    /// On error, Held's guards - locals till it returns - are dropped; init()
    /// gives back the rest.
    fn init_body(&mut self, t: &TransAttempt<'_, '_>, iter: Option<&BtreeIter<'_>>,
                 io_opts: &c::bch_inode_opts, pos: c::bbpos) -> Result<Held, BchError> {
        let fs = self.fs();
        let mut held = Held { in_flight: None, nocow: None, devs: Default::default() };
        let type_ = self.opts.type_;
        let k = BkeySC::from(self.k.key());

        let snapshot = k.k.p.snapshot;
        if type_ != Type::BCH_DATA_UPDATE_scrub_no_repair && snapshot != 0 && !fs.snapshots.exists(snapshot) {
            let r = match crate::snapshots::check_snapshots::check_key_has_snapshot(t, iter, k) {
                Ok(false) => Ok(()),
                // the key was deleted
                Ok(true) => t.commit(Some(&self.op.res), CommitFlags::NO_ENOSPC)
                    .and_then(|()| fs.throw(bch_errcode::BCH_ERR_data_update_fail_no_snapshot)),
                Err(e) => Err(e),
            };
            // can't repair yet, waiting on other recovery passes
            r.map_err(|e| if e.matches(bch_errcode::BCH_ERR_recovery_will_run) {
                fs.err(bch_errcode::BCH_ERR_data_update_fail_no_snapshot)
            } else {
                e
            })?;
        }

        if self.opts.extra_replicas != 0 {
            // the reservation's first charge: op.init() zeroed it
            self.op.res.add(k.k.size.into(), self.opts.extra_replicas.into(), c::bch_reservation_flags::empty())?;
        }

        if self.opts.ptrs_kill != 0 && self.op.opts.data_checksum != 0 {
            self.opts.checksummed_and_non_checksummed_handling(fs, k);
        }

        let mut buf_bytes = 0u32;
        let mut unwritten = false;
        let mut keeping = 0u32;
        if let Some(kp) = k.arm::<Ptrs>() {
            let mut bit = 1u32;
            for d in kp.ptrs_decoded(fs) {
                let p = &d.p;
                if bit & u32::from(self.opts.ptrs_kill) == 0 {
                    keeping += if bit & u32::from(self.opts.ptrs_kill_ec) != 0 {
                        fs.dev_durability(p.ptr.dev().into(), &read_lock())
                    } else {
                        extent_ptr_durability(t, p)?
                    };
                    if !self.opts.bits.no_devs_have() && p.ptr.cached() == 0 {
                        self.op.devs_have.add(p.ptr.dev());
                    }
                }

                // An encrypted extent must stay encrypted - but only its class
                // is forced, not its exact type: reconcile must be able to
                // change the MAC width, or a wide_macs change livelocks it
                let csum = c::bch_csum_type(p.crc.csum_type.into());
                if csum.is_encryption() {
                    self.op.nonce = (u32::from(p.crc.nonce) + u32::from(p.crc.offset)) as u16;
                    if !c::bch_csum_type(self.op.bits.csum_type().into()).is_encryption() {
                        self.op.bits.set_csum_type(p.crc.csum_type);
                    }
                }

                if u32::from(p.crc.compression_type) == c::bch_compression_type::BCH_COMPRESSION_TYPE_incompressible.0 {
                    self.op.bits.set_incompressible(1);
                }

                buf_bytes = buf_bytes.max(p.crc.uncompressed_size << 9);
                unwritten |= p.ptr.unwritten() != 0;
                bit <<= 1;
            }
        }

        if !is_scrub(type_) {
            // A move that only relocates keeps its old pointers if the write
            // comes up short, so a short write isn't degraded; one restoring
            // lost durability needs every copy it asks for
            let d = k.durability(t)?;
            if d.online >= io_opts.data_replicas {
                self.op.flags |= c::bch_write_flags::BCH_WRITE_replicas_best_effort.bits() as u16;
            }

            // Short of data_replicas, don't rereplicate up to it unless
            // extra_replicas asks: rereplicating is an explicit operation, so
            // users don't get an unexpected ENOSPC
            let mut nr = (i32::from(io_opts.data_replicas) - keeping as i32).max(0) as u32 +
                self.op.res.nr_replicas.get();
            if keeping == 0 {
                nr = nr.max(1);
                self.op.flags &= !(c::bch_write_flags::BCH_WRITE_cached.bits() as u16);
            }
            self.op.bits.set_nr_replicas(nr as u8);

            // No new replicas needed: the settings changed since it was
            // written
            if nr == 0 {
                // without @iter, it's a promote
                if let Some(iter) = iter {
                    extent_drop_ptrs(t, iter, k, io_opts, &self.opts)?;
                }
                return fs.throw(bch_errcode::BCH_ERR_data_update_done_no_writes_needed);
            }

            // Check that the write can allocate, rather than failing in
            // bch2_write() after a useless read: alloc_nowait (promotes), a
            // full or offline target, too little durability on it
            if type_ != Type::BCH_DATA_UPDATE_copygc {
                fs.data_update_write_feasible(io_opts, &self.opts, &self.op.devs_have, k, None)?;
            }

            held.in_flight = fs.data_update_in_flight_add(self, pos)?;
        } else if unwritten {
            return fs.throw(bch_errcode::BCH_ERR_data_update_done_unwritten);
        }

        let k = BkeySC::from(self.k.key());
        if let Some(kp) = k.arm::<Ptrs>() {
            for (i, ptr) in kp.ptrs(fs).enumerate() {
                // the error is a missing device's repair being scheduled
                held.devs[i] = fs.dev_bkey_tryget(k, ptr.dev().into()).ok().flatten();
            }
        }

        if fs.opts().nocow_enabled != 0 {
            let devs = &held.devs;
            held.nocow = Some(match nocow_locking::bkey_trylock(fs, k, devs, NocowKind::Copy) {
                Some(l) => l,
                None => {
                    let Some(moving) = self.moving() else {
                        // a promote mustn't block, or they'd pile up
                        return fs.throw(bch_errcode::BCH_ERR_nocow_lock_blocked);
                    };
                    // @t's transaction is the mover's own here. Wait for the
                    // locks, or until nothing we have in flight could release
                    // them - then block for them with the transaction unlocked
                    let locked = moving.wait_until(|| match nocow_locking::bkey_trylock(fs, k, devs, NocowKind::Copy) {
                        Some(l) => Some(Some(l)),
                        None => moving.updates_idle().then_some(None),
                    });
                    locked.unwrap_or_else(|| {
                        t.trans().unlock();
                        nocow_locking::bkey_lock(fs, Some(t.trans()), k, devs, NocowKind::Copy)
                    })
                }
            });
        }

        if unwritten {
            t.ignore_restarts(|trans| self.update_unwritten_extent(trans))?;
            return fs.throw(bch_errcode::BCH_ERR_data_update_done_unwritten);
        }

        t.trans().unlock();

        self.bios_init(io_opts, buf_bytes)?;

        self.rbio.bits_mut().set_data_update_verify_decompress(u8::from(type_ == Type::BCH_DATA_UPDATE_scrub));
        Ok(held)
    }

    /// Whether there's a write for write() to issue. There isn't for a failed
    /// read, a scrub with nothing to repair, or IO errors that leave nothing to
    /// write: then op.error is the update's result.
    pub fn read_done(&mut self) -> bool {
        let fs = self.fs();
        let mut crc = self.rbio.pick.crc;
        let type_ = self.opts.type_;

        // Before the bitrot fixup below, which clears rbio.ret. The damage is
        // recorded at btree_id, not the read path's read_pos, which for an
        // indirect extent is a reflink position
        if let Err(e) = ret_to_result_void(self.rbio.ret.into()) {
            if !read_err_benign(e) {
                if type_ == Type::BCH_DATA_UPDATE_scrub_no_repair {
                    // the journal scrub can't commit: see bch2_scrub_journal_queue()
                    let bit = dev_ptr_bit(fs, self.key(), self.rbio.pick.ptr.dev().into());
                    // SAFETY: its key, which the queue copies
                    unsafe { bch2_scrub_journal_queue(fs.as_ptr(), self.btree_id, 0, self.k.k, bit, self.rbio.ret.into()) };
                } else {
                    // SAFETY: maps an error code; no pointers
                    let err = unsafe { bch2_data_read_sb_err(self.rbio.ret.into()) };
                    let (btree, pos) = (self.btree_id, self.key().k.p);
                    let r = trans_commit_do(fs, None, CommitFlags::NO_ENOSPC,
                                            |t| crate::init::damage::record_key(t, btree, pos, err));
                    if let Err(e) = r {
                        if should_print_err(&e) {
                            bch_err_ratelimited!(fs, "{}(): error recording a data update's read error: {e}",
                                                 function_name!());
                        }
                    }
                }
            }
        }

        // A bitrotted extent gets a new checksum so it can be moved; its
        // poison bit keeps reads of it returning errors
        let poisoned = self.key().arm::<Ptrs>()
            .is_some_and(|k| k.extent_flags(fs) & (1 << c::bch_extent_flags_e::BCH_EXTENT_FLAG_poisoned.0) != 0);
        if i32::from(self.rbio.ret) == -(bch_errcode::BCH_ERR_data_read_csum_err.0 as i32) && poisoned {
            let nonce = crate::data::ptrs::extent_nonce(self.rbio.version, &crc);
            // SAFETY: the read's bio, done, its data in
            let bio = unsafe { crate::util::bio::Bio::from_raw(&self.rbio.bio) };
            crc.csum = fs.checksum_range(c::bch_csum_type(crc.csum_type.into()), nonce, bio.range());
            self.rbio.ret = 0;
        }

        if self.rbio.ret != 0 {
            self.op.error = self.rbio.ret;
            return false;
        }

        if type_ == Type::BCH_DATA_UPDATE_scrub_no_repair {
            if self.opts.ptrs_io_error != 0 {
                // SAFETY: as above
                unsafe {
                    bch2_scrub_journal_queue(fs.as_ptr(), self.btree_id, 0, self.k.k, self.opts.ptrs_io_error.into(), 0)
                };
            }
            return false;
        }

        if type_ == Type::BCH_DATA_UPDATE_scrub && self.opts.ptrs_io_error == 0 {
            return false;
        }

        if self.opts.ptrs_io_error != 0 {
            let trans = crate::btree_trans!(fs);
            let k = BkeySC::from(self.k.key());
            if let Some(kp) = k.arm::<Ptrs>() {
                let mut nr = u32::from(self.op.bits.nr_replicas());
                let mut bit = 1u32;
                for d in kp.ptrs_decoded(fs) {
                    if bit & u32::from(self.opts.ptrs_io_error) != 0 && bit & u32::from(self.opts.ptrs_kill) == 0 {
                        if let Ok(dur) = lockrestart_do(&trans, |t| extent_ptr_durability(t, &d.p)) {
                            nr += dur;
                        }
                        self.opts.ptrs_kill |= bit as u16;
                        self.op.devs_have.drop(d.p.ptr.dev());
                    }
                    bit <<= 1;
                }
                self.op.bits.set_nr_replicas(nr as u8);
            }

            if self.op.bits.nr_replicas() == 0 {
                self.op.error = ret_to_c(crate::data::update_index::index_update_nowrite(&trans, self)) as i16;
                return false;
            }
        }

        assert!(self.op.wbio.bio.bi_vcnt != 0, "a data update's write bio must own its pages");

        self.op.crc = crc;
        self.op.wbio.bio.bi_iter.bi_size = crc.compressed_size << 9;
        true
    }

    /// Issues the write read_done() set up; it completes to op.end_io().
    pub fn write(&mut self) {
        // SAFETY: its op, ready to write
        unsafe { c::closure_call(&mut self.op.cl, Some(bch2_write), core::ptr::null_mut(), core::ptr::null_mut()) };
    }

    /// Gives back what init() took: @held, the reservation, its place in its
    /// moving context's counts - whose waiters are woken after the nocow locks
    /// go - and the bios' memory.
    pub(crate) fn exit(&mut self, held: Held, ret: c_int) {
        self.trace(ret);
        drop(held);
        self.op.res.put();

        if let Some(moving) = self.moving() {
            moving.update_done(self.io_seq);
            moving.wake();
        }

        self.bios_free();
    }
}

