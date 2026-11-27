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

#![allow(non_camel_case_types)]

use crate::btree::bkey::{AnyKeySC, BkeyWhole};
use crate::format::Sb;
use crate::posix::inode::InodeUnpacked;
use crate::posix::quota_format::{SbFieldQuota, Quota, QuotaCounter, QuotaType};
use crate::util::{vstructs::{Unvalidated, Validated}, Printbuf};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::error::BkeyValidate;
use crate::sb::io::SbFieldOps;
use crate::{bkey_fsck_err_on, new_lock};
use cstruct_macros::{bch_enum, MaybeZeroable, TypeInfo};
use crate::locking::{Locked, SpinLock};
use crate::data_structures::genradix::SharedGenRadix;
use core::sync::atomic::AtomicU32;
use pin_init::{pin_data, pin_init, PinInit};
#[cfg(CONFIG_BCACHEFS_QUOTA)]
use crate::format::Btree;

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, TypeInfo)]
pub struct Qid {
    pub q: [u32; QuotaType::NR as usize],
}

#[bch_enum(closed)]
#[repr(u32)]
pub enum QuotaAcctMode {
    PREALLOC,
    WARN,
    NOCHECK,
}

/// One counter of one id's in-memory quota: its usage, its limits, and the
/// grace period's end and warnings.
#[derive(Clone, Copy, Default, PartialEq, ::pin_init::Zeroable)]
pub struct MemquotaCounter {
    pub v:              u64,
    pub hardlimit:      u64,
    pub softlimit:      u64,
    pub timer:          i64,
    pub warns:          i32,
    pub warning_issued: u32,
}

/// One id's in-memory quota - a uid's, a gid's or a project's.
#[derive(Clone, Copy, Default, PartialEq, ::pin_init::Zeroable)]
pub struct Memquota {
    pub c: [MemquotaCounter; QuotaCounter::NR as usize],
}

/// A limit from the superblock: written under sb_lock, read without a lock,
/// as C's are.
#[derive(Default)]
pub struct QuotaLimit {
    pub timelimit: AtomicU32,
    pub warnlimit: AtomicU32,
}

/// The in-memory quotas of one type, as C's struct bch_memquota_type: the
/// limits, and the counters by id - allocated without the lock, genradix
/// being safe against concurrent allocation and lookup, and touched only
/// with it held. quota.rs's guard is the way in. A spinlock, as an inode's
/// ei_quota is: a folio's dirtying charges its quota, and mustn't sleep.
#[pin_data]
pub struct MemquotaType {
    pub limits:       [QuotaLimit; QuotaCounter::NR as usize],
    #[pin]
    pub(crate) lock:  Locked<SpinLock>,
    pub(crate) table: SharedGenRadix<crate::locking::LockedBy<Memquota, (), MemquotaType>>,
}
crate::genradix_entry!(crate::locking::LockedBy<Memquota, (), MemquotaType>);

impl MemquotaType {
    fn new(lock: impl PinInit<Locked<SpinLock>>) -> impl PinInit<Self> {
        pin_init!(Self {
            limits:  Default::default(),
            lock    <- lock,
            table:   SharedGenRadix::new(),
        })
    }
}

/// The in-memory quotas, by type - c->quotas, Rust's alone: quota.rs.
#[pin_data]
pub struct Quotas {
    #[pin]
    usr: MemquotaType,
    #[pin]
    grp: MemquotaType,
    #[pin]
    prj: MemquotaType,
}

impl Quotas {
    /// No limits, and empty tables. A new_lock!() each, so a lock class
    /// each: they're taken together, in type order, as C's are with
    /// mutex_lock_nested().
    pub fn new() -> impl PinInit<Self> {
        pin_init!(Self {
            usr <- MemquotaType::new(new_lock!(())),
            grp <- MemquotaType::new(new_lock!(())),
            prj <- MemquotaType::new(new_lock!(())),
        })
    }

