// SPDX-License-Identifier: GPL-2.0

//! Write points - foreground.c's: what a write allocates its sectors from,
//! and how it finds one.
//!
//! A write point holds open buckets, one per copy, that writes append to:
//! with what a write allocating from it uses, WritePointLocked, under its
//! lock, held for a write: by a LockedWritePoint, or for a C caller, by
//! bch2_alloc_sectors_req() until bch2_alloc_sectors_done() - see
//! sector_alloc.rs.
//!
//! A write names one by a specifier (WritePointSpec): a pointer to it - the
//! btree's, copygc's and reconcile's own - or a hashed value, bit 0 set, for
//! which one of write_points[0..nr] is found, or the least recently used
//! given it. Those are in a hash by the specifier each was last used for,
//! write_points_hash_lock's to change, walked without it: write points are
//! never freed, so a walk racing a move only strays into another chain -
//! bounded, and a miss falls to the least recently used, under the lock,
//! where it's looked for again. A write point's specifier is written under
//! its own lock and the hash lock, so a finder holding its lock can check it
//! still has the one it wanted.
//!
//! The write points in use grow while free space is plentiful against what
//! they strand - a bucket each - and shrink when an allocation finds it
//! isn't: too_many_writepoints().

use crate::btree::iter::TransAttempt;
use crate::c;
use crate::data::ec::head::stripe_heads_stop;
use crate::fs::Fs;
use crate::locking::Guard;
use crate::prt;
use crate::util::ffi::Opaque;
use crate::util::hash::hash_64;
use crate::util::kernel::local_clock;
use crate::util::{HumanReadable, Nsecs, Printbuf, ToText};
use core::ffi::{c_uint, c_ulong};
use cstruct_macros::c_export;

/// write_points_hash_lock, held.
type HashGuard<'a> = Guard<'a, c::mutex, (), c::WritePointsHash>;

/// A write point's lock, held.
pub(crate) type WritePointGuard<'a> = Guard<'a, c::mutex, c::WritePointLocked, c::WritePointId>;

/// A write point, as a write names it.
#[derive(Clone, Copy)]
pub(crate) enum WritePointSpec<'a> {
    Ptr(&'a c::write_point),
    Hashed(c_ulong),
}

impl c::write_point_specifier {
    /// # Safety
    /// Unless it's hashed - bit 0 set - it's a writepoint_ptr() of one of
    /// @fs's write points.
    pub(crate) unsafe fn spec(self, _fs: &Fs) -> WritePointSpec<'_> {
        match self.v & 1 {
            0 => WritePointSpec::Ptr(unsafe { &*(self.v as *const c::write_point) }),
            _ => WritePointSpec::Hashed(self.v),
        }
    }
}

// ── The hash ─────────────────────────────────────────────────────────────

impl c::bch_fs_allocator {
    fn wp_hash_slot(write_point: c_ulong) -> usize {
        hash_64(write_point as u64, c::WRITE_POINT_HASH_NR.ilog2()) as usize
    }

    /// The write point last used for @write_point, if a walk without the
    /// lock finds it: one may be missed, or moved since.
    fn wp_hash_find(&self, write_point: c_ulong) -> Option<&c::write_point> {
        let mut i = self.write_points_hash_lock.write_points_hash[Self::wp_hash_slot(write_point)].get();
        // bounded: a link followed racing a move may lead anywhere
        for _ in 0..self.write_points.len() {
            let wp = &self.write_points[i?.get()];
            if wp.alloc.lock.write_point.get() == write_point {
                return Some(wp);
            }
            i = wp.alloc.hash_next.get();
        }
        None
    }

    /// Write point @i into the hash, by its specifier.
    fn wp_hash_add(&self, g: &mut HashGuard<'_>, i: c::WritePointIdx) {
        let wp = &self.write_points[i.get()];
        let head = &g.published().write_points_hash[Self::wp_hash_slot(wp.alloc.lock.write_point.get())];
        wp.alloc.hash_next.set(head.get());
        head.set(Some(i));
    }

