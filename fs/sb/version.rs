// SPDX-License-Identifier: GPL-2.0

//! The superblock made ready for a start: its devices' last_mount, the
//! recovery passes it asks for, and the version upgrade or downgrade the
//! options and the version say - with the passes each needs run.

use crate::c;
use crate::c::bch_fs_flags as Flag;
use crate::c::bch_recovery_pass as Pass;
use crate::c::bch_version_upgrade_opts as Upgrade;
use crate::format::{Btree, MetadataVersion};
use crate::fs::{Fs, SbLockGuard};
use crate::init::passes::{passes_from_stable, passes_to_stable, prt_stable};
use crate::prt;
use crate::sb::downgrade::{ext, sb_set_downgrade, sb_set_upgrade};
use crate::sb::errors::SbErrorId;
use crate::sb::io::{check_version_downgrade, sb_upgrade, version};
use crate::types::Le64;
use crate::data_structures::bitmap::LeBitmap;
use crate::util::kernel::ktime_get_real_seconds;
use crate::util::names;
use crate::util::rcu::read_lock;
use crate::util::Printbuf;

/// Older than this, an upgrade isn't optional
const REQUIRED_UPGRADE_BELOW: u32 =
    MetadataVersion::btree_node_accounting.0;

fn check_version_upgrade(fs: &Fs, sb: &mut SbLockGuard<'_>, out: &mut Printbuf) -> bool {
    let latest_version = c::bcachefs_metadata_version_current;
    let latest_compatible = latest_version.min(version(fs.sb.version.get()).latest_compatible().0);
    let old_version = u32::from(match fs.sb.version_upgrade_complete.get() {
        0 => fs.sb.version.get(),
        v => v,
    });
    let upgrade = Upgrade(fs.opts().version_upgrade.get().into());
    let incompat = upgrade == Upgrade::BCH_VERSION_UPGRADE_incompatible;
    let mut ret = false;

    let new_version = if old_version < REQUIRED_UPGRADE_BELOW {
        if incompat || latest_compatible < REQUIRED_UPGRADE_BELOW {
            latest_version
        } else {
            latest_compatible
        }
    } else {
        match upgrade {
            Upgrade::BCH_VERSION_UPGRADE_compatible   => latest_compatible,
            Upgrade::BCH_VERSION_UPGRADE_incompatible => latest_version,
            Upgrade::BCH_VERSION_UPGRADE_none         => old_version.min(latest_version),
            _                                         => 0,
        }
    };

    if new_version > old_version {
        if old_version < REQUIRED_UPGRADE_BELOW {
            prt!(out, "Version upgrade required:\n");
        }

        if old_version != u32::from(fs.sb.version.get()) {
            prt!(out, "Version upgrade from {} to {} incomplete\n",
                 version(fs.sb.version_upgrade_complete.get()), version(fs.sb.version.get()));
        }

        let major = |v: u32| c::BCH_VERSION_MAJOR(v.into());
        prt!(out, "Doing {} version upgrade from {} to {}\n",
             if major(old_version) != major(new_version) { "incompatible" } else { "compatible" },
             version(old_version), version(new_version));

        let passes = ext(sb).recovery_passes_required[0].get();
        sb_set_upgrade(sb, old_version, new_version);
        let passes = ext(sb).recovery_passes_required[0].get() & !passes;

        if passes != 0 {
            prt!(out, "Upgrade requires recovery passes: ");
            prt_stable(out, passes);
            out.newline();
        }

        ret = true;
    }

    if new_version > u32::from(fs.sb.version_incompat_allowed.get()) && incompat {
        prt!(out, "Now allowing incompatible features up to {}, previously allowed up to {}\n",
             version(new_version), version(fs.sb.version_incompat_allowed.get()));

        ret = true;
    }

    if ret {
        sb_upgrade(fs, sb, new_version, incompat);
    }

    ret
}

/*
 * Prepare the superblock for a start: last_mount, required recovery passes,
 * version upgrade/downgrade, and what those imply. An open that will never
 * start - offline superblock edits - skips this, so its in-memory superblock
 * stays what's on disk plus the caller's edit, and writing it persists only
 * that. See __bch2_write_super().
 */
pub fn prep_for_start(fs: &Fs, out: &mut Printbuf) {
    let pass = |p: Pass| passes_to_stable(1 << p as u32);
    let mut sb = fs.sb_lock();

    let now = Le64::new(ktime_get_real_seconds() as u64);
    {
        let rcu = read_lock();
        for ca in fs.online_devs_rcu(&rcu) {
            sb.fs_member_mut(ca.dev_idx.into()).last_mount = now;
        }
    }

    if sb.sb().has_topology_errors() {
        ext(&mut sb).recovery_passes_required[0] |= pass(Pass::BCH_RECOVERY_PASS_check_topology);
    }

    let sb_passes = passes_from_stable(ext(&mut sb).recovery_passes_required[0].get());
    if sb_passes != 0 {
        prt!(out, "superblock requires following recovery passes to be run: ");
        names::prt_set::<Pass>(out, sb_passes);
        out.newline();
    }

    let btrees_lost_data = ext(&mut sb).btrees_lost_data().get();
    if btrees_lost_data != 0 {
        prt!(out, "superblock indicates damage to following btrees:  ");
        names::prt_set::<Btree>(out, btrees_lost_data);
        out.newline();
    }

    if fs.flags.get(Flag::BCH_FS_may_upgrade_downgrade) {
        if check_version_downgrade(fs, &mut sb) {
            prt!(out, "Version downgrade required");

            let passes = ext(&mut sb).recovery_passes_required[0].get();
            let minor = |v: u32| c::BCH_VERSION_MINOR(v.into()) as u32;
            sb_set_downgrade(&mut sb, minor(c::bcachefs_metadata_version_current),
                             minor(fs.sb.version.get().into()));
            let passes = ext(&mut sb).recovery_passes_required[0].get() & !passes;
            if passes != 0 {
                prt!(out, ", running recovery passes: ");
                prt_stable(out, passes);
            }
            out.newline();
        }

        check_version_upgrade(fs, &mut sb, out);
    }

    let required = passes_from_stable(ext(&mut sb).recovery_passes_required[0].get());
    let recovery_passes = fs.opts().recovery_passes.get() | required;
    fs.opts_writer(&mut sb).recovery_passes.set(recovery_passes);

    if u32::from(fs.sb.version_upgrade_complete.get()) <
       MetadataVersion::autofix_errors.0 {
        sb.sb_mut().set_error_action(c::bch_error_actions::BCH_ON_ERROR_fix_safe.0.into());
    }

    let extent_bp_shift_needed = (fs.opts().encoded_extent_max.get() >> 9).ilog2() + 1;
    if extent_bp_shift_needed > u32::from(fs.sb.extent_bp_shift.get()) {
        prt!(out, "extent_bp_shift too small: must repair backpointers\n");
        sb.sb_mut().set_extent_bp_shift(extent_bp_shift_needed.into());

        let ext = ext(&mut sb);
        ext.recovery_passes_required[0] |= pass(Pass::BCH_RECOVERY_PASS_check_extents_to_backpointers);
        ext.recovery_passes_required[0] |= pass(Pass::BCH_RECOVERY_PASS_check_backpointers_to_extents);
        let silent = LeBitmap::from_words_mut(&mut ext.errors_silent);
        silent.set(SbErrorId::backpointer_to_missing_ptr.0 as usize);
        silent.set(SbErrorId::ptr_to_missing_backpointer.0 as usize);
    }

    fs.flags.set(Flag::BCH_FS_sb_dirty, true);
}
