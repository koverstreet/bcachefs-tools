//! Reading a key's data: which of its pointers to read - pick_read_device() -
//! and the record of what failed, a device at a time, that the read paths
//! retry against: bch_io_failures, which the write paths keep too.
//!
//! A pick prefers what hasn't failed, then a checksummed copy, then - with
//! BCH_READ_soft_require_read_device - the asked-for device, then, at
//! random, the faster device: squared latencies, so the slower is still read
//! now and then and its latency stays known. An erasure coded pointer whose
//! device failed, or is offline, is read by reconstructing it from its
//! stripe.

use crate::format::BkeyType;
use crate::fs::Dev;
use super::*;
use crate::debug::params as debug_params;
use crate::errcode::bch_errcode;
use crate::util::Printbuf;
use crate::sb::errors::blk_sts_sb_err;
use crate::util::kernel::{cold_path, random_u64_below, unlikely, READ};
use crate::data_structures::array_vec::ArrayVec;
use core::ffi::c_int;
use core::ops::{Deref, DerefMut};

#[cfg(CONFIG_BCACHEFS_DEBUG)]
crate::module_param! {
    /// If >= 0, the device reads prefer after failures and checksums.
    pub(crate) static force_read_device: i32 = -1;
    perm = 0o644, desc = "";
}

// ── io failures ──────────────────────────────────────────────────────────

/// What failed on one device: an io error, a reconstruct's, and how many
/// checksum errors.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DevIoFailures {
    pub dev: u8,
    pub csum_nr: u8,
    pub ec_errcode: i16,
    pub errcode: i16,
}

/// What failed, a device at a time, with room for @N devices - and an erasure
/// coded reconstruct's message. All zeroes is empty.
#[repr(C)]
#[derive(Default)]
pub struct IoFailures<const N: usize> {
    devs: ArrayVec<DevIoFailures, N>,
    pub ec_msg: Printbuf,
}

// SAFETY: its message owns its buffer
unsafe impl<const N: usize> Send for IoFailures<N> {}

impl Deref for c::bch_io_failures {
    type Target = IoFailures<{ c::BCH_BKEY_PTRS_MAX as usize }>;

    fn deref(&self) -> &Self::Target {
        &self.failures
    }
}

impl DerefMut for c::bch_io_failures {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.failures
    }
}

impl<const N: usize> IoFailures<N> {
    pub fn devs(&self) -> &[DevIoFailures] {
        &self.devs
    }

    pub fn is_empty(&self) -> bool {
        self.devs.is_empty()
    }

    /// Forget every device's failures - not the reconstruct's message.
    pub fn reset_devs(&mut self) {
        self.devs.clear();
    }

    pub fn dev(&self, dev: u32) -> Option<&DevIoFailures> {
        self.devs.iter().find(|f| u32::from(f.dev) == dev)
    }

    /// @dev's record, a new one if it has none.
    /// Panics if there's no room for one: @N is the most devices its user can
    /// have failures on.
    pub fn dev_mut(&mut self, dev: u32) -> &mut DevIoFailures {
        if let Some(i) = self.devs.iter().position(|f| u32::from(f.dev) == dev) {
            return &mut self.devs[i];
        }

        if self.devs.try_push(DevIoFailures { dev: dev as u8, ..Default::default() }).is_err() {
            panic!("io failures of more than {N} devices, recording dev {dev}");
        }
        self.devs.last_mut().expect("just pushed")
    }

    pub fn set_errcode(&mut self, dev: u32, err: c_int) {
        self.dev_mut(dev).errcode = err as i16;
    }

    /// Record @err, reading @p: a failed reconstruct, a checksum error - those
    /// counted, retried until checksum_err_retry_nr - or an io error.
    /// bch2_mark_io_failure().
    pub fn mark(&mut self, p: &c::extent_ptr_decoded, err: BchError) {
        assert!(err.raw() != 0, "marking an io failure that isn't one");
        assert!(!err.matches(bch_errcode::BCH_ERR_transaction_restart),
                "marking a transaction restart as an io failure");

        let f = self.dev_mut(p.ptr.dev().into());
        let code = -(err.raw() as i16);

        if p.do_ec_reconstruct {
            f.ec_errcode = code;
        } else if err.raw() == bch_errcode::BCH_ERR_data_read_retry_csum_err.0 as i32 {
            f.csum_nr = f.csum_nr.saturating_add(1);
        } else {
            f.errcode = code;
        }
    }

