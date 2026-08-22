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

use anyhow::Result;
use log::warn;

use crate::prompt::{fs_name, Choice, Prompt, Question, PROMPT_TIMEOUT};
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
    let other = other_side(sbs, divergent);

    let _ = writeln!(
        out,
        "Split brain: {n} {plural} writes {other} never saw."
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
                "      {other} believed it to be at seq {}",
                d.expected_seq
            );
        }
    }

    if let Some(best_idx) = authoritative(sbs) {
        let best = sbs[best_idx].1.sb();
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  {other} last written {}, seq {}.",
            datetime(write_time(best)),
            seq(best)
        );
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Both sides hold real data and nothing can merge them, so continuing \
         with one leaves the other's writes behind."
    );

    // Both routes out, named, whether or not anyone is here to be asked - this
    // is also what someone reads in the journal after a boot refused.
    let _ = writeln!(out);
    for d in divergent {
        let _ = writeln!(
            out,
            "  To continue with {}'s history instead, mount naming only its devices.",
            name(&d.path)
        );
    }
    let _ = writeln!(
        out,
        "  To rejoin a diverged device once mounted: `bcachefs device remove` it \
         and add it back, which rewrites it and discards what it holds."
    );

    out
}

/// Terminal only, and not a limitation to fix later: a destructive choice may
/// only be offered where its evidence fits, and the evidence here - which
/// device, written when, how far each side got - does not fit the agent
/// protocol's one-line `Message=`. Offering the choice without it is worse
/// than refusing, because they will take the default.
///
/// Yes erases nothing: the diverged devices are left out of this mount and
/// untouched on disk, so the other history is still there to mount afterwards.
/// That is what makes it askable at all. Rewriting a device so it rejoins
/// needs a mounted filesystem, and is left to the user via [`report`].
pub fn ask(sb: &bch_sb_handle) -> Result<bool> {
    let Some(p) = Prompt::detect() else {
        warn!("no way to ask which history to continue with; refusing");
        return Ok(false);
    };

    if matches!(p, Prompt::Agent) {
        warn!("cannot show two histories through a one-line prompt; refusing");
        return Ok(false);
    }

    let name = fs_name(sb);
    let uuid = sb.sb().uuid().hyphenated().to_string();

    // Moot cannot happen: a device arriving does not un-diverge anything, so
    // no watch is passed.
    Ok(p.put(&Question {
        prompt:  &format!("Continue with {name}'s surviving history?"),
        detail:  None,
        choices: CHOICES,
        silence: false,
        uuid:    &uuid,
        timeout: Some(PROMPT_TIMEOUT),
    }, None)?.unwrap_or(false))
}

/// Only an explicit `c` continues, and note what is *not* here: `y`.
///
/// This is not a yes/no. Someone answering a prompt they did not read out of
/// habit should not thereby choose which of two histories to keep - so `y`
/// matches nothing and falls to silence, which refuses.
const CHOICES: &[Choice<bool>] = &[
    Choice { key: 'c', aliases: &["continue"], short: "continue",
             blurb: "continue, leaving the diverged device(s) out of this mount",
             answer: true },
    Choice { key: 'n', aliases: &["no"], short: "",
             blurb: "don't mount", answer: false },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(reply: &str) -> bool {
        Question {
            prompt: "", detail: None, choices: CHOICES,
            silence: false, uuid: "", timeout: None,
        }.parse(reply)
    }

    #[test]
    fn only_an_explicit_c_continues() {
        for yes in ["c", "C", "continue", "Continue", " c ", "c\n"] {
            assert!(parse(yes), "{yes:?} should continue");
        }
        // "" is a bare Enter, and "y" is the habit this question must not honour.
        for no in ["", " ", "n", "N", "no", "y", "Y", "yes", "f", "very", "cc", "x"] {
            assert!(!parse(no), "{no:?} must not continue");
        }
    }
}

/// What to call the side that is not diverging.
///
/// "The rest of the filesystem" reads fine when nine devices agree and one does
/// not. With two devices there is no rest - there are two halves, and which one
/// counts as "the filesystem" is the arbitrary newest-wins pick that
/// [`authoritative`] exists to warn about. Naming the device instead of
/// implying a verdict keeps the prose as neutral as the code.
fn other_side(sbs: &[(PathBuf, bch_sb_handle)], divergent: &[Divergent]) -> String {
    let surviving = sbs.len() - divergent.len();

    match (surviving, authoritative(sbs)) {
        (1, Some(i)) => name(&sbs[i].0),
        _ => "the rest of the filesystem".to_string(),
    }
}