    /// Type @qtype's: a QuotaType.
    pub fn get(&self, qtype: usize) -> &MemquotaType {
        match qtype {
            0 => &self.usr,
            1 => &self.grp,
            2 => &self.prj,
            _ => panic!("quota type {qtype} out of range"),
        }
    }
}

const TYPES: [&str; QuotaType::NR as usize] = ["user", "group", "project"];
const COUNTERS: [&str; QuotaCounter::NR as usize] = ["space", "inodes"];

// ── The superblock section ───────────────────────────────────────────────

impl SbFieldOps for SbFieldQuota<Validated> {
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &Sb<Unvalidated>,
                q: &SbFieldQuota<Unvalidated>) {
        for (t, name) in q.q.iter().zip(TYPES) {
            write!(out, "{name}: flags {:x}", t.flags.get());
            for (c, counter) in t.c.iter().zip(COUNTERS) {
                write!(out, " {counter} timelimit {} warnlimit {}", c.timelimit.get(), c.warnlimit.get());
            }
            writeln!(out);
        }
    }
}

// ── Keys ─────────────────────────────────────────────────────────────────

impl crate::btree::bkey::BkeyOps for Quota {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        let ty = v.k.k.p.inode;
        bkey_fsck_err_on!(v, ty >= QuotaType::NR as u64, quota_type_invalid,
                          "invalid quota type ({ty} >= {})", QuotaType::NR)
    }

    #[cold]
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, k: AnyKeySC<'_, Unvalidated>) {
        let q = Quota::val_copy_pad(k).expect("a quota");

        for (c, counter) in q.c.iter().zip(COUNTERS) {
            write!(out, "{counter} hardlimit {} softlimit {}", c.hardlimit.get(), c.softlimit.get());
        }
    }
}

/// The quota types the mount options turn on.
pub fn enabled_qtypes(fs: &Fs) -> u32 {
    (fs.opts.usrquota.get() as u32) << QuotaType::QTYP_USR.0 |
    (fs.opts.grpquota.get() as u32) << QuotaType::QTYP_GRP.0 |
    (fs.opts.prjquota.get() as u32) << QuotaType::QTYP_PRJ.0
}

/// The quota types in mask @qtypes.
pub fn set_qtypes(qtypes: u32) -> impl Iterator<Item = usize> {
    (0..QuotaType::NR as usize).filter(move |i| qtypes & (1 << i) != 0)
}

/// The ids an inode's usage is charged to.
pub fn qid(u: &InodeUnpacked) -> Qid {
    Qid { q: [u.bi_uid, u.bi_gid, u.bi_project.saturating_sub(1)] }
}

#[cfg(CONFIG_BCACHEFS_QUOTA)]
pub use vfs::*;

/// The space and inode counters, as acct() takes them.
pub const Q_SPC: usize = QuotaCounter::Q_SPC.0 as usize;
pub const Q_INO: usize = QuotaCounter::Q_INO.0 as usize;

/// Without quotas, nothing's counted, and there's nothing to warn of.
#[cfg(not(CONFIG_BCACHEFS_QUOTA))]
pub struct QuotaWarnings<'f>(core::marker::PhantomData<&'f Fs>);

#[cfg(not(CONFIG_BCACHEFS_QUOTA))]
impl<'f> QuotaWarnings<'f> {
    pub fn new(_fs: &'f Fs) -> Self {
        QuotaWarnings(core::marker::PhantomData)
    }
}

#[cfg(not(CONFIG_BCACHEFS_QUOTA))]
pub fn acct(_fs: &Fs, _qid: Qid, _counter: usize, _v: i64, _mode: QuotaAcctMode,
            _w: &mut QuotaWarnings<'_>) -> Result<(), BchError> {
    Ok(())
}

#[cfg(not(CONFIG_BCACHEFS_QUOTA))]
pub fn fs_quota_read(_fs: &Fs) -> Result<(), BchError> {
    Ok(())
}