    /// The BCH_READ_ERR_* kinds of error, for a read's error report.
    pub fn err_mask(&self) -> u32 {
        let mut errors = 0;
        for f in self.devs() {
            if f.csum_nr != 0 {
                errors |= c::BCH_READ_ERR_checksum;
            }
            if f.ec_errcode != 0 {
                errors |= c::BCH_READ_ERR_ec_reconstruct;
            }
            if f.errcode != 0 {
                errors |= if BchError::from_raw(-i32::from(f.errcode)).matches(bch_errcode::BCH_ERR_decompress) {
                    c::BCH_READ_ERR_decompression
                } else {
                    c::BCH_READ_ERR_io
                };
            }
        }
        errors
    }

    /// Count, by status, each failure that came back from the block layer.
    pub fn count_blk_errors(&self, fs: &Fs) {
        let count = |code: i16| {
            if code != 0 && BchError::from_raw(-i32::from(code)).matches(bch_errcode::BCH_ERR_blockdev_io_error) {
                fs.sb_error_count(blk_sts_sb_err(code.into()));
            }
        };
        for f in self.devs() {
            count(f.errcode);
            count(f.ec_errcode);
        }
    }

    /// Whether every failure is a device that's been removed - which said so
    /// once, when it was: bch2_io_failures_all_dev_removed(). Only for an
    /// operation that succeeded; a record with no error at all isn't one.
    pub fn all_dev_removed(&self) -> bool {
        self.devs().iter().all(|f| {
            f.csum_nr == 0 && f.ec_errcode == 0 &&
                BchError::from_raw(-i32::from(f.errcode)).matches(bch_errcode::BCH_ERR_BLK_STS_REMOVED)
        })
    }

    /// A line per device - its name, and its errors - then the reconstruct's
    /// message: bch2_io_failures_to_text().
    pub fn to_text(&self, out: &mut Printbuf, fs: &Fs) {
        {
            let mut out = out.indent(2);

            for f in self.devs() {
                out.make_room(1024);
                {
                    let rcu = read_lock();
                    match fs.dev_rcu(f.dev.into(), &rcu) {
                        Some(ca) => write!(out, "{ca}"),
                        None => write!(out, "(invalid device {})", f.dev),
                    }
                }

                let err_str = |code: i16| BchError::from_raw(-i32::from(code)).msg();
                if f.csum_nr == 0 && f.ec_errcode == 0 && f.errcode == 0 {
                    write!(out, " no error - confused");
                }
                if f.csum_nr != 0 {
                    write!(out, " checksum ({})", f.csum_nr);
                }
                if f.errcode != 0 {
                    write!(out, " io: {}", err_str(f.errcode));
                }
                if f.ec_errcode != 0 {
                    write!(out, " ec reconstruct: {}", err_str(f.ec_errcode));
                }
                out.newline();
            }
        }

        if self.ec_msg.pos != 0 {
            write!(out, "ec reconstruct:\n");
            let mut out = out.indent(2);
            out.write_bytes(self.ec_msg.as_bytes());
        }
    }
}

// ── picking a pointer ────────────────────────────────────────────────────

/// What a read from @ca costs: its read latency, or the most for no device -
/// dev_latency().
fn dev_latency(ca: Option<&Dev>) -> u64 {
    ca.map_or(i64::MAX as u64, |ca| ca.cur_latency(READ) as u64)
}

/// Whether @p1 is a better read than @p2, of latencies @l1 and @l2:
/// ptr_better(). The last choice is random.
fn ptr_better(fs: &Fs, p1: &c::extent_ptr_decoded, mut l1: u64, p2: &c::extent_ptr_decoded, mut l2: u64,
              preferred_dev: u32, flags: c::bch_read_flags) -> bool {
    if debug_params::force_reconstruct_read() {
        return p1.do_ec_reconstruct > p2.do_ec_reconstruct;
    }
    if unlikely(p1.do_ec_reconstruct || p2.do_ec_reconstruct) {
        return p1.do_ec_reconstruct < p2.do_ec_reconstruct;
    }

    if unlikely(p1.crc_retry_nr != p2.crc_retry_nr) {
        return p1.crc_retry_nr < p2.crc_retry_nr;
    }

    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    {
        let force = force_read_device.get();
        if force >= 0 {
            let (f1, f2) = (i32::from(p1.ptr.dev()) == force, i32::from(p2.ptr.dev()) == force);
            if f1 != f2 {
                return f1;
            }
        }
    }

    // prefer extents with checksums
    let (c1, c2) = (p1.crc.csum_type != 0, p2.crc.csum_type != 0);
    if unlikely(c1 != c2) {
        return c1;
    }

    if unlikely(flags.contains(c::bch_read_flags::BCH_READ_soft_require_read_device)) {
        if u32::from(p1.ptr.dev()) == preferred_dev {
            if fs.dev_rotational(p2.ptr.dev().into()) {
                return true;
            }
            l1 /= 256;
        }
        if u32::from(p2.ptr.dev()) == preferred_dev {
            if fs.dev_rotational(p1.ptr.dev().into()) {
                return false;
            }
            l2 /= 256;
        }
    }

    // at random, biased toward the faster device
    let sum = l1.wrapping_add(l2);
    sum != 0 && random_u64_below(sum) > l1
}

