// SPDX-License-Identifier: GPL-2.0

//! Whether a data update can be done now, checked before reading anything: a
//! write that can't succeed fails here, with its reason, instead of after a
//! wasted read. Movers check before setting an update up; init() checks
//! again, except for copygc.
//!
//! The check is on durability: what the update keeps, plus room on the target
//! for the rest of the replicas - rw devices without a kept copy, with free
//! buckets at the update's watermark. If copygc could free buckets, the error
//! is need_copygc and copygc is woken, so the update can come back later;
//! otherwise it's no_rw_devs. must_ec also needs a stripe head. Reconcile's
//! btree node rewrites are checked differently: whether a rewrite would
//! improve anything.
//!
//! Each check can explain itself into a trace, for reconcile_set_pending.

use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{BkeySC, BkeyV};
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::c;
use crate::c::bch_errcode;
use crate::data::copygc;
use crate::data::durability::{extent_ptr_durability, BkeyVDurability};
use crate::data::ec::head::{head_get, stripe_head_put};
use crate::data::ptrs::{BkeyVPtrs, BtreePtrs, Ptrs};
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::util::rcu::read_lock;
use crate::util::Printbuf;
use core::ffi::c_int;
use cstruct_macros::c_export;

use c::bch_write_flags as w;

#[derive(Clone, Copy, Default)]
struct Available {
    durability:  u32,
    /// A device had no free buckets, but copygc can make some
    need_copygc: bool,
}

fn no_room<T>(fs: &Fs, need_copygc: bool) -> Result<T, BchError> {
    fs.throw(if need_copygc {
        bch_errcode::BCH_ERR_data_update_fail_need_copygc
    } else {
        bch_errcode::BCH_ERR_data_update_fail_no_rw_devs
    })
}

fn watermark(data_opts: &c::data_update_opts) -> c::bch_watermark {
    c::bch_watermark::from_bits(data_opts.commit_flags.bits() & c::BCH_WATERMARK_MASK)
}

impl Fs {
    /// Durability available to a write to @target, 0 meaning every device. A
    /// cached write counts 1 per device.
    fn durability_available(&self, data_opts: &c::data_update_opts, data_type: c::bch_data_type,
                            target: u32, devs_have: &c::bch_devs_list,
                            mut trace: Option<&mut Printbuf>) -> Available {
        if let Some(out) = trace.as_deref_mut() {
            write!(out, "available to write on target=");
            self.target_to_text(out, Target::decode(target));
            out.newline();
        }
        let mut trace = trace.map(|out| out.indent(2));
        let mut trace = trace.as_deref_mut().map(Printbuf::atomic);

        let watermark = watermark(data_opts);
        let cached = data_opts.write_flags.contains(w::BCH_WRITE_cached);

        // copygc::can_make_progress() reads accounting for dev_leaving
        let _mark = self.mark_lock().read();
        let rcu = read_lock();
        let devs = self.target_rw_devs(data_type, Target::decode(target));
        let mut ret = Available::default();

        for i in devs.d.iter() {
            if devs_have.has(i as u8) {
                continue;
            }
            let Some(ca) = self.dev_rcu(i as u32, &rcu) else { continue };

            let free = ca.free_buckets_at(&ca.usage_read_fast(), watermark);
            if free != 0 {
                ret.durability += if cached { 1 } else { ca.rw_durability() };
            } else if copygc::can_make_progress(ca) {
                ret.need_copygc = true;
                self.copygc_wakeup();
            }

            if let Some(out) = trace.as_deref_mut() {
                write!(out, "{}: durability {} free {free} \n",
                       ca, ca.mi.get().durability);
            }
        }
        ret
    }

    /// Ignores stripe pointers: btree pointers have none.
    fn btree_ptr_durability_on_target(&self, k: BkeySC<'_, BtreePtrs>, target: u32) -> u32 {
        let devs = self.target_rw_devs(c::bch_data_type::BCH_DATA_btree, Target::decode(target));
        let rcu = read_lock();

        k.ptrs(self)
            .filter(|ptr| ptr.cached() == 0)
            .map(|ptr| u32::from(ptr.dev()))
            .filter(|&dev| dev != c::BCH_SB_MEMBER_INVALID && devs.d.test(dev as usize))
            .map(|dev| self.dev_durability(dev, &rcu))
            .sum()
    }

    fn btree_ptr_has_dev_evacuating(&self, k: BkeySC<'_, BtreePtrs>) -> bool {
        let rcu = read_lock();
        k.ptrs(self).any(|ptr| {
            self.dev_rcu(ptr.dev().into(), &rcu)
                .is_some_and(|ca| u32::from(ca.mi.get().state) == c::bch_member_state::BCH_MEMBER_STATE_evacuating.0)
        })
    }