#[cfg(not(CONFIG_BCACHEFS_QUOTA))]
pub fn transfer(_fs: &Fs, _qtypes: u32, _dst: Qid, _src: Qid, _space: u64,
                _mode: QuotaAcctMode, _w: &mut QuotaWarnings<'_>) -> Result<(), BchError> {
    Ok(())
}

#[cfg(not(CONFIG_BCACHEFS_QUOTA))]
pub fn prealloc(_fs: &Fs, _qid: Qid) -> Result<(), BchError> {
    Ok(())
}

#[cfg(CONFIG_BCACHEFS_QUOTA)]
mod vfs {
    use super::*;
    use crate::c;
    use crate::init::dev::bch_sb_buf;
    use crate::snapshots::subvolume::SubvolInum;
    use crate::btree::bkey::{pos, BkeyI, BkeySC, POS_MIN};
    use crate::btree::iter::{BtreeIter, BtreeIterFlags, CommitOpts, LoopFlow,
                             TransAttempt, UpdateTriggerFlags};
    use crate::errcode::Errcode;
    use crate::locking::{ExclusiveGuard, Guard as _};
    use crate::sb::io::SbResize;
    use crate::util::ffi::c_char;
    use crate::os::kernel::ktime_get_real_seconds;
    use crate::os::quota::{Kqid as _, QuotactlOps, QuotactlVTable};
    use crate::types::CStatic;
    use crate::{bch_err_fn, bch_info, fs_inconsistent};
    use crate::posix::inode;
    use crate::snapshots::snapshot;
    use crate::vfs::super_block::SuperBlock;
    use core::fmt;
    use cstruct_macros::vtable;
    use core::sync::atomic::Ordering;

    const QTYP_NR: usize = QuotaType::NR as usize;

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

    // ── Enforcement ─────────────────────────────────────────────────────

    fn ignore_hardlimit() -> bool {
        crate::os::kernel::capable(crate::os::kernel::Capability::SysResource)
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

    /// The warnings accounting has for when no lock's held - a netlink send
    /// sleeps: quota type, id, netlink warning. Sent when it's dropped, so
    /// it's made before the locks they're found under - the quota types',
    /// and an inode's ei_quota. Two at most from each check_limit() - a below
    /// message for each limit - and a transfer checks both counters, of each
    /// type: as many as the two accountings ei_quota is held across, at most,
    /// i_sectors_acct_put()'s.
    pub struct QuotaWarnings<'f> {
        fs: &'f Fs,
        nr: usize,
        m:  [(usize, u32, u32); QTYP_NR * QuotaCounter::NR as usize * 2],
    }

    impl<'f> QuotaWarnings<'f> {
        pub fn new(fs: &'f Fs) -> Self {
            QuotaWarnings { fs, nr: 0, m: Default::default() }
        }

        fn prepare(&mut self, qtype: usize, id: usize, counter: usize, msg: QuotaMsg) {
            assert!(self.nr < self.m.len());

            self.m[self.nr] = (qtype, id as u32, msg.nl(counter));
            self.nr += 1;
        }

        /// Once, until usage goes back below the limit.
        fn prepare_warning(&mut self, qc: &mut MemquotaCounter, qtype: usize, id: usize, counter: usize,
                           msg: QuotaMsg) {
            if qc.warning_issued & msg.bit() != 0 {
                return;
            }

            qc.warning_issued |= msg.bit();
            self.prepare(qtype, id, counter, msg);
        }
    }

