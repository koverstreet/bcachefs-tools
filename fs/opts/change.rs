// SPDX-License-Identifier: GPL-2.0
//! Changing an option on a running filesystem: OptChange, the option change
//! lock held, and the hooks either side of the change - before, what it
//! needs, and whether it may be: the reconcile scans of what it changes the
//! io options of registered, feature bits, a device's state; after, what it
//! starts. The pre-set hooks run at mount too, for every option, with no
//! change to register: opts_hooks_pre_set().

use super::{opt_id, opt_to_text, Opt, OPTS};
use crate::c;
use crate::data::compress;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::locking::{Guard, Lock};
use crate::sb::io::SbFieldOps;
use crate::sb::members::dev_ptr;
use crate::util::printbuf::Printbuf;
use core::ffi::c_int;

impl Fs {
    /// Before setting option @id to @v, for device @ca or the filesystem,
    /// without an OptChange - offline: as OptChange::pre_set().
    pub fn opt_hook_pre_set(&self, ca: Option<&c::bch_dev>, id: opt_id, v: u64) -> Result<(), BchError> {
        opt_hook_pre_set(self, ca, 0, id, v, true)
    }
}

/// The reconcile scans an option change needs, registered before it and
/// done after: those of what it changes the io options of.
fn opt_hook_io(fs: &Fs, ca: Option<&c::bch_dev>, inum: u64, id: opt_id, v: u64, post: bool)
    -> Result<(), BchError>
{
    use c::reconcile_scan as scan;

    if !fs.flag(c::bch_fs_flags::BCH_FS_started) {
        return Ok(());
    }

    let bracket = |s: scan| if post { fs.opt_change_scan_post(s) } else { fs.opt_change_scan_pre(s) };
    let dev = || u32::from(ca.expect("a device option").dev_idx);

    match id {
        opt_id::Opt_foreground_target |
        opt_id::Opt_background_target |
        opt_id::Opt_promote_target |
        opt_id::Opt_compression |
        opt_id::Opt_background_compression |
        opt_id::Opt_data_checksum |
        opt_id::Opt_data_replicas |
        opt_id::Opt_erasure_code |
        opt_id::Opt_nocow => bracket(if inum == 0 {
            scan::fs()
        } else {
            scan::from_arm::<c::reconcile_scan_inum>(inum)
        })?,
        opt_id::Opt_metadata_target |
        opt_id::Opt_metadata_checksum |
        opt_id::Opt_metadata_replicas => bracket(scan::from_arm(c::reconcile_scan_metadata {}))?,
        opt_id::Opt_durability => {
            if !post && v > ca.expect("a device option").mi.get().durability.into() {
                fs.request_scan(scan::from_arm(c::reconcile_scan_pending {}), false)?;
            }
            bracket(scan::from_arm::<c::reconcile_scan_device>(dev()))?;
        }
        // The cap is in stripe.can_widen: existing stripes converge to it
        opt_id::Opt_ec_max_data_blocks => bracket(scan::from_arm(c::reconcile_scan_stripes {}))?,
        opt_id::Opt_label | opt_id::Opt_failure_domain => {
            // The in-memory label tree before the scan wakes: reconcile reads
            // the groups' devs masks. An error (ENOMEM) leaves it stale until
            // the next superblock swap
            if post {
                let _ = crate::bch_err_fn!(fs, c::bch_sb_field_disk_groups::to_cpu(fs, &mut fs.sb_lock()));
            }

            // Only the label is in targets: a failure domain only spreads
            // future allocations
            if id == opt_id::Opt_label {
                bracket(scan::from_arm(c::reconcile_scan_pending {}))?;
                // Data on the device may now be outside its target
                bracket(scan::from_arm::<c::reconcile_scan_device>(dev()))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Before setting option @id to @v - for device @ca, or inode @inum, or with
/// neither (0) the filesystem: what it needs, and whether it may be; with
/// @change, the scans it needs registered.
fn opt_hook_pre_set(fs: &Fs, ca: Option<&c::bch_dev>, inum: u64, id: opt_id, v: u64, change: bool)
    -> Result<(), BchError>
{
    match id {
        opt_id::Opt_state => {
            if let Some(ca) = ca {
                return crate::errcode::ret_to_result_void(unsafe {
                    c::bch2_dev_set_state(fs.as_ptr(), dev_ptr(Some(ca)), c::bch_member_state(v as u32),
                                          c::BCH_FORCE_IF_DEGRADED as c_int, core::ptr::null_mut())
                });
            }
        }
        opt_id::Opt_compression | opt_id::Opt_background_compression => {
            compress::check_set_has_compressed_data(fs, v as u8)?;
        }
        opt_id::Opt_erasure_code => {
            if v != 0 {
                fs.check_set_feature(c::bch_sb_feature::BCH_FEATURE_ec);
            }
        }
        opt_id::Opt_rotational => {
            // Offline: there's no post hook, and nothing to race with
            if let Some(ca) = ca {
                if change && v != u64::from(ca.mi.get().rotational) && !fs.flag(c::bch_fs_flags::BCH_FS_started) {
                    crate::data::reconcile::check::rotational_changed(fs, ca);
                }
            }
        }
        opt_id::Opt_casefold_disabled => {
            if v != 0 && fs.sb.features.get() & 1 << c::bch_sb_feature::BCH_FEATURE_casefolding.0 != 0 {
                crate::bch_err!(fs, "cannot mount with casefolding disabled: casefolding already in use");
                return fs.throw(bch_errcode::BCH_ERR_casefolding_in_use);
            }
        }
        _ => {}
    }

    if change {
        opt_hook_io(fs, ca, inum, id, v, false)?;
    }
    Ok(())
}

/// Each option's pre-set hook, for c->opts as they are: at mount.
pub(super) fn opts_hooks_pre_set(fs: &Fs) -> Result<(), BchError> {
    for opt in &OPTS {
        opt_hook_pre_set(fs, None, 0, opt.id, fs.opts.get(opt.id), false)?;
    }
    Ok(())
}

/// After setting option @id to @v, as opt_hook_pre_set(): logged, and what
/// the change starts.
fn opt_hook_post_set(fs: &Fs, ca: Option<&c::bch_dev>, inum: u64, id: opt_id, v: u64) {
    if fs.flag(c::bch_fs_flags::BCH_FS_started) {
        let mut buf = Printbuf::new();
        write!(buf, "opt change: {}=", id.opt().name);
        opt_to_text(&mut buf, Some(fs), None, id.opt(), v, 0);
        if let Some(ca) = ca {
            write!(buf, " dev {}", ca.dev_idx);
        }
        if inum != 0 {
            write!(buf, " inum {inum}");
        }
        // Into the journal - or, while it's stopped (read-only), into the
        // entries it starts with: bch2_fs_log_msg()'s
        // SAFETY: a C string, for "%s"
        let _ = unsafe { c::bch2_fs_log_msg(fs.as_ptr(), c"%s".as_ptr().cast(), buf.as_c_str().as_ptr()) };
    }

    let _ = opt_hook_io(fs, ca, inum, id, v, true);

    match id {
        opt_id::Opt_reconcile_enabled | opt_id::Opt_read_only => fs.reconcile_wakeup(),
        opt_id::Opt_copygc_enabled => fs.copygc_wakeup(),
        opt_id::Opt_discard => {
            if ca.is_none() {
                let mut sb = fs.sb_lock();
                for ca in fs.members() {
                    sb.fs_member_mut(ca.dev_idx.into()).set_discard(fs.opts.discard());
                }
                fs.write_super(&mut sb);
            }
        }
        opt_id::Opt_durability => {
            let state = fs.state_lock_read();
            if let Some(ca) = ca {
                if fs.flag(c::bch_fs_flags::BCH_FS_rw) && ca.is_online() &&
                   ca.mi.get().state == c::BCH_MEMBER_STATE_rw as u8 {
                    fs.dev_allocator_set_rw(ca.dev_idx.into(), true);
                    state.recalc_capacity();
                }
            }
        }
        // XXX: in the future we'll likely want to do compatible upgrades at
        // runtime as well, but right now there's nothing that does that
        opt_id::Opt_version_upgrade => {
            if v == c::BCH_VERSION_UPGRADE_incompatible.into() {
                fs.upgrade_incompat();
            }
        }
        // Online: the new value has to be live before the pass runs
        opt_id::Opt_rotational => {
            if let Some(ca) = ca {
                if fs.flag(c::bch_fs_flags::BCH_FS_started) {
                    crate::data::reconcile::check::rotational_changed(fs, ca);
                }
            }
        }
        opt_id::Opt_btree_cache_shrinker_seeks => {
            for (live, seeks) in fs.btree.cache.live.iter().zip([v, v * 4]) {
                if !live.shrink.is_null() {
                    // SAFETY: the shrinker is the cache's, live as it is
                    unsafe { (*live.shrink).seeks = seeks as _ };
                }
            }
        }
        _ => {}
    }
}

/// An option change in progress: the option change lock held, and the scans
/// its pre-set hooks request registered as in flight - the reconcile thread
/// doesn't start them (data/reconcile/scan.rs). Both are released on drop,
/// the registrations first - drop()'s, then the guard's.
///
/// c->opt_change_cookie is odd while it's held: io options read under an
/// even one are as the options are.
pub struct OptChange<'f> {
    fs:    &'f Fs,
    _lock: Guard<'f, c::mutex>,
}

impl<'f> OptChange<'f> {
    pub fn new(fs: &'f Fs) -> Self {
        let _lock = Lock::from_c(&fs.opt_change_lock).lock();
        let cookie = fs.opt_change_cookie.get();
        assert!(cookie & 1 == 0);
        // the lock makes us its one writer
        fs.opt_change_cookie.set(cookie + 1);
        OptChange { fs, _lock }
    }

    /// Before setting option @id to @v - for device @ca, or inode @inum, or
    /// with neither (0) the filesystem: records what the change needs, and
    /// fails if it may not.
    pub fn pre_set(&mut self, ca: Option<&c::bch_dev>, inum: u64, id: opt_id, v: u64) -> Result<(), BchError> {
        opt_hook_pre_set(self.fs, ca, inum, id, v, true)
    }

    pub fn post_set(&self, ca: Option<&c::bch_dev>, inum: u64, id: opt_id, v: u64) {
        opt_hook_post_set(self.fs, ca, inum, id, v)
    }

    /// Set @opt to @v - in the superblock if it's a field of it, @ca's member
    /// for a device option - and for a filesystem option in c->opts, no longer
    /// as mount set it; both under one sb_lock. Whether it changed.
    pub fn set(&self, ca: Option<&c::bch_dev>, opt: &Opt, v: u64, val: &[u8]) -> bool {
        let fs = self.fs;
        let mut sb = fs.sb_lock();

        let changed = if opt.in_sb() {
            let changed = crate::opts::opt_set_sb(&mut sb, ca.map(|ca| ca.dev_idx.into()), opt, v, Some(val));
            if changed {
                fs.write_super(&mut sb);
            }
            changed
        } else {
            assert!(ca.is_none(), "device option {} isn't a superblock option: not supported", opt.name);
            fs.opts().get(opt.id) != v
        };

        if changed && ca.is_none() {
            fs.opts_writer(&mut sb).set(opt.id, v);

            let mount_opts = &fs.mount_opts.d;
            // SAFETY: unsigned longs, which C changes with clear_bit()
            let words = unsafe {
                core::slice::from_raw_parts(mount_opts.as_ptr().cast::<core::sync::atomic::AtomicUsize>(),
                                            mount_opts.len())
            };
            crate::util::bitmap::AtomicBitmap::from_words(words).clear(opt.id.0 as usize);
        }
        changed
    }
}

impl Drop for OptChange<'_> {
    fn drop(&mut self) {
        self.fs.opt_change_scans_done();
        let cookie = self.fs.opt_change_cookie.get();
        assert!(cookie & 1 != 0);
        self.fs.opt_change_cookie.set(cookie + 1);
    }
}
