// SPDX-License-Identifier: GPL-2.0

//! Quotas: space and inode counts and limits, per uid, gid and project.
//!
//! The limits are KEY_TYPE_quota keys, in the quotas btree; the time and
//! warning limits are the superblock's quota section. Usage is counted in
//! memory, in Fs.quotas: from the inodes at mount, then as the VFS changes it -
//! and enforced there. Charged by inode, so a snapshot's inodes are charged to
//! their tree's master subvolume (see snapshot.rs).
//!
//! The VFS end - the counting and enforcement, quotactl - is the kernel's
//! only, as C's #ifdef CONFIG_BCACHEFS_QUOTA; the superblock section and the
//! keys are both builds'.

use crate::btree::bkey::{BkeySC, BkeyWhole};
use crate::c;
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::sb::io::{sb_field_ops, SbFieldOps};
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use crate::bkey_fsck_err_on;
use core::ffi::c_int;
use cstruct_macros::c_export;

const TYPES: [&str; c::QTYP_NR as usize] = ["user", "group", "project"];
const COUNTERS: [&str; c::Q_COUNTERS as usize] = ["space", "inodes"];

// ── The superblock section ───────────────────────────────────────────────

impl SbFieldOps for c::bch_sb_field_quota {
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, q: &Self) {
        for (t, name) in q.q.iter().zip(TYPES) {
            write!(out, "{name}: flags {:x}", t.flags.get());
            for (c, counter) in t.c.iter().zip(COUNTERS) {
                write!(out, " {counter} timelimit {} warnlimit {}", c.timelimit.get(), c.warnlimit.get());
            }
            writeln!(out);
        }
    }
}

#[c_export]
pub static bch_sb_field_ops_quota: c::bch_sb_field_ops = sb_field_ops::<c::bch_sb_field_quota>();

// ── Keys ─────────────────────────────────────────────────────────────────

fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let ty = v.k.k.p.inode;
    bkey_fsck_err_on!(v, ty >= c::QTYP_NR as u64, id::quota_type_invalid,
                      "invalid quota type ({ty} >= {})", c::QTYP_NR)
}

#[c_export]
pub fn bch2_quota_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    ret_to_c(validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_quota_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    let q = c::Quota::val_copy_pad(k).expect("a quota");

    for (c, counter) in q.c.iter().zip(COUNTERS) {
        write!(out, "{counter} hardlimit {} softlimit {}", c.hardlimit.get(), c.softlimit.get());
    }
}

#[cfg(CONFIG_BCACHEFS_QUOTA)]
pub use vfs::*;

#[cfg(CONFIG_BCACHEFS_QUOTA)]
mod vfs {
    use super::*;
    use crate::btree::bkey::{pos, BkeyI, POS_MIN};
    use crate::btree::iter::{commit_do, BtreeIter, BtreeIterFlags, CommitOpts, LoopFlow,
                             TransAttempt, UpdateTriggerFlags};
    use crate::errcode::{bch_errcode, ret_to_class};
    use crate::util::sync::MutexGuard;
    use crate::{bch_err_fn, bch_info, fs_inconsistent, inode};
    use crate::snapshots::snapshot;
    use core::ffi::c_uint;
    use core::fmt;
    use core::sync::atomic::Ordering;

    /// SB_RDONLY is BIT(0), a macro bindgen doesn't evaluate. A bit number,
    /// as s_flags is the kernel's c_ulong - usize - and not core's.
    const SB_RDONLY_BIT: u32 = 0;

    const QTYP_NR: usize = c::QTYP_NR as usize;
    const Q_SPC: usize = c::quota_counters::Q_SPC.0 as usize;
    const Q_INO: usize = c::quota_counters::Q_INO.0 as usize;

    /// @s's fieldmask, in hex, then @fields: one a line, names in a column 20
    /// wide - as C's to_text tabstop.
    macro_rules! fields_text {
        ($f:expr, $s:expr, $mask:ident, $($field:ident)*) => {{
            writeln!($f, "{:<20}{:x}", stringify!($mask), $s.$mask)?;
            $(writeln!($f, "{:<20}{}", stringify!($field), $s.$field)?;)*
            Ok(())
        }};
    }