    /// Write point @i out of the hash. Its own link stays: a walk on it
    /// carries on.
    fn wp_hash_del(&self, g: &mut HashGuard<'_>, i: c::WritePointIdx) {
        let wp = &self.write_points[i.get()];
        let head = &g.published().write_points_hash[Self::wp_hash_slot(wp.alloc.lock.write_point.get())];
        if head.get() == Some(i) {
            head.set(wp.alloc.hash_next.get());
            return;
        }

        let mut j = head.get();
        loop {
            let prev = &self.write_points[j.expect("a write point in its hash chain").get()];
            if prev.alloc.hash_next.get() == Some(i) {
                prev.alloc.hash_next.set(wp.alloc.hash_next.get());
                return;
            }
            j = prev.alloc.hash_next.get();
        }
    }
}

// ── Finding one ──────────────────────────────────────────────────────────

impl Fs {
    /// Whether the write points in use strand more than 1/@factor of the
    /// free space: an open bucket each.
    fn too_many_writepoints(&self, factor: u64) -> bool {
        let stranded = u64::from(self.allocator.write_points_hash_lock.write_points_nr.get()) *
            u64::from(self.capacity.bucket_size_max.get());
        stranded * factor > self.usage_read_short().free
    }

    /// One more write point in use - its last specifier, which nothing may
    /// name any more, into the hash - if there's room for one.
    fn try_increase_writepoints(&self, g: &mut HashGuard<'_>) -> bool {
        let a = &self.allocator;
        let nr = a.write_points_hash_lock.write_points_nr.get();
        if nr as usize == a.write_points.len() || self.too_many_writepoints(32) {
            return false;
        }

        g.published().write_points_nr.set(nr + 1);
        a.wp_hash_add(g, c::WritePointIdx::new(nr as usize));
        true
    }

    /// One fewer write point in use, if there are too many - or if another's
    /// already dropped one since @old_nr: its buckets onto the partial list.
    /// Whether there are fewer: an allocation failed for space is worth
    /// retrying.
    pub(crate) fn try_decrease_writepoints(&self, t: &TransAttempt<'_, '_>, old_nr: c_uint) -> bool {
        let a = &self.allocator;
        let wp = {
            let mut g = a.write_points_hash_lock.lock();
            let nr = a.write_points_hash_lock.write_points_nr.get();
            if nr < old_nr {
                return true;
            }
            if nr == 1 || !self.too_many_writepoints(8) {
                return false;
            }

            g.published().write_points_nr.set(nr - 1);
            let i = c::WritePointIdx::new(nr as usize - 1);
            a.wp_hash_del(&mut g, i);
            &a.write_points[i.get()]
        };

        let mut g = wp.alloc.lock.lock_trans_norelock(t);
        g.ptrs.drain(self).for_each(|ob| self.open_bucket_free_unused(ob));
        true
    }

    /// The write point @spec names, locked: a pointer's, or for a hashed
    /// specifier the write point last used for it - or the least recently
    /// used, given it, the write points in use grown first if they may be.
    /// Waiting for its lock may drop the btree locks: the caller relocks.
    pub(crate) fn writepoint_find<'a>(&'a self, t: &TransAttempt<'_, '_>, spec: WritePointSpec<'a>)
        -> (&'a c::write_point, WritePointGuard<'a>)
    {
        let a = &self.allocator;
        let write_point = match spec {
            WritePointSpec::Ptr(wp) => return (wp, wp.alloc.lock.lock_trans_norelock(t)),
            WritePointSpec::Hashed(v) => v,
        };

        let mut found = a.wp_hash_find(write_point);
        let (wp, mut g) = loop {
            if let Some(wp) = found {
                let g = wp.alloc.lock.lock_trans_norelock(t);
                if wp.alloc.lock.write_point.get() == write_point {
                    break (wp, g);
                }
                drop(g);
                found = a.wp_hash_find(write_point);
                continue;
            }

            let nr = a.write_points_hash_lock.write_points_nr.get() as usize;
            let (oldest_idx, oldest) = a.write_points[..nr].iter().enumerate()
                .min_by_key(|(_, wp)| wp.alloc.lock.last_used.get())
                .expect("a write point in use");

            let mut og = oldest.alloc.lock.lock_trans_norelock(t);
            let mut hg = a.write_points_hash_lock.lock_trans_norelock(t);
            if oldest_idx >= a.write_points_hash_lock.write_points_nr.get() as usize ||
               self.try_increase_writepoints(&mut hg) {
                continue;
            }

            found = a.wp_hash_find(write_point);
            if found.is_some_and(|wp| !core::ptr::eq(wp, oldest)) {
                continue;
            }

            let i = c::WritePointIdx::new(oldest_idx);
            a.wp_hash_del(&mut hg, i);
            og.published().write_point.set(write_point);
            a.wp_hash_add(&mut hg, i);
            drop(hg);
            break (oldest, og);
        };

        g.published().last_used.set(local_clock());
        (wp, g)
    }
}

