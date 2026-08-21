//! Member devices whose history has diverged from the filesystem's.
//!
//! Not "a device is stale": a stale device's history is a *prefix* and normal
//! recovery catches it up. A divergent one holds writes the filesystem never
//! saw, so the two histories are concurrent, nothing merges them, and somebody
//! has to choose.
//!
//! This runs before [`crate::device_scan::filter_current_sbs`] because
//! bch2_sbs_filter_dead() drops divergent devices *and frees their
//! superblocks* - and drops them the same way it drops devices that were
//! properly removed, so all that is left afterwards is a short device count,
//! which reads as "you are missing a disk".
//!
//! bch2_dev_in_fs() (fs/init/dev.c) decides what diverged; this only decides
//! what to say. Only BCH_ERR_device_splitbrain means divergence - the other
//! returns are different problems, left to the kernel to report. Its own
//! account reaches stderr first, ending in "Not using <dev>", which describes
//! what bch2_sbs_filter_dead() would do rather than what we do.

use std::path::{Path, PathBuf};

use bcachefs_kernel::c;
use bcachefs_kernel::errcode::BchError;
use bcachefs_kernel::sb::members;
use bcachefs_kernel::util::printbuf::Printbuf;
use c::bch_sb_handle;

pub struct Divergent {
    path: PathBuf,
    dev_idx: u8,
    seq: u64,
    write_time: u64,
    /// What the authoritative superblock last recorded for this device, or 0.
    expected_seq: u64,
}

fn seq(sb: &c::bch_sb) -> u64 {
    u64::from_le(sb.seq)
}

fn write_time(sb: &c::bch_sb) -> u64 {
    u64::from_le(sb.write_time)
}

/// Highest seq, then newest write time - sb_cmp() in fs/init/fs.c.
///
/// Only authoritative in the sense the kernel means it: newest wins, which
/// across a genuine fork is arbitrary and quite possibly wrong - a stale
/// rescue image booted later is newer than the filesystem the user wants. Used
/// to have something to compare against, never to decide anything.
fn authoritative(sbs: &[(PathBuf, bch_sb_handle)]) -> Option<usize> {
    sbs.iter()
        .enumerate()
        .max_by_key(|(_, (_, sb))| (seq(sb.sb()), write_time(sb.sb())))
        .map(|(i, _)| i)
}

/// Devices that have diverged, or an empty vec. Never errors: anything that is
/// not divergence is somebody else's problem, reported at mount.
pub fn find(sbs: &[(PathBuf, bch_sb_handle)], opts: &c::bch_opts) -> Vec<Divergent> {
    let Some(best_idx) = authoritative(sbs) else {
        return Vec::new();
    };

    let best_handle = &sbs[best_idx].1;
    let best_members = members::members_v2(best_handle.sb());
    let mut opts = *opts;
    let mut out = Vec::new();

    for (i, (path, handle)) in sbs.iter().enumerate() {
        if i == best_idx {
            continue;
        }

        // SAFETY: bch2_dev_in_fs() compares two superblocks and formats a
        // message; it mutates neither handle. Both outlive the call. The
        // non-const pointers are the C signature, not a licence.
        let ret = unsafe {
            c::bch2_dev_in_fs(
                best_handle as *const _ as *mut _,
                handle as *const _ as *mut _,
                &mut opts,
            )
        };

        if BchError::from_raw(-ret).matches(c::bch_errcode::BCH_ERR_device_splitbrain) {
            let sb = handle.sb();
            out.push(Divergent {
                path: path.clone(),
                dev_idx: sb.dev_idx,
                seq: seq(sb),
                write_time: write_time(sb),
                // What the surviving side last recorded for this device. 0
                // only when it has no member entry at all - a seq collision
                // still has one, and it is the most useful line in the report
                // ("believed it to be at 63, it says 65"), which is the whole
                // vector-clock argument in one sentence.
                expected_seq: best_members
                    .as_ref()
                    .and_then(|m| m.get(sb.dev_idx as u32))
                    .map_or(0, |m| u64::from_le(m.seq)),
            });
        }
    }

    out
}

fn datetime(secs: u64) -> String {
    let mut out = Printbuf::new();
    // SAFETY: writing into a printbuf we own; the value is a plain time64_t.
    unsafe { c::bch2_prt_datetime(out.as_raw(), secs as i64) };
    out.as_str().to_owned()
}

fn name(path: &Path) -> String {
    path.display().to_string()
}

/// Timestamps first: "that is the rescue boot I did on Tuesday" is how someone
/// identifies which half is which. The sequence numbers say roughly how much
/// happened on each side, which is the other half of the judgement.
pub fn report(sbs: &[(PathBuf, bch_sb_handle)], divergent: &[Divergent]) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let n = divergent.len();
    let plural = if n == 1 { "device has" } else { "devices have" };

    let _ = writeln!(
        out,
        "Split brain: {n} {plural} writes this filesystem never saw."
    );

    for d in divergent {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  {} (device {}) last written {}, seq {}",
            name(&d.path),
            d.dev_idx,
            datetime(d.write_time),
            d.seq
        );

        if d.expected_seq != 0 {
            let _ = writeln!(
                out,
                "      the rest of the filesystem last recorded it at seq {}",
                d.expected_seq
            );
        }
    }

    if let Some(best_idx) = authoritative(sbs) {
        let best = sbs[best_idx].1.sb();
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  The rest of the filesystem was last written {}, seq {}.",
            datetime(write_time(best)),
            seq(best)
        );
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Both sides hold real data and nothing can merge them, so continuing \
         with one discards the other's writes. Refusing rather than choosing \
         for you."
    );
    let _ = writeln!(
        out,
        "Mount the side you want by naming only its devices, then \
         `bcachefs device remove` and re-add the others to reintegrate them - \
         which rewrites them, discarding what they hold."
    );

    out
}