/// Which replica of a key to read, in whichever form it's held, of any type.
pub trait BkeyVRead: BkeyV<V: BkeyVal<State = Validated>> {
    /// Which of its pointers to read - preferring @preferred_dev, and with
    /// BCH_READ_hard_require_read_device only it - against @failed, what
    /// failed reading it so far: bch2_bkey_pick_read_device(). None if there's
    /// nothing to read: it's unwritten, or has no pointers but cached ones, or
    /// none. An error is why nothing could be picked.
    fn pick_read_device(&self, fs: &Fs, failed: Option<&c::bch_io_failures>, preferred_dev: u32,
                        flags: c::bch_read_flags) -> Result<Option<c::extent_ptr_decoded>, BchError> {
        use c::bch_errcode as e;
        let key = self.as_sc();

        if key.k.key_type() == BkeyType::error {
            return fs.throw(e::BCH_ERR_key_type_error);
        }

        let (mut have_csum_errors, mut have_io_errors, mut have_missing_devs) = (false, false, false);
        let mut have_dirty_ptrs = false;
        let mut pick: Option<(c::extent_ptr_decoded, u64)> = None;

        if let Some(k) = key.arm::<Ptrs>() {
            let mut rcu = read_lock();

            for d in k.ptrs_decoded(fs) {
                let mut p = d.p;
                have_dirty_ptrs |= p.ptr.cached() == 0;

                // an unwritten extent reads as a hole, of zeroes
                if p.ptr.unwritten() != 0 {
                    return Ok(None);
                }

                let dev = u32::from(p.ptr.dev());
                if flags.contains(c::bch_read_flags::BCH_READ_hard_require_read_device) && dev != preferred_dev {
                    continue;
                }

                // reported outside the read-side section; still not there
                let missing = unlikely(fs.dev_rcu(dev, &rcu).is_none() && dev != c::BCH_SB_MEMBER_INVALID);
                if missing {
                    drop(rcu);
                    fs.dev_missing_bkey(key.untyped(), dev)?;
                    rcu = read_lock();
                }
                let ca = if missing { None } else { fs.dev_rcu(dev, &rcu) };

                if p.ptr.cached() != 0 && ca.is_none_or(|ca| ca.dev_ptr_stale(&p.ptr, &rcu)) {
                    continue;
                }

                if let Some(f) = failed.and_then(|f| f.dev(dev)) {
                    cold_path();
                    let csum_nr = f.csum_nr;
                    p.crc_retry_nr = csum_nr;
                    p.has_ec &= f.ec_errcode == 0;

                    if ca.is_some() {
                        have_io_errors |= f.errcode != 0 || f.ec_errcode != 0;
                    }
                    have_csum_errors |= csum_nr != 0;

                    if p.has_ec && (f.errcode != 0 || csum_nr != 0) {
                        p.do_ec_reconstruct = true;
                    } else if f.errcode != 0 || csum_nr > fs.opts().checksum_err_retry_nr.get() {
                        continue;
                    }
                }

                have_missing_devs |= ca.is_some_and(|ca| !ca.is_online());

                if ca.is_none_or(|ca| !ca.is_online()) {
                    if !p.has_ec {
                        continue;
                    }
                    p.do_ec_reconstruct = true;
                }

                if debug_params::force_reconstruct_read() && p.has_ec {
                    p.do_ec_reconstruct = true;
                }

                // squared, to favour the faster device more - but not so much
                // the slower is never read, and its latency never updated
                let l = dev_latency(ca);
                let l = l.wrapping_mul(l);

                if pick.as_ref().is_none_or(|(pp, pl)| ptr_better(fs, &p, l, pp, *pl, preferred_dev, flags)) {
                    pick = Some((p, l));
                }
            }
        }

        if let Some((p, _)) = pick {
            return Ok(Some(p));
        }

        if !have_dirty_ptrs && !matches!(key.k.key_type(), BkeyType::btree_ptr | BkeyType::btree_ptr_v2) {
            return Ok(None);
        }
        if have_missing_devs || !have_dirty_ptrs {
            return fs.throw(e::BCH_ERR_no_device_to_read_from);
        }
        if have_csum_errors {
            return fs.throw(e::BCH_ERR_data_read_csum_err);
        }
        if have_io_errors {
            return fs.throw(e::BCH_ERR_data_read_io_err);
        }
        // pointers - validate makes sure of that - none to a device that's valid
        fs.throw(e::BCH_ERR_no_devices_valid)
    }
}

impl<K: BkeyV<V: BkeyVal<State = Validated>>> BkeyVRead for K {}