    impl Drop for QuotaWarnings<'_> {
        fn drop(&mut self) {
            if self.nr == 0 {
                return;
            }
            // Charged only through the VFS:
            let sb = self.fs.vfs_super().expect("quota warnings, but not mounted");
            for &(qtype, id, msg) in &self.m[..self.nr] {
                c::kqid::new(qtype as c::quota_type, id).send_warning(sb.dev(), msg as c_char);
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
    fn clear_over(qc: &mut MemquotaCounter, n: u64) -> u32 {
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
        q:      &'a MemquotaType,
        _guard: ExclusiveGuard<'a, SpinLock>,
    }

    // its type's lock, and its type: what its counters name
    impl crate::locking::Guard for Locked<'_> {
        type Name = MemquotaType;
        type Root = MemquotaType;

        fn root(&self) -> &MemquotaType {
            self.q
        }
    }

    impl crate::locking::Alone for Locked<'_> {}

    impl<'a> Locked<'a> {
        fn new(fs: &'a Fs, qtype: usize) -> Self {
            let q = fs.quotas.get(qtype);
            Locked { qtype, q, _guard: q.lock.lock() }
        }

        fn get(&self, idx: usize) -> Option<&Memquota> {
            self.q.table.get(idx).map(|e| self.get_of(e))
        }

        /// @idx's entry, which the caller preallocated.
        fn get_mut(&mut self, idx: usize) -> &mut Memquota {
            let q = self.q;
            self.get_mut_of(q.table.get(idx).expect("preallocated"))
        }

        fn iter_from(&self, idx: usize) -> impl Iterator<Item = (usize, &Memquota)> {
            self.q.table.iter_from(idx).map(move |(pos, e)| (pos, self.get_of(e)))
        }

        /// Whether @idx may have @v more of @counter, and the warnings that
        /// go with it.
        fn check_limit(
            &mut self,
            idx:     usize,
            counter: usize,
            v:       i64,
            mode:    QuotaAcctMode,
            msgs:    &mut QuotaWarnings<'_>,
        ) -> Result<(), BchError> {
            let (qtype, q) = (self.qtype, self.q);
            let qc = &mut self.get_mut(idx).c[counter];
            let n = qc.v.wrapping_add(v as u64);

            assert!(n as i64 >= 0, "quota counter going negative");

            if mode == QuotaAcctMode::NOCHECK {
                return Ok(());
            }

            if v <= 0 {
                let cleared = clear_over(qc, n);

                if cleared & QuotaMsg::HardWarn.bit() != 0 {
                    msgs.prepare(qtype, idx, counter, QuotaMsg::HardBelow);
                }
                if cleared & QuotaMsg::SoftWarn.bit() != 0 {
                    msgs.prepare(qtype, idx, counter, QuotaMsg::SoftBelow);
                }
                return Ok(());
            }

            if over(qc.hardlimit, n) && !ignore_hardlimit() {
                msgs.prepare_warning(qc, qtype, idx, counter, QuotaMsg::HardWarn);
                return Err(Errcode::EDQUOT_hardlimit.into());
            }

            if over(qc.softlimit, n) {
                let now = ktime_get_real_seconds();

                if qc.timer == 0 {
                    qc.timer = now + q.limits[counter].timelimit.load(Ordering::Relaxed) as i64;
                    msgs.prepare_warning(qc, qtype, idx, counter, QuotaMsg::SoftWarn);
                } else if now >= qc.timer && !ignore_hardlimit() {
                    msgs.prepare_warning(qc, qtype, idx, counter, QuotaMsg::SoftLongWarn);
                    return Err(Errcode::EDQUOT_softlimit_expired.into());
                }
            }

            Ok(())
        }
    }

    /// @f, with each type in @qtypes locked - in type order, as C's
    /// mutex_lock_nested() subclasses: a lock class each, QuotaType.rs.
    fn with_locks<'a>(
        fs:     &'a Fs,
        qtypes: u32,
        f:      impl FnOnce(&mut [Option<Locked<'a>>; QTYP_NR]) -> Result<(), BchError>,
    ) -> Result<(), BchError> {
        let mut locked = [None, None, None];
        for i in set_qtypes(qtypes) {
            locked[i] = Some(Locked::new(fs, i));
        }
        let ret = f(&mut locked);
        // unlocked last taken first: each is a NORECLAIM scope, which restores
        // what it found - an array drops first to last
        for l in locked.iter_mut().rev() {
            drop(l.take());
        }
        ret
    }

    /// @qid's entries, in each quota type accounted: what accounting to it
    /// under a lock - an inode's ei_quota - needs made first, as it can't
    /// allocate there.
    pub fn prealloc(fs: &Fs, qid: Qid) -> Result<(), BchError> {
        for i in set_qtypes(enabled_qtypes(fs)) {
            fs.quotas.get(i).table.prealloc(qid.q[i] as usize)?;
        }
        Ok(())
    }

    /// Charge @v of @counter to @qid, in each quota type accounted - all or
    /// none of them. Its warnings are @w's to send.
    pub fn acct(
        fs:      &Fs,
        qid:     Qid,
        counter: usize,
        v:       i64,
        mode:    QuotaAcctMode,
        w:       &mut QuotaWarnings<'_>,
    ) -> Result<(), BchError> {
        let qtypes = enabled_qtypes(fs);

        prealloc(fs, qid)?;

        with_locks(fs, qtypes, |locked| {
            for l in locked.iter_mut().flatten() {
                l.check_limit(qid.q[l.qtype] as usize, counter, v, mode, w)?;
            }
            for l in locked.iter_mut().flatten() {
                let qc = &mut l.get_mut(qid.q[l.qtype] as usize).c[counter];
                qc.v = qc.v.wrapping_add(v as u64);
            }
            Ok(())
        })
    }

    /// Move an inode's charges - @space and the inode itself - from @src's
    /// ids to @dst's, in the quota types @qtypes that are accounted. Its
    /// warnings are @w's to send.
    pub fn transfer(
        fs:     &Fs,
        qtypes: u32,
        dst:    Qid,
        src:    Qid,
        space:  u64,
        mode:   QuotaAcctMode,
        w:      &mut QuotaWarnings<'_>,
    ) -> Result<(), BchError> {
        let qtypes = qtypes & enabled_qtypes(fs);

        prealloc(fs, src)?;
        prealloc(fs, dst)?;

        with_locks(fs, qtypes, |locked| {
            for l in locked.iter_mut().flatten() {
                let d = dst.q[l.qtype] as usize;
                l.check_limit(d, Q_SPC, space as i64, mode, w)?;
                l.check_limit(d, Q_INO, 1, mode, w)?;
            }

            // From src, then to dst - the same
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

    /// The limits in quota key @k, into memory; and from quotactl, @qdq's
    /// timers and warning counts.
    fn set(fs: &Fs, k: BkeySC<'_>, qdq: Option<&c::qc_dqblk>) -> Result<(), BchError> {
        let ty = k.k.p.inode as usize;
        assert!(ty < QTYP_NR);

        if (1 << ty) & enabled_qtypes(fs) == 0 {
            return Ok(());
        }

        let Some(dq) = Quota::val_copy_pad(k) else {
            return Ok(());
        };

        let id = k.k.p.offset as usize;
        fs.quotas.get(ty).table.prealloc(id)?;
        let mut l = Locked::new(fs, ty);
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
    /// one.
    fn sb_get_or_create_quota(sb: &mut (impl SbResize + ?Sized)) -> Option<&mut SbFieldQuota<Validated>> {
        if sb.field::<SbFieldQuota>().is_some() {
            return sb.field_mut::<SbFieldQuota>();
        }

        let u64s = core::mem::size_of::<SbFieldQuota>() / core::mem::size_of::<u64>();
        let q = sb.field_resize::<SbFieldQuota>(u64s as u32)?;

        for t in &mut q.q {
            for c in &mut t.c {
                c.timelimit = (7 * 24 * 60 * 60).into();
            }
        }
        Some(q)
    }

    /// The superblock's time and warning limits, into memory.
    fn sb_quota_read(fs: &Fs, sb: &bch_sb_buf) {
        let Some(sb_quota) = sb.field::<SbFieldQuota>() else {
            return;
        };

        for (i, t) in sb_quota.q.iter().enumerate() {
            for (l, c) in fs.quotas.get(i).limits.iter().zip(&t.c) {
                l.timelimit.store(c.timelimit.get(), Ordering::Relaxed);
                l.warnlimit.store(c.warnlimit.get(), Ordering::Relaxed);
            }
        }
    }

    /// Charge inode @k to its tree's master subvolume's ids. Its other
    /// snapshot versions are the caller's to skip.
    fn read_inode(t: &TransAttempt<'_, '_>, k: BkeySC<'_>) -> Result<(), BchError> {
        let fs = t.fs();
        let tree = fs.snapshots.tree(k.k.p.snapshot);

        let s_t = snapshot::tree_lookup(t, tree).inspect_err(|e| {
            if e.matches(c::ENOENT) {
                fs_inconsistent!(fs, "{}: snapshot tree {tree} not found", crate::here_str!());
            }
        })?;

        let master_subvol = s_t.master_subvol.get();
        if master_subvol == 0 {
            return Ok(());
        }

        let u = match inode::find_by_inum_nowarn_trans(t, SubvolInum {
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

        // NOCHECK: nothing to warn of
        let w = &mut QuotaWarnings::new(fs);
        let _ = acct(fs, qid(&u), Q_SPC, u.bi_sectors as i64, QuotaAcctMode::NOCHECK, w);
        let _ = acct(fs, qid(&u), Q_INO, 1, QuotaAcctMode::NOCHECK, w);
        Ok(())
    }

    /// Read the quotas in: the limits, from the superblock and the quotas
    /// btree; the usage, counted from the inodes.
    pub fn fs_quota_read(fs: &Fs) -> Result<(), BchError> {
        {
            let mut sb = fs.sb_lock();
            if sb_get_or_create_quota(&mut fs.sb_resize(&mut sb)).is_none() {
                return fs.throw(Errcode::ENOSPC_sb_quota);
            }

            sb_quota_read(fs, &sb);
        }

        bch_err_fn!(fs, read_keys(fs))
    }

    fn read_keys(fs: &Fs) -> Result<(), BchError> {
        let trans = crate::btree_trans!(fs);

        let mut iter = BtreeIter::new(&trans, Btree::quotas, POS_MIN, BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |_, _, k| set(fs, k, None))?;

        // An inode is charged once, for its first snapshot version: the
        // rest are skipped
        let mut iter = BtreeIter::new(&trans, Btree::inodes, POS_MIN,
                                      BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each(&trans, |t, _, k| {
            read_inode(t, k)?;
            Ok(LoopFlow::ContinueFrom(k.k.p.nosnap_successor()))
        })
    }

    // ── quotactl ────────────────────────────────────────────────────────

    fn set_sb_quotas(sb: &mut bch_sb_buf, uflags: u32, on: bool) {
        let sb = sb.sb_mut();
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

    fn enable(fs: &Fs, rdonly: bool, uflags: u32) -> Result<(), BchError> {
        if rdonly {
            return fs.throw(Errcode::erofs_quota_enable);
        }

        /* Accounting must be enabled at mount time: */
        if uflags & (c::FS_QUOTA_UDQ_ACCT | c::FS_QUOTA_GDQ_ACCT | c::FS_QUOTA_PDQ_ACCT) != 0 {
            return fs.throw(Errcode::EINVAL_quota_enable_acct);
        }

        /* Can't enable enforcement without accounting: */
        if uflags & c::FS_QUOTA_UDQ_ENFD != 0 && fs.opts.usrquota.get() == 0 {
            return fs.throw(Errcode::EINVAL_quota_enable_usrquota);
        }
        if uflags & c::FS_QUOTA_GDQ_ENFD != 0 && fs.opts.grpquota.get() == 0 {
            return fs.throw(Errcode::EINVAL_quota_enable_grpquota);
        }
        if uflags & c::FS_QUOTA_PDQ_ENFD != 0 && fs.opts.prjquota.get() == 0 {
            return fs.throw(Errcode::EINVAL_quota_enable_prjquota);
        }

        let mut sb = fs.sb_lock();
        if sb_get_or_create_quota(&mut fs.sb_resize(&mut sb)).is_none() {
            return bch_err_fn!(fs, fs.throw(Errcode::ENOSPC_sb_quota));
        }

        set_sb_quotas(&mut sb, uflags, true);
        fs.write_super(&mut sb);
        Ok(())
    }

    fn disable(fs: &Fs, rdonly: bool, uflags: u32) -> Result<(), BchError> {
        if rdonly {
            return fs.throw(Errcode::erofs_quota_disable);
        }

        let mut sb = fs.sb_lock();
        set_sb_quotas(&mut sb, uflags, false);
        fs.write_super(&mut sb);
        Ok(())
    }

    fn remove(fs: &Fs, rdonly: bool, uflags: u32) -> Result<(), BchError> {
        if rdonly {
            return fs.throw(Errcode::erofs_quota_remove);
        }

        let types = [
            (c::FS_USER_QUOTA,  fs.opts.usrquota.get(), QuotaType::QTYP_USR,
             Errcode::EINVAL_quota_remove_usrquota),
            (c::FS_GROUP_QUOTA, fs.opts.grpquota.get(), QuotaType::QTYP_GRP,
             Errcode::EINVAL_quota_remove_grpquota),
            (c::FS_PROJ_QUOTA,  fs.opts.prjquota.get(), QuotaType::QTYP_PRJ,
             Errcode::EINVAL_quota_remove_prjquota),
        ];

        for (flag, enabled, qtype, err) in types {
            if uflags & flag == 0 {
                continue;
            }
            if enabled != 0 {
                return fs.throw(err);
            }

            fs.btree_delete_range(Btree::quotas, pos(qtype.0 as u64, 0),
                                  pos(qtype.0 as u64, u64::MAX), BtreeIterFlags::empty(),
                                  UpdateTriggerFlags::empty())?;
        }
        Ok(())
    }

    fn get_state(fs: &Fs, state: &mut c::qc_state) {
        let qtypes = enabled_qtypes(fs);

        *state = ::pin_init::zeroed();

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
    }

    /// A type's time and warning limits, in the superblock.
    fn set_info(fs: &Fs, rdonly: bool, ty: i32, info: &c::qc_info) -> Result<(), BchError> {
        if false {
            bch_info!(fs, "setting:\n{}", QcInfo(info));
        }

        if rdonly {
            return fs.throw(Errcode::erofs_quota_set_info);
        }

        let ty = ty as usize;
        if ty >= QTYP_NR {
            return fs.throw(Errcode::EINVAL_quota_set_info_bad_type);
        }

        if (1 << ty) & enabled_qtypes(fs) == 0 {
            return fs.throw(Errcode::ESRCH_quota_set_info_not_enabled);
        }

        let mask = info.i_fieldmask as u32;
        if mask & !(c::QC_SPC_TIMER | c::QC_INO_TIMER | c::QC_SPC_WARNS | c::QC_INO_WARNS) != 0 {
            return fs.throw(Errcode::EINVAL_quota_set_info_bad_field);
        }

        let mut sb = fs.sb_lock();
        let mut disk_sb = fs.sb_resize(&mut sb);
        let Some(sb_quota) = sb_get_or_create_quota(&mut disk_sb) else {
            return bch_err_fn!(fs, fs.throw(Errcode::ENOSPC_sb_quota));
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

        sb_quota_read(fs, &sb);

        fs.write_super(&mut sb);
        Ok(())
    }

    /* Get/set individual quotas: */

    fn dqblk(src: &Memquota) -> c::qc_dqblk {
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
            ..::pin_init::zeroed()
        }
    }

    fn get_quota(fs: &Fs, kqid: c::kqid, qdq: &mut c::qc_dqblk) {
        let qid = kqid.id();

        let l = Locked::new(fs, kqid.type_ as usize);
        *qdq = l.get(qid as usize).map_or_else(|| ::pin_init::zeroed(), dqblk);
    }

    fn get_next_quota(fs: &Fs, kqid: &mut c::kqid, qdq: &mut c::qc_dqblk) -> Result<(), BchError> {
        let ty = kqid.type_;
        let qid = kqid.id();

        let l = Locked::new(fs, ty as usize);
        let Some((pos, mq)) = l.iter_from(qid as usize).find(|(_, mq)| **mq != Memquota::default()) else {
            return Err(Errcode::ENOENT_quota_get_next.into());
        };

        *qdq = dqblk(mq);
        *kqid = c::kqid::new_current(ty, pos as u32);
        Ok(())
    }

    /// The new limits in @qdq onto @new_quota - the key there is, if there is
    /// one - and the update.
    fn set_quota_trans(t: &TransAttempt<'_, '_>, new_quota: &mut BkeyI<Quota>, qdq: &c::qc_dqblk)
        -> Result<(), BchError>
    {
        let mut iter = BtreeIter::new(t, Btree::quotas, new_quota.k.p,
                                      BtreeIterFlags::SLOTS | BtreeIterFlags::INTENT);
        let k = iter.peek_slot(t)?.expect("a slot always has a key");

        if let Some(old) = Quota::val_copy_pad(k) {
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

        t.update(&iter, t.bkey_copy(new_quota.as_ref())?, UpdateTriggerFlags::empty())
    }

    fn set_quota(fs: &Fs, rdonly: bool, kqid: c::kqid, qdq: &c::qc_dqblk) -> Result<(), BchError> {
        if false {
            bch_info!(fs, "setting:\n{}", QcDqblk(qdq));
        }

        if rdonly {
            return fs.throw(Errcode::erofs_quota_set_quota);
        }

        let mut new_quota = BkeyI::<Quota>::at(pos(kqid.type_ as u64, kqid.id() as u64));

        let trans = crate::btree_trans!(fs);
        trans.commit_do(None, CommitOpts::new(), |t| set_quota_trans(t, &mut new_quota, qdq))?;
        set(fs, BkeySC::from(new_quota.as_ref()), Some(qdq))
    }

    pub(crate) struct Quotactl;

    #[vtable]
    impl QuotactlOps for Quotactl {
        type Super = Fs;

        fn quota_enable(sb: &SuperBlock, uflags: u32) -> Result<(), BchError> {
            enable(sb.fs(), sb.rdonly(), uflags)
        }

        fn quota_disable(sb: &SuperBlock, uflags: u32) -> Result<(), BchError> {
            disable(sb.fs(), sb.rdonly(), uflags)
        }

        fn rm_xquota(sb: &SuperBlock, uflags: u32) -> Result<(), BchError> {
            remove(sb.fs(), sb.rdonly(), uflags)
        }

        fn get_state(sb: &SuperBlock, state: &mut c::qc_state) -> Result<(), BchError> {
            get_state(sb.fs(), state);
            Ok(())
        }

        fn set_info(sb: &SuperBlock, ty: i32, info: &c::qc_info) -> Result<(), BchError> {
            set_info(sb.fs(), sb.rdonly(), ty, info)
        }

        fn get_dqblk(sb: &SuperBlock, kqid: c::kqid, qdq: &mut c::qc_dqblk) -> Result<(), BchError> {
            get_quota(sb.fs(), kqid, qdq);
            Ok(())
        }

        fn get_nextdqblk(sb: &SuperBlock, kqid: &mut c::kqid, qdq: &mut c::qc_dqblk) -> Result<(), BchError> {
            get_next_quota(sb.fs(), kqid, qdq)
        }

        fn set_dqblk(sb: &SuperBlock, kqid: c::kqid, qdq: &c::qc_dqblk) -> Result<(), BchError> {
            set_quota(sb.fs(), sb.rdonly(), kqid, qdq)
        }
    }

    /// The VFS's: sb->s_qcop.
    pub(crate) static QUOTACTL_OPS: CStatic<c::quotactl_ops, Quotactl> = QuotactlVTable::<Quotactl>::STATIC;
}