    struct QcInfo<'a>(&'a c::qc_info);

    impl fmt::Display for QcInfo<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fields_text!(f, self.0, i_fieldmask,
                         i_flags i_spc_timelimit i_ino_timelimit i_rt_spc_timelimit
                         i_spc_warnlimit i_ino_warnlimit i_rt_spc_warnlimit)
        }
    }

    struct QcDqblk<'a>(&'a c::qc_dqblk);

    impl fmt::Display for QcDqblk<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fields_text!(f, self.0, d_fieldmask,
                         d_spc_hardlimit d_spc_softlimit d_ino_hardlimit d_ino_softlimit
                         d_space d_ino_count d_ino_timer d_spc_timer d_ino_warns d_spc_warns)
        }
    }

    /// As quota.h's bch_qid(), which vfs/fs.c still calls.
    pub fn qid(u: &c::bch_inode_unpacked) -> c::bch_qid {
        c::bch_qid { q: [u.bi_uid, u.bi_gid, u.bi_project.saturating_sub(1)] }
    }

    /// As quota.h's enabled_qtypes(), which C still calls.
    pub fn enabled_qtypes(fs: &Fs) -> u32 {
        (fs.opts.usrquota as u32) << c::quota_types::QTYP_USR.0 |
        (fs.opts.grpquota as u32) << c::quota_types::QTYP_GRP.0 |
        (fs.opts.prjquota as u32) << c::quota_types::QTYP_PRJ.0
    }

    fn set_qtypes(qtypes: u32) -> impl Iterator<Item = usize> {
        (0..QTYP_NR).filter(move |i| qtypes & (1 << i) != 0)
    }

    // ── Enforcement ─────────────────────────────────────────────────────

    fn ignore_hardlimit() -> bool {
        unsafe { c::capable(c::CAP_SYS_RESOURCE as c_int) }
        /*
         * C's, #if 0'd:
         *
         *	struct mem_dqinfo *info = &sb_dqopt(dquot->dq_sb)->info[dquot->dq_id.type];
         *
         *	return capable(CAP_SYS_RESOURCE) &&
         *	       (info->dqi_format->qf_fmt_id != QFMT_VFS_OLD ||
         *		!(info->dqi_flags & DQF_ROOT_SQUASH));
         */
    }

    #[derive(Clone, Copy)]
    enum QuotaMsg {
        SoftWarn,     /* Softlimit reached */
        SoftLongWarn, /* Grace time expired */
        HardWarn,     /* Hardlimit reached */

        HardBelow,    /* Usage got below inode hardlimit */
        SoftBelow,    /* Usage got below inode softlimit */
    }

    impl QuotaMsg {
        fn bit(self) -> u32 {
            1 << self as u32
        }

        /// The netlink warning for this, on @counter.
        fn nl(self, counter: usize) -> u32 {
            let spc = counter == Q_SPC;
            match self {
                QuotaMsg::HardWarn     => if spc { c::QUOTA_NL_BHARDWARN }     else { c::QUOTA_NL_IHARDWARN },
                QuotaMsg::SoftLongWarn => if spc { c::QUOTA_NL_BSOFTLONGWARN } else { c::QUOTA_NL_ISOFTLONGWARN },
                QuotaMsg::SoftWarn     => if spc { c::QUOTA_NL_BSOFTWARN }     else { c::QUOTA_NL_ISOFTWARN },
                QuotaMsg::HardBelow    => if spc { c::QUOTA_NL_BHARDBELOW }    else { c::QUOTA_NL_IHARDBELOW },
                QuotaMsg::SoftBelow    => if spc { c::QUOTA_NL_BSOFTBELOW }    else { c::QUOTA_NL_ISOFTBELOW },
            }
        }
    }

    /// The warnings an accounting call has for after it unlocks: quota type,
    /// netlink warning. Two at most from each check_limit() - a below message
    /// for each limit - and a transfer checks both counters, of each type.
    #[derive(Default)]
    struct QuotaMsgs {
        nr: usize,
        m:  [(usize, u32); QTYP_NR * c::Q_COUNTERS as usize * 2],
    }

    impl QuotaMsgs {
        fn prepare(&mut self, qtype: usize, counter: usize, msg: QuotaMsg) {
            assert!(self.nr < self.m.len());

            self.m[self.nr] = (qtype, msg.nl(counter));
            self.nr += 1;
        }

        /// Once, until usage goes back below the limit.
        fn prepare_warning(&mut self, qc: &mut c::MemquotaCounter, qtype: usize, counter: usize, msg: QuotaMsg) {
            if qc.warning_issued & msg.bit() != 0 {
                return;
            }

            qc.warning_issued |= msg.bit();
            self.prepare(qtype, counter, msg);
        }

        fn flush(&self, fs: &Fs, qid: c::bch_qid) {
            for &(qtype, msg) in &self.m[..self.nr] {
                unsafe {
                    c::rust_quota_send_warning(qtype as c_uint, qid.q[qtype], (*fs.vfs_sb).s_dev,
                                               msg as c_int)
                }
            }
        }
    }

    /// Usage @n is over @limit - 0, no limit, it never is.
    fn over(limit: u64, n: u64) -> bool {
        limit != 0 && limit < n
    }

    /// For each limit usage @n isn't over, clear its warnings - and the soft
    /// limit's grace period - so they're only set while it is, as the VFS's
    /// are. Returns the warnings cleared.
    fn clear_over(qc: &mut c::MemquotaCounter, n: u64) -> u32 {
        let mut clear = 0;

        if !over(qc.hardlimit, n) {
            clear |= QuotaMsg::HardWarn.bit();
        }
        if !over(qc.softlimit, n) {
            clear |= QuotaMsg::SoftWarn.bit() | QuotaMsg::SoftLongWarn.bit();
            qc.timer = 0;
        }

        let cleared = qc.warning_issued & clear;
        qc.warning_issued &= !clear;
        cleared
    }

    /// One quota type, locked: its counters are touched only through this.
    struct Locked<'a> {
        qtype:  usize,
        q:      &'a c::MemquotaType,
        _guard: MutexGuard<'a, ()>,
    }

    impl<'a> Locked<'a> {
        fn new(fs: &'a Fs, qtype: usize) -> Self {
            let q = fs.quotas.get(qtype);
            Locked { qtype, q, _guard: q.lock.lock() }
        }

        fn get(&self, idx: usize) -> Option<&c::Memquota> {
            // SAFETY: the lock's - this type's counters are only touched
            // under it
            self.q.table.get_raw(idx).map(|p| unsafe { p.as_ref() })
        }

        /// @idx's entry, which the caller preallocated.
        fn get_mut(&mut self, idx: usize) -> &mut c::Memquota {
            let mut p = self.q.table.get_raw(idx).expect("preallocated");
            // SAFETY: as get()'s, and &mut self
            unsafe { p.as_mut() }
        }

        fn iter_from(&self, idx: usize) -> impl Iterator<Item = (usize, &c::Memquota)> {
            // SAFETY: as get()'s
            self.q.table.iter_from_raw(idx).map(|(pos, p)| (pos, unsafe { p.as_ref() }))
        }

        /// Whether @idx may have @v more of @counter, and the warnings that
        /// go with it: as bch2_quota_check_limit().
        fn check_limit(
            &mut self,
            idx:     usize,
            counter: usize,
            v:       i64,
            mode:    c::quota_acct_mode,
            msgs:    &mut QuotaMsgs,
        ) -> Result<(), BchError> {
            let (qtype, q) = (self.qtype, self.q);
            let qc = &mut self.get_mut(idx).c[counter];
            let n = qc.v.wrapping_add(v as u64);

            assert!(n as i64 >= 0, "quota counter going negative");

            if mode == c::quota_acct_mode::KEY_TYPE_QUOTA_NOCHECK {
                return Ok(());
            }

            if v <= 0 {
                let cleared = clear_over(qc, n);

                if cleared & QuotaMsg::HardWarn.bit() != 0 {
                    msgs.prepare(qtype, counter, QuotaMsg::HardBelow);
                }
                if cleared & QuotaMsg::SoftWarn.bit() != 0 {
                    msgs.prepare(qtype, counter, QuotaMsg::SoftBelow);
                }
                return Ok(());
            }

            if over(qc.hardlimit, n) && !ignore_hardlimit() {
                msgs.prepare_warning(qc, qtype, counter, QuotaMsg::HardWarn);
                return Err(c::EDQUOT.into());
            }

            if over(qc.softlimit, n) {
                let now = unsafe { c::ktime_get_real_seconds() };

                if qc.timer == 0 {
                    qc.timer = now + q.limits[counter].timelimit.load(Ordering::Relaxed) as i64;
                    msgs.prepare_warning(qc, qtype, counter, QuotaMsg::SoftWarn);
                } else if now >= qc.timer && !ignore_hardlimit() {
                    msgs.prepare_warning(qc, qtype, counter, QuotaMsg::SoftLongWarn);
                    return Err(c::EDQUOT.into());
                }
            }

            Ok(())
        }
    }

    /// @f, with each type in @qtypes locked - in type order, as C's
    /// mutex_lock_nested() subclasses: a lock class each, quota_types.rs - and
    /// then, unlocked, the warnings it prepared for @qid.
    fn with_locks<'a>(
        fs:     &'a Fs,
        qtypes: u32,
        qid:    c::bch_qid,
        f:      impl FnOnce(&mut [Option<Locked<'a>>; QTYP_NR], &mut QuotaMsgs) -> Result<(), BchError>,
    ) -> Result<(), BchError> {
        let mut msgs = QuotaMsgs::default();
        let ret = {
            let mut locked = [None, None, None];
            for i in set_qtypes(qtypes) {
                locked[i] = Some(Locked::new(fs, i));
            }
            f(&mut locked, &mut msgs)
        };

        msgs.flush(fs, qid);
        ret
    }

    /// Charge @v of @counter to @qid, in each quota type accounted - all or
    /// none of them: as bch2_quota_acct().
    pub fn acct(
        fs:      &Fs,
        qid:     c::bch_qid,
        counter: usize,
        v:       i64,
        mode:    c::quota_acct_mode,
    ) -> Result<(), BchError> {
        let qtypes = enabled_qtypes(fs);

        for i in set_qtypes(qtypes) {
            fs.quotas.get(i).table.prealloc(qid.q[i] as usize, c::GFP_KERNEL)?;
        }

        with_locks(fs, qtypes, qid, |locked, msgs| {
            for l in locked.iter_mut().flatten() {
                l.check_limit(qid.q[l.qtype] as usize, counter, v, mode, msgs)?;
            }
            for l in locked.iter_mut().flatten() {
                let qc = &mut l.get_mut(qid.q[l.qtype] as usize).c[counter];
                qc.v = qc.v.wrapping_add(v as u64);
            }
            Ok(())
        })
    }

    #[c_export]
    pub fn bch2_quota_acct(
        c:       &Opaque<c::bch_fs>,
        qid:     c::bch_qid,
        counter: c::quota_counters,
        v:       i64,
        mode:    c::quota_acct_mode,
    ) -> c_int {
        ret_to_c(acct(Fs::from_c(c), qid, counter.0 as usize, v, mode))
    }

    /// Move an inode's charges - @space and the inode itself - from @src's
    /// ids to @dst's, in the quota types @qtypes that are accounted: as
    /// bch2_quota_transfer().
    pub fn transfer(
        fs:     &Fs,
        qtypes: u32,
        dst:    c::bch_qid,
        src:    c::bch_qid,
        space:  u64,
        mode:   c::quota_acct_mode,
    ) -> Result<(), BchError> {
        let qtypes = qtypes & enabled_qtypes(fs);

        for i in set_qtypes(qtypes) {
            let table = &fs.quotas.get(i).table;
            table.prealloc(src.q[i] as usize, c::GFP_KERNEL)?;
            table.prealloc(dst.q[i] as usize, c::GFP_KERNEL)?;
        }

        with_locks(fs, qtypes, dst, |locked, msgs| {
            for l in locked.iter_mut().flatten() {
                let d = dst.q[l.qtype] as usize;
                l.check_limit(d, Q_SPC, space as i64, mode, msgs)?;
                l.check_limit(d, Q_INO, 1, mode, msgs)?;
            }

            // __bch2_quota_transfer(): from src, then to dst - the same
            // entry, if the ids are
            for l in locked.iter_mut().flatten() {
                let (s, d) = (src.q[l.qtype] as usize, dst.q[l.qtype] as usize);
                for (counter, v) in [(Q_SPC, space), (Q_INO, 1)] {
                    let sc = &mut l.get_mut(s).c[counter];
                    assert!(v <= sc.v);
                    sc.v -= v;

                    let dc = &mut l.get_mut(d).c[counter];
                    dc.v = dc.v.checked_add(v).expect("quota counter overflow");
                }
            }
            Ok(())
        })
    }

    #[c_export]
    pub fn bch2_quota_transfer(
        c:      &Opaque<c::bch_fs>,
        qtypes: c_uint,
        dst:    c::bch_qid,
        src:    c::bch_qid,
        space:  u64,
        mode:   c::quota_acct_mode,
    ) -> c_int {
        ret_to_c(transfer(Fs::from_c(c), qtypes, dst, src, space, mode))
    }

    /// The limits in quota key @k, into memory; and from quotactl, @qdq's
    /// timers and warning counts - as __bch2_quota_set().
    fn set(fs: &Fs, k: BkeySC<'_>, qdq: Option<&c::qc_dqblk>) -> Result<(), BchError> {
        let ty = k.k.p.inode as usize;
        assert!(ty < QTYP_NR);

        if (1 << ty) & enabled_qtypes(fs) == 0 {
            return Ok(());
        }

        let Some(dq) = c::Quota::val_copy_pad(k) else {
            return Ok(());
        };

        let id = k.k.p.offset as usize;
        let mut l = Locked::new(fs, ty);
        l.q.table.prealloc(id, c::GFP_KERNEL)?;
        let mq = l.get_mut(id);

        for (m, d) in mq.c.iter_mut().zip(dq.c) {
            m.hardlimit = d.hardlimit.get();
            m.softlimit = d.softlimit.get();
        }

        if let Some(qdq) = qdq {
            let mask = qdq.d_fieldmask as u32;

            if mask & c::QC_SPC_TIMER != 0 {
                mq.c[Q_SPC].timer = qdq.d_spc_timer;
            }
            if mask & c::QC_SPC_WARNS != 0 {
                mq.c[Q_SPC].warns = qdq.d_spc_warns;
            }
            if mask & c::QC_INO_TIMER != 0 {
                mq.c[Q_INO].timer = qdq.d_ino_timer;
            }
            if mask & c::QC_INO_WARNS != 0 {
                mq.c[Q_INO].warns = qdq.d_ino_warns;
            }
        }

        // New limits, which usage may not be over - as the VFS's
        // do_set_dqblk(), without below messages
        for qc in &mut mq.c {
            let n = qc.v;
            clear_over(qc, n);
        }
        Ok(())
    }

    // ── Reading them in ─────────────────────────────────────────────────

    /// The quota section, made with the default grace period if there isn't
    /// one. Under sb_lock.
    fn sb_get_or_create_quota(sb: &mut c::bch_sb_handle) -> Option<&mut c::bch_sb_field_quota> {
        if sb.field::<c::bch_sb_field_quota>().is_some() {
            return sb.field_mut::<c::bch_sb_field_quota>();
        }

        let u64s = core::mem::size_of::<c::bch_sb_field_quota>() / core::mem::size_of::<u64>();
        let q = sb.field_resize::<c::bch_sb_field_quota>(u64s as u32)?;

        for t in &mut q.q {
            for c in &mut t.c {
                c.timelimit = (7 * 24 * 60 * 60).into();
            }
        }
        Some(q)
    }

    /// The superblock's time and warning limits, into memory. Under sb_lock.
    fn sb_quota_read(fs: &Fs) {
        let Some(sb_quota) = fs.disk_sb().field::<c::bch_sb_field_quota>() else {
            return;
        };

        for (i, t) in sb_quota.q.iter().enumerate() {
            for (l, c) in fs.quotas.get(i).limits.iter().zip(&t.c) {
                l.timelimit.store(c.timelimit.get(), Ordering::Relaxed);
                l.warnlimit.store(c.warnlimit.get(), Ordering::Relaxed);
            }
        }
    }

    /// Charge inode @k to its tree's master subvolume's ids - as
    /// bch2_fs_quota_read_inode(). Its other snapshot versions are the
    /// caller's to skip.
    fn read_inode(t: &TransAttempt<'_, '_>, k: BkeySC<'_>) -> Result<(), BchError> {
        let fs = t.fs();
        let tree = fs.snapshots.tree(k.k.p.snapshot);

        let s_t = snapshot::tree_lookup(t, tree).inspect_err(|e| {
            if e.matches(c::ENOENT) {
                fs_inconsistent!(fs, "{}: snapshot tree {tree} not found", crate::function_name!());
            }
        })?;

        let master_subvol = s_t.master_subvol.get();
        if master_subvol == 0 {
            return Ok(());
        }

        let u = match inode::find_by_inum_nowarn_trans(t, c::subvol_inum {
            subvol: master_subvol as u64,
            inum:   k.k.p.offset,
        }) {
            /*
             * Inode might be deleted in this snapshot - the easiest way to
             * handle that is to just skip it here:
             */
            Err(e) if e.matches(c::ENOENT) => return Ok(()),
            r => r?,
        };

        let _ = acct(fs, qid(&u), Q_SPC, u.bi_sectors as i64, c::quota_acct_mode::KEY_TYPE_QUOTA_NOCHECK);
        let _ = acct(fs, qid(&u), Q_INO, 1, c::quota_acct_mode::KEY_TYPE_QUOTA_NOCHECK);
        Ok(())
    }

    /// Read the quotas in: the limits, from the superblock and the quotas
    /// btree; the usage, counted from the inodes. As bch2_fs_quota_read().
    pub fn fs_quota_read(fs: &Fs) -> Result<(), BchError> {
        {
            let _lock = fs.sb_lock();
            // SAFETY: under sb_lock
            if sb_get_or_create_quota(unsafe { fs.disk_sb_mut() }).is_none() {
                return fs.throw(bch_errcode::BCH_ERR_ENOSPC_sb_quota);
            }

            sb_quota_read(fs);
        }

        bch_err_fn!(fs, read_keys(fs))
    }

    fn read_keys(fs: &Fs) -> Result<(), BchError> {
        let trans = crate::btree_trans!(fs);

        let mut iter = BtreeIter::new(&trans, c::btree_id::quotas, POS_MIN, BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |_, _, k| set(fs, k, None))?;

        // An inode is charged once, for its first snapshot version: the
        // rest are skipped
        let mut iter = BtreeIter::new(&trans, c::btree_id::inodes, POS_MIN,
                                      BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each(&trans, |t, _, k| {
            read_inode(t, k)?;
            Ok(LoopFlow::ContinueFrom(k.k.p.nosnap_successor()))
        })
    }

    #[c_export]
    pub fn bch2_fs_quota_read(c: &Opaque<c::bch_fs>) -> c_int {
        ret_to_c(fs_quota_read(Fs::from_c(c)))
    }

    // ── quotactl ────────────────────────────────────────────────────────

    /// # Safety
    /// @sb is a bcachefs superblock.
    unsafe fn sb_fs<'a>(sb: *mut c::super_block) -> &'a Fs {
        unsafe { Fs::borrow_raw((*sb).s_fs_info.cast()) }
    }

    fn sb_rdonly(sb: *mut c::super_block) -> bool {
        let flags = unsafe { (*sb).s_flags };
        flags & (1 << SB_RDONLY_BIT) != 0
    }

    /// Under sb_lock.
    fn set_sb_quotas(fs: &Fs, uflags: u32, on: bool) {
        // SAFETY: the caller's - sb_lock
        let sb = unsafe { fs.disk_sb_mut() }.sb_mut();
        if uflags & c::FS_QUOTA_UDQ_ENFD != 0 {
            sb.set_usrquota(on);
        }
        if uflags & c::FS_QUOTA_GDQ_ENFD != 0 {
            sb.set_grpquota(on);
        }
        if uflags & c::FS_QUOTA_PDQ_ENFD != 0 {
            sb.set_prjquota(on);
        }
    }

    /* Enable/disable/delete quotas for an entire filesystem: */

    fn enable(sb: *mut c::super_block, uflags: u32) -> Result<(), BchError> {
        let fs = unsafe { sb_fs(sb) };

        if sb_rdonly(sb) {
            return Err(c::EROFS.into());
        }

        /* Accounting must be enabled at mount time: */
        if uflags & (c::FS_QUOTA_UDQ_ACCT | c::FS_QUOTA_GDQ_ACCT | c::FS_QUOTA_PDQ_ACCT) != 0 {
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_quota_enable_acct);
        }

        /* Can't enable enforcement without accounting: */
        if uflags & c::FS_QUOTA_UDQ_ENFD != 0 && fs.opts.usrquota == 0 {
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_quota_enable_usrquota);
        }
        if uflags & c::FS_QUOTA_GDQ_ENFD != 0 && fs.opts.grpquota == 0 {
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_quota_enable_grpquota);
        }
        if uflags & c::FS_QUOTA_PDQ_ENFD != 0 && fs.opts.prjquota == 0 {
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_quota_enable_prjquota);
        }

        let _lock = fs.sb_lock();
        // SAFETY: under sb_lock
        if sb_get_or_create_quota(unsafe { fs.disk_sb_mut() }).is_none() {
            return bch_err_fn!(fs, fs.throw(bch_errcode::BCH_ERR_ENOSPC_sb_quota));
        }

        set_sb_quotas(fs, uflags, true);
        fs.write_super();
        Ok(())
    }

    unsafe extern "C" fn quota_enable(sb: *mut c::super_block, uflags: c_uint) -> c_int {
        ret_to_class(enable(sb, uflags))
    }

    unsafe extern "C" fn quota_disable(sb: *mut c::super_block, uflags: c_uint) -> c_int {
        let fs = unsafe { sb_fs(sb) };

        if sb_rdonly(sb) {
            return -(c::EROFS as c_int);
        }

        let _lock = fs.sb_lock();
        set_sb_quotas(fs, uflags, false);
        fs.write_super();
        0
    }

    fn remove(sb: *mut c::super_block, uflags: u32) -> Result<(), BchError> {
        let fs = unsafe { sb_fs(sb) };

        if sb_rdonly(sb) {
            return Err(c::EROFS.into());
        }

        let types = [
            (c::FS_USER_QUOTA,  fs.opts.usrquota, c::quota_types::QTYP_USR,
             bch_errcode::BCH_ERR_EINVAL_quota_remove_usrquota),
            (c::FS_GROUP_QUOTA, fs.opts.grpquota, c::quota_types::QTYP_GRP,
             bch_errcode::BCH_ERR_EINVAL_quota_remove_grpquota),
            (c::FS_PROJ_QUOTA,  fs.opts.prjquota, c::quota_types::QTYP_PRJ,
             bch_errcode::BCH_ERR_EINVAL_quota_remove_prjquota),
        ];

        for (flag, enabled, qtype, err) in types {
            if uflags & flag == 0 {
                continue;
            }
            if enabled != 0 {
                return fs.throw(err);
            }

            fs.btree_delete_range(c::btree_id::quotas, pos(qtype.0 as u64, 0),
                                  pos(qtype.0 as u64, u64::MAX), BtreeIterFlags::empty())?;
        }
        Ok(())
    }

    unsafe extern "C" fn quota_remove(sb: *mut c::super_block, uflags: c_uint) -> c_int {
        ret_to_class(remove(sb, uflags))
    }

    /// Which types are accounted, and their time and warning limits.
    unsafe extern "C" fn quota_get_state(sb: *mut c::super_block, state: *mut c::qc_state) -> c_int {
        let fs = unsafe { sb_fs(sb) };
        let state = unsafe { &mut *state };
        let qtypes = enabled_qtypes(fs);

        // SAFETY: a C struct, integers
        *state = unsafe { core::mem::zeroed() };

        for (i, s) in state.s_state.iter_mut().enumerate() {
            s.flags |= c::QCI_SYSFILE;

            if qtypes & (1 << i) == 0 {
                continue;
            }

            s.flags |= c::QCI_ACCT_ENABLED;

            let l = &fs.quotas.get(i).limits;
            s.spc_timelimit = l[Q_SPC].timelimit.load(Ordering::Relaxed);
            s.spc_warnlimit = l[Q_SPC].warnlimit.load(Ordering::Relaxed);

            s.ino_timelimit = l[Q_INO].timelimit.load(Ordering::Relaxed);
            s.ino_warnlimit = l[Q_INO].warnlimit.load(Ordering::Relaxed);
        }
        0
    }

    /// A type's time and warning limits, in the superblock.
    fn set_info(sb: *mut c::super_block, ty: c_int, info: &c::qc_info) -> Result<(), BchError> {
        let fs = unsafe { sb_fs(sb) };

        if false {
            bch_info!(fs, "setting:\n{}", QcInfo(info));
        }

        if sb_rdonly(sb) {
            return Err(c::EROFS.into());
        }

        let ty = ty as usize;
        if ty >= QTYP_NR {
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_quota_set_info_bad_type);
        }

        if (1 << ty) & enabled_qtypes(fs) == 0 {
            return Err(c::ESRCH.into());
        }

        let mask = info.i_fieldmask as u32;
        if mask & !(c::QC_SPC_TIMER | c::QC_INO_TIMER | c::QC_SPC_WARNS | c::QC_INO_WARNS) != 0 {
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_quota_set_info_bad_field);
        }

        let _lock = fs.sb_lock();
        // SAFETY: under sb_lock
        let Some(sb_quota) = sb_get_or_create_quota(unsafe { fs.disk_sb_mut() }) else {
            return bch_err_fn!(fs, fs.throw(bch_errcode::BCH_ERR_ENOSPC_sb_quota));
        };

        let q = &mut sb_quota.q[ty];
        if mask & c::QC_SPC_TIMER != 0 {
            q.c[Q_SPC].timelimit = info.i_spc_timelimit.into();
        }
        if mask & c::QC_SPC_WARNS != 0 {
            q.c[Q_SPC].warnlimit = info.i_spc_warnlimit.into();
        }
        if mask & c::QC_INO_TIMER != 0 {
            q.c[Q_INO].timelimit = info.i_ino_timelimit.into();
        }
        if mask & c::QC_INO_WARNS != 0 {
            q.c[Q_INO].warnlimit = info.i_ino_warnlimit.into();
        }

        sb_quota_read(fs);

        fs.write_super();
        Ok(())
    }

    unsafe extern "C" fn quota_set_info(sb: *mut c::super_block, ty: c_int, info: *mut c::qc_info) -> c_int {
        ret_to_class(set_info(sb, ty, unsafe { &*info }))
    }

    /* Get/set individual quotas: */

    fn dqblk(src: &c::Memquota) -> c::qc_dqblk {
        let (spc, ino) = (&src.c[Q_SPC], &src.c[Q_INO]);

        c::qc_dqblk {
            d_space:         spc.v << 9,
            d_spc_hardlimit: spc.hardlimit << 9,
            d_spc_softlimit: spc.softlimit << 9,
            d_spc_timer:     spc.timer,
            d_spc_warns:     spc.warns,

            d_ino_count:     ino.v,
            d_ino_hardlimit: ino.hardlimit,
            d_ino_softlimit: ino.softlimit,
            d_ino_timer:     ino.timer,
            d_ino_warns:     ino.warns,
            // SAFETY: a C struct, integers
            ..unsafe { core::mem::zeroed() }
        }
    }

    fn kqid_from(kqid: c::kqid) -> u32 {
        unsafe { c::from_kqid(core::ptr::addr_of_mut!(c::init_user_ns), kqid) }
    }

    unsafe extern "C" fn get_quota(sb: *mut c::super_block, kqid: c::kqid, qdq: *mut c::qc_dqblk) -> c_int {
        let fs = unsafe { sb_fs(sb) };
        let qdq = unsafe { &mut *qdq };
        let qid = kqid_from(kqid);

        let l = Locked::new(fs, kqid.type_ as usize);
        // SAFETY: a C struct, integers
        *qdq = l.get(qid as usize).map_or_else(|| unsafe { core::mem::zeroed() }, dqblk);
        0
    }

    unsafe extern "C" fn get_next_quota(sb: *mut c::super_block, kqid: *mut c::kqid,
                                        qdq: *mut c::qc_dqblk) -> c_int {
        let fs = unsafe { sb_fs(sb) };
        let kqid = unsafe { &mut *kqid };
        let ty = kqid.type_;
        let qid = kqid_from(*kqid);

        let l = Locked::new(fs, ty as usize);
        let Some((pos, mq)) = l.iter_from(qid as usize).find(|(_, mq)| **mq != c::Memquota::default()) else {
            return -(c::ENOENT as c_int);
        };

        unsafe { *qdq = dqblk(mq) };
        *kqid = unsafe { c::rust_make_kqid_current(ty as c_uint, pos as u32) };
        0
    }

    /// The new limits in @qdq onto @new_quota - the key there is, if there is
    /// one - and the update: as bch2_set_quota_trans().
    fn set_quota_trans(t: &TransAttempt<'_, '_>, new_quota: &mut BkeyI<c::Quota>, qdq: &c::qc_dqblk)
        -> Result<(), BchError>
    {
        let mut iter = BtreeIter::new(t, c::btree_id::quotas, new_quota.k.p,
                                      BtreeIterFlags::SLOTS | BtreeIterFlags::INTENT);
        let k = iter.peek_slot(t)?.expect("a slot always has a key");

        if let Some(old) = c::Quota::val_copy_pad(k) {
            new_quota.v = old;
        }

        let mask = qdq.d_fieldmask as u32;
        if mask & c::QC_SPC_SOFT != 0 {
            new_quota.v.c[Q_SPC].softlimit = (qdq.d_spc_softlimit >> 9).into();
        }
        if mask & c::QC_SPC_HARD != 0 {
            new_quota.v.c[Q_SPC].hardlimit = (qdq.d_spc_hardlimit >> 9).into();
        }

        if mask & c::QC_INO_SOFT != 0 {
            new_quota.v.c[Q_INO].softlimit = qdq.d_ino_softlimit.into();
        }
        if mask & c::QC_INO_HARD != 0 {
            new_quota.v.c[Q_INO].hardlimit = qdq.d_ino_hardlimit.into();
        }

        t.update(&iter, &t.bkey_copy(new_quota.as_ref())?, UpdateTriggerFlags::empty())
    }

    fn set_quota(sb: *mut c::super_block, kqid: c::kqid, qdq: &c::qc_dqblk) -> Result<(), BchError> {
        let fs = unsafe { sb_fs(sb) };

        if false {
            bch_info!(fs, "setting:\n{}", QcDqblk(qdq));
        }

        if sb_rdonly(sb) {
            return Err(c::EROFS.into());
        }

        let mut new_quota = BkeyI::<c::Quota>::new();
        new_quota.k.p = pos(kqid.type_ as u64, kqid_from(kqid) as u64);

        let trans = crate::btree_trans!(fs);
        commit_do(&trans, None, CommitOpts::new(), |t| set_quota_trans(t, &mut new_quota, qdq))?;
        set(fs, BkeySC::from(new_quota.as_ref()), Some(qdq))
    }

    unsafe extern "C" fn set_dqblk(sb: *mut c::super_block, kqid: c::kqid, qdq: *mut c::qc_dqblk) -> c_int {
        ret_to_class(set_quota(sb, kqid, unsafe { &*qdq }))
    }

    /// The VFS's: sb->s_qcop.
    #[c_export]
    pub static bch2_quotactl_operations: c::quotactl_ops = c::quotactl_ops {
        quota_enable:  Some(quota_enable),
        quota_disable: Some(quota_disable),
        rm_xquota:     Some(quota_remove),

        get_state:     Some(quota_get_state),
        set_info:      Some(quota_set_info),

        get_dqblk:     Some(get_quota),
        get_nextdqblk: Some(get_next_quota),
        set_dqblk:     Some(set_dqblk),
        // SAFETY: a C struct of fn pointers; the rest None
        ..unsafe { core::mem::zeroed() }
    };
}