    /// A btree node rewrite replaces the whole node, so it's worth it only if
    /// it improves something - replicas on target, durability, getting off an
    /// evacuating device - without regressing. Node allocation spills off
    /// target rather than failing, so a rewrite that can't improve placement
    /// "succeeds" off target and requeues forever; and the replacement gets
    /// whatever durability the filesystem has, which trades replicas for
    /// placement if it's less - worth it only to evacuate.
    fn write_btree_feasible(&self, opts: &c::bch_inode_opts, data_opts: &c::data_update_opts,
                            k: BkeySC<'_, BtreePtrs>, mut trace: Option<&mut Printbuf>) -> Result<(), BchError> {
        let evacuating = self.btree_ptr_has_dev_evacuating(k);
        let replicas = u32::from(opts.data_replicas);

        // Dropping excess replicas is progress regardless of placement
        if u32::from(k.durability_safe(self).nr_ptrs) > replicas {
            return Ok(());
        }

        let none = c::bch_devs_list::default();
        let btree = c::bch_data_type::BCH_DATA_btree;
        let target = data_opts.target.into();

        let durability        = u32::from(k.btree_ptr_durability(self).total);
        let target_durability = self.btree_ptr_durability_on_target(k, target);
        let on_target         = self.durability_available(data_opts, btree, target, &none, trace.as_deref_mut());
        let on_fs             = if target != 0 {
            self.durability_available(data_opts, btree, 0, &none, trace.as_deref_mut())
        } else {
            on_target
        };
        let need_copygc = on_target.need_copygc || on_fs.need_copygc;

        let improves_placement  = on_target.durability > target_durability;
        let improves_durability = durability < replicas && on_fs.durability > durability;
        let degrades            = on_fs.durability < durability.min(replicas);

        let worth_it = if evacuating {
            on_fs.durability > 0
        } else {
            (improves_placement || improves_durability) && !degrades
        };
        if worth_it { Ok(()) } else { no_room(self, need_copygc) }
    }

    pub fn data_update_write_feasible(&self, opts: &c::bch_inode_opts, data_opts: &c::data_update_opts,
                                      devs_have: &c::bch_devs_list, k: BkeySC<'_>,
                                      mut trace: Option<&mut Printbuf>) -> Result<(), BchError> {
        let btree = k.arm::<BtreePtrs>();
        if let Some(b) = btree.filter(|_| data_opts.type_ == c::bch_data_update_types::BCH_DATA_UPDATE_reconcile) {
            return self.write_btree_feasible(opts, data_opts, b, trace);
        }

        let data_type = if btree.is_some() { c::bch_data_type::BCH_DATA_btree } else { c::bch_data_type::BCH_DATA_user };
        let target = if data_opts.write_flags.contains(w::BCH_WRITE_only_specified_devs) {
            data_opts.target.into()
        } else {
            0
        };

        if let Some(out) = trace.as_deref_mut() {
            write!(out, "keeping devices: ");
            self.devs_list_to_text(out, devs_have);
            out.newline();

            if data_opts.write_flags.contains(w::BCH_WRITE_alloc_nowait) {
                write!(out, "nonblocking\n");
            }
        }

        let a = self.durability_available(data_opts, data_type, target, devs_have, trace);
        if a.durability != 0 { Ok(()) } else { no_room(self, a.need_copygc) }
    }
}

impl TransAttempt<'_, '_> {
    /// Ok if the update can be done; otherwise why not, a data_update_fail_*
    /// or ec_alloc_failed error, explained into @trace too. Getting a stripe
    /// head takes mutexes, so it can restart.
    pub fn can_do_data_update(&self, opts: &c::bch_inode_opts, data_opts: &c::data_update_opts,
                              k: BkeySC<'_>, mut trace: Option<&mut Printbuf>) -> Result<(), BchError> {
        let fs = self.fs();
        let mut devs_have = c::bch_devs_list::default();
        let mut keeping = 0;

        if let Some(k) = k.arm::<Ptrs>() {
            let mut bit = 1u32;
            for d in k.ptrs_decoded(fs) {
                let p = &d.p;
                let dev = u32::from(p.ptr.dev());
                if dev != c::BCH_SB_MEMBER_INVALID && bit & u32::from(data_opts.ptrs_kill) == 0 {
                    // a pointer only losing its stripe keeps its device's durability
                    keeping += if bit & u32::from(data_opts.ptrs_kill_ec) != 0 {
                        fs.dev_durability(dev, &read_lock())
                    } else {
                        extent_ptr_durability(self, p)?
                    };
                    if !data_opts.bits.no_devs_have() && p.ptr.cached() == 0 {
                        devs_have.add(p.ptr.dev());
                    }
                }
                bit <<= 1;
            }
        }

        let replicas = u32::from(opts.data_replicas);
        if !k.k.is_btree_ptr() && data_opts.extra_replicas == 0 && keeping >= replicas {
            return Ok(());
        }

        if let Some(out) = trace.as_deref_mut() {
            // wraps if keeping exceeds replicas
            write!(out, "need {} replicas\n", replicas.wrapping_sub(keeping));
        }

        fs.data_update_write_feasible(opts, data_opts, &devs_have, k, trace)?;

        // Erasure coding that's sure to fail - too few devices, say -
        // shouldn't cost a read first
        if data_opts.write_flags.contains(w::BCH_WRITE_must_ec) {
            // The ec_alloc_failed subtypes park the extent; a restart must
            // stay one, or an extent that only needed a retry is parked
            let mut req = self.alloc_request(data_opts.target.into(), false, core::ptr::null_mut(), 0, replicas,
                                             c::bch_watermark::BCH_WATERMARK_normal, w::empty(),
                                             core::ptr::null_mut())?;
            // alloc_request() leaves it to the caller. The write path caps it
            // from the options; this probe runs uncapped
            req.ec_max_data_blocks = 0;

            let (_, g, _) = head_get(self, &mut req, 0, None)?;
            stripe_head_put(fs, g);
        }
        Ok(())
    }
}

/// # Safety
/// @trans is in an attempt.
#[c_export]
pub unsafe fn bch2_can_do_data_update<'t>(trans: &'t BtreeTrans<'t>, opts: &c::bch_inode_opts,
                                          data_opts: &c::data_update_opts, k: BkeySC<'_>,
                                          trace: Option<&mut Printbuf>) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(t.can_do_data_update(opts, data_opts, k, trace))
}