// ── Stopping ─────────────────────────────────────────────────────────────

impl Fs {
    /// @wp's buckets that go - see open_bucket_should_drop() - put.
    fn writepoint_stop(&self, wp: &c::write_point, ca: Option<&c::bch_dev>, ec: bool) {
        let mut g = wp.alloc.lock.lock();
        let mut keep = c::open_buckets::default();
        for ob in g.ptrs.drain(self) {
            if !self.open_bucket_should_drop(&ob, ca, ec) {
                keep.push(ob);
            }
        }
        g.ptrs = keep;
    }

    /// The open buckets that would keep writing to @ca - or with @ec, to
    /// stripes being made - or with neither, any: closed, the write points'
    /// and the partial list's, and the btree's reserve cache emptied; the
    /// stripes being made on @ca stopped.
    pub fn open_buckets_stop(&self, ca: Option<&c::bch_dev>, ec: bool) {
        let a = &self.allocator;
        for wp in a.write_points.iter().chain([&self.copygc.write_point, &a.reconcile_write_point,
                                               &a.btree_write_point]) {
            self.writepoint_stop(wp, ca, ec);
        }

        {
            let mut g = self.btree.reserve_cache.lock.lock();
            while g.nr != 0 {
                g.nr -= 1;
                let nr = g.nr as usize;
                g.data[nr].ob.drain(self).for_each(drop);
            }
        }

        self.open_buckets_partial_stop(ca, ec);
        stripe_heads_stop(self, ca);
    }
}

// ── Starting ─────────────────────────────────────────────────────────────

impl Fs {
    /// Every write point in use, each last used for its own address - which
    /// no write names, a hashed specifier having bit 0 set - and in the hash
    /// for it.
    pub(crate) fn write_points_init(&self) {
        let a = &self.allocator;
        let mut hg = a.write_points_hash_lock.lock();
        hg.published().write_points_nr.set(a.write_points.len() as c_uint);

        for (i, wp) in a.write_points.iter().enumerate() {
            {
                let mut g = wp.alloc.lock.lock();
                g.published().last_used.set(local_clock());
                g.published().write_point.set(core::ptr::from_ref(wp) as c_ulong);
            }
            a.wp_hash_add(&mut hg, c::WritePointIdx::new(i));
        }
    }
}

// ── Debug ────────────────────────────────────────────────────────────────

impl Fs {
    #[cold]
    fn write_point_to_text(&self, out: &mut Printbuf, wp: &c::write_point) {
        let g = wp.alloc.lock.lock();

        prt!(out, "{}: {} last wrote: {}", wp.alloc.lock.write_point.get(),
             HumanReadable(g.sectors_allocated << 9),
             Nsecs(local_clock().wrapping_sub(wp.alloc.lock.last_used.get())));

        for (i, t) in wp.io.writes_lock.time.iter().enumerate() {
            prt!(out, " {}: {}", c::write_point_state::from_bits(i as u32), Nsecs(t.get()));
        }
        writeln!(out);

        let mut out = out.indent(2);
        g.ptrs.iter(self).for_each(|ob| ob.to_text(&mut out));
    }

    #[cold]
    pub fn write_points_to_text(&self, out: &mut Printbuf) {
        let a = &self.allocator;
        write!(out, "Foreground write points\n");
        a.write_points.iter().for_each(|wp| self.write_point_to_text(out, wp));

        write!(out, "Copygc write point\n");
        self.write_point_to_text(out, &self.copygc.write_point);

        write!(out, "Rebalance write point\n");
        self.write_point_to_text(out, &a.reconcile_write_point);

        write!(out, "Btree write point\n");
        self.write_point_to_text(out, &a.btree_write_point);
    }
}

// ── For C ────────────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_open_buckets_stop(c: &Opaque<c::bch_fs>, ca: Option<&c::bch_dev>, ec: bool) {
    Fs::from_c(c).open_buckets_stop(ca, ec)
}

#[c_export]
pub fn bch2_reset_alloc_cursors(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    let rcu = crate::util::rcu::read_lock();
    for ca in fs.devs_rcu(&rcu) {
        ca.alloc_cursor.iter().for_each(|cursor| cursor.set(0));
    }
}
