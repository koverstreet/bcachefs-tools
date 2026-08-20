//! Answering degraded=ask.
//!
//! `degraded` is a superblock option with four values - ask/yes/very/no - and
//! the kernel implements three of them. `bch2_fs_may_start()` switches on
//! very and yes, and everything else, `ask` included, falls to the default
//! arm and refuses. That's correct for a kernel: it has no console and no
//! user to put a question to.
//!
//! So `ask` is ours. We turn it into one of the three the kernel implements
//! and pass that down explicitly, and the kernel never sees an `ask` it would
//! have to treat as a refusal. Since `ask` is also the *default*, a
//! multi-device filesystem that loses a member is refused outright until
//! something does this.
//!
//! The user picks between both force levels, not just on/off: `yes` is
//! BCH_FORCE_IF_DEGRADED and stops short of mounting when data has no
//! remaining copy, `very` adds BCH_FORCE_IF_LOST and goes anyway. Offering
//! only `yes` - which is what this did first - means someone whose data really
//! is gone answers the question, gets refused regardless, and is never told
//! which answer would have worked.
//!
//! We can't tell them which case they're in. That needs to know how much data
//! is on the missing devices, which is btree accounting, inside the filesystem
//! we're deciding whether to open; and the superblock stopped carrying
//! user-data replicas entries at
//! bcachefs_metadata_version_no_sb_user_data_replicas, because that section
//! didn't scale with large numbers of drives. Nor can we mount first and look:
//! after an unclean shutdown that means reading and sorting the journal, which
//! is far too much work to spend on a question. So: state both options
//! plainly, and let them choose.
//!
//! Where the answer comes from:
//!   - an explicit -o degraded=... on the command line wins; the user already
//!     answered, don't ask twice
//!   - otherwise whoever crate::prompt can reach - see there for how that's
//!     decided, and why it is settled before a question is composed
//!   - nobody to ask: refuse, and say what we would have asked. Mounting
//!     degraded is a decision about data; making it silently by default is not
//!     ours to make.

use std::fmt::Write as _;
use std::path::PathBuf;
use crate::prompt::{Ask, Prompt};
use std::time::Duration;

use anyhow::Result;
use bcachefs_kernel::c::bch_member_state::BCH_MEMBER_STATE_evacuating;
use bcachefs_kernel::errcode::{self, BchError};
use bcachefs_kernel::util::printbuf::Printbuf;
use bcachefs_kernel::{c, opt_defined, opt_get};
use c::bch_opts;
use c::bch_sb_handle;
use log::warn;

use crate::device_scan;

/// How long the boot-time prompt waits before giving up and refusing. Distinct
/// from missing_dev_timeout, which bounds waiting for hardware; this bounds
/// waiting for a person.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

fn sb_opts(sb: &bch_sb_handle) -> Option<bch_opts> {
    let mut opts: bch_opts = Default::default();

    (unsafe { c::bch2_opts_from_sb(&mut opts, sb.sb) } == 0).then_some(opts)
}

/// What the filesystem says to do about a missing device, unless the caller
/// said otherwise on the command line.
fn degraded_action(sbs: &[(PathBuf, bch_sb_handle)], cli_opts: &bch_opts) -> u8 {
    if opt_defined!(cli_opts, degraded) != 0 {
        return opt_get!(cli_opts, degraded);
    }

    sbs.first()
        .and_then(|(_, sb)| sb_opts(sb))
        .map(|o| opt_get!(o, degraded))
        .unwrap_or(c::bch_degraded_actions::BCH_DEGRADED_ask as u8)
}

/// The two force levels are a real choice, so the user gets to make it.
///
/// `yes` is BCH_FORCE_IF_DEGRADED: mount, but not if that means data with no
/// remaining copy. `very` adds BCH_FORCE_IF_LOST: mount anyway, and accept
/// that some reads will fail. We can't tell the user which case they're in;
/// the module header has the reasons.
///
/// Answering `y` when data *is* lost is not silently wrong: the kernel still
/// refuses, and it is bch2_fs_may_start()'s job to say that `very` was the
/// answer that would have worked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    No,
    /// degraded=yes - only if everything is still readable.
    IfReadable,
    /// The same, read-only. Mounting read-write after losing a device starts
    /// re-replicating onto the survivors, which can fill them and is not easy
    /// to back out of; someone who wants to look at their data before they
    /// have replaced the drive should be able to say so.
    ReadOnly,
    /// degraded=very - even if it isn't.
    Force,
}

/// Which question is being put, and so which answers it will take.
///
/// The two questions this module asks have different vocabularies, and the
/// difference is not cosmetic. `r` is the *cautious* answer to the first one -
/// look at the data, change nothing - and the second one has no read-only
/// rung, because by then we are asking whether to mount with data that has no
/// remaining copy at all. Parsing the second question's reply with the first
/// question's vocabulary turns the most careful answer into the most
/// destructive one, so the vocabulary travels with the question rather than
/// sitting in one shared parser that both callers reach for.
#[derive(Clone, Copy)]
enum Question {
    /// Some devices are missing. Mount without them?
    Degraded,
    /// The kernel refused even that: some data has no remaining copy. Mount
    /// anyway? Yes or no - there is no cautious version of this one.
    Force,
}

impl Question {
    fn parse(self, s: &str) -> Answer {
        match self {
            Question::Degraded => match s.trim() {
                "y" | "Y" | "yes" | "Yes" => Answer::IfReadable,
                "r" | "R" | "ro" | "readonly" => Answer::ReadOnly,
                // "very" because that's the option name we print at them when
                // we refuse ("or degraded=very if data may have no remaining
                // copy"): someone who read that and typed it back meant it.
                "f" | "F" | "force" | "Force" | "very" | "Very" => Answer::Force,
                _ => Answer::No,
            },
            Question::Force => match s.trim() {
                "y" | "Y" | "yes" | "Yes"
                | "f" | "F" | "force" | "Force" | "very" | "Very" => Answer::Force,
                _ => Answer::No,
            },
        }
    }

    /// One line per answer. A terminal has room to spell them out.
    fn choices(self) -> &'static [&'static str] {
        match self {
            Question::Degraded => &[
                "  y  mount degraded, but not if any data has no remaining copy",
                "  r  the same, read-only: nothing gets written or re-replicated",
                "  f  force: mount even if some data will be unreadable",
                "  n  don't mount",
            ],
            Question::Force => &[
                "  y  mount, and accept that some reads will fail",
                "  n  don't mount",
            ],
        }
    }

    /// The bracketed summary, for a prompt that only gets one line.
    fn brief(self) -> &'static str {
        match self {
            Question::Degraded => "[y=if readable / r=read-only / f=force / N]",
            Question::Force    => "[y/N]",
        }
    }
}

impl Answer {
    /// The degraded= value the kernel acts on. Read-only is not one of these -
    /// it is a mount flag, see flags().
    fn opt(self) -> &'static str {
        match self {
            Answer::No => "degraded=no",
            Answer::IfReadable | Answer::ReadOnly => "degraded=yes",
            Answer::Force => "degraded=very",
        }
    }

    /// Mount flags the answer implies.
    ///
    /// MS_RDONLY rather than the read_only filesystem option, so the mount is
    /// read-only to the VFS too and /proc/mounts says so. A user who asked for
    /// read-only should not have to take our word for it.
    fn flags(self) -> libc::c_ulong {
        match self {
            Answer::ReadOnly => libc::MS_RDONLY,
            _ => 0,
        }
    }

    /// Whether a refusal is worth escalating to degraded=very. `f` already
    /// forces and `n` was a refusal; read-only can still be refused, because
    /// bch2_fs_may_start() checks whether the filesystem is *readable* with
    /// what's present, and BCH_FORCE_IF_DEGRADED alone doesn't cover data with
    /// no remaining copy.
    fn escalatable(self) -> bool {
        matches!(self, Answer::IfReadable | Answer::ReadOnly)
    }
}

/// What to call the filesystem when asking about it: its label, or its UUID if
/// it hasn't got one.
///
/// The same choice the passphrase prompt already makes (key.rs), for the same
/// reason - at boot there may be several of these, and a question that doesn't
/// say which filesystem it is about isn't answerable.
fn fs_name(sb: &bch_sb_handle) -> String {
    let label = String::from_utf8_lossy(sb.sb().label());

    if label.is_empty() {
        sb.sb().uuid().hyphenated().to_string()
    } else {
        label.into_owned()
    }
}

fn question(name: &str, missing: usize, expected: usize) -> String {
    format!(
        "Filesystem {name}: only {} of {expected} devices are present. \
         Mount without the missing {}?",
        expected - missing,
        if missing == 1 { "device" } else { "devices" },
    )
}

/// The members we don't have, as the superblock describes them.
///
/// bch2_fs_may_start() prints exactly this when it refuses - it walks the
/// offline members and runs bch2_member_to_text_short() over each. But `ask`
/// has already been resolved to yes/very/no by the time the kernel sees it, so
/// the person answering the question is the one person who never gets told
/// which device it is about. Same formatter, run against the superblock, at
/// the point where the decision is actually made.
///
/// Filtered the way bch2_fs_may_start() filters: a member being evacuated that
/// has no data left on it is being removed on purpose, not missing.
fn missing_devices_to_text(sbs: &[(PathBuf, bch_sb_handle)]) -> Option<String> {
    let (_, first) = sbs.first()?;
    let have = device_scan::present_devices(sbs);

    let sb = first.sb();
    let members = bcachefs_kernel::sb::members::members_v2(sb)?;

    // Null is a legitimate value here - a filesystem with no disk groups has
    // no such field, and bch2_member_to_text_short_sb() handles it.
    let gi = bcachefs_kernel::sb::io::sb_field_get::<c::bch_sb_field_disk_groups>(sb)
        .map_or(std::ptr::null_mut(), |f| f as *const _ as *mut _);
    let sb_ptr = first.sb;

    let mut out = Printbuf::new();

    for idx in 0..members.nr_devices() {
        if have.contains(&(idx as u8)) {
            continue;
        }

        let Some(mut m) = members.get(idx) else { continue };

        // An empty or deleted slot is a hole in the array, not a device. Both
        // spellings matter: a member that was removed keeps its slot with a
        // tombstone UUID rather than being zeroed, and naming a disk the user
        // took out on purpose as one they are now missing is the worst place
        // in this file to get it wrong.
        if !crate::wrappers::sb_display::member_alive(&m) {
            continue;
        }

        if m.member_state() == BCH_MEMBER_STATE_evacuating as u64
            && unsafe { c::bch2_sb_dev_has_data(sb_ptr, idx) } == 0
        {
            continue;
        }

        writeln!(out, "Device {idx}:").unwrap();
        let mut indented = out.indent(2);
        // SAFETY: sb_ptr is the live superblock behind `first`, which outlives
        // this call; `gi` came from it and may be null; idx < nr_devices.
        unsafe { c::bch2_member_to_text_short_sb(indented.as_raw(), &mut m, gi, sb_ptr, idx) };
    }

    (!out.as_str().is_empty()).then(|| out.as_str().to_owned())
}

/// Put a question to whoever is at the machine.
///
/// @extra is anything only a multi-line destination can carry (the missing
/// device list); the agent protocol's Message= is one line, so it reaches a
/// terminal and nothing else.
///
/// Anything not positively recognised as consent is a refusal - see
/// Question::parse - which covers a bare Enter and a timed-out boot prompt.
/// "Nobody to ask" does not reach here: resolve_mount_opts() settles that
/// before there is a question, so holding a Prompt means someone can see it.
fn ask(p: &Prompt, q: Question, prompt: &str, extra: Option<&str>, uuid: &str)
       -> Result<Answer> {
    let reply = p.ask(&Ask {
        prompt,
        detail:  extra,
        choices: q.choices(),
        brief:   q.brief(),
        id:      &format!("bcachefs:UUID={uuid}"),
        timeout: PROMPT_TIMEOUT,
    })?;

    Ok(reply.map_or(Answer::No, |reply| q.parse(&reply)))
}

/// What resolve_mount_opts() worked out, and whether there's a second question
/// worth asking if the mount is refused anyway.
pub struct MountOpts {
    pub fs_opts: Option<String>,
    /// Mount flags the answer implies, to be OR'd into the caller's - MS_RDONLY
    /// when the user asked for read-only. Zero unless we asked and they did.
    pub flags: libc::c_ulong,
    /// Set only when the answer can still be refused - `y` or `r`; see
    /// escalate().
    retry: Option<Retry>,
}

/// Carries the Prompt rather than re-deriving one: this is the second question
/// of the mount, and if the first one reached a person the second must go to
/// the same place. Holding it here also means retry can only be armed when
/// there was somewhere to ask in the first place.
struct Retry {
    fs_opts: Option<String>,
    uuid: String,
    prompt: Prompt,
}

impl MountOpts {
    fn plain(fs_opts: Option<String>) -> MountOpts {
        MountOpts { fs_opts, flags: 0, retry: None }
    }

    /// The mount was refused; @err is everything the kernel had to say about
    /// it. If it was refused because data has no remaining copy, and the user
    /// had allowed only the still-readable case, ask the harder question and
    /// hand back the options to retry with.
    ///
    /// Retrying is cheap: bch2_fs_may_start() is the first thing
    /// __bch2_fs_start() does, before recovery, so a refusal there costs the
    /// superblock reads and nothing else - in particular it has not read and
    /// sorted the journal, which after an unclean shutdown is the expensive
    /// part. This would not be worth doing if it had.
    ///
    /// @err is the code bch2_fs_get_tree() returned, which only reaches us on
    /// the fsconfig(2) path: mount(2) can only carry an errno, and the errno
    /// under insufficient_devices_to_start is EINVAL, which is also what a
    /// typo'd option gives. So there we can't tell and don't try - which is
    /// half of why multi-device mounts take the fs_context path at all.
    pub fn escalate(&self, err: &BchError) -> Result<Option<String>> {
        let Some(retry) = &self.retry else { return Ok(None) };

        if !err.matches(errcode::insufficient_devices_to_start) {
            return Ok(None);
        }

        let prompt = "Some data has no remaining copy and will be unreadable. \
                      Mount anyway?";

        if ask(&retry.prompt, Question::Force, prompt, None, &retry.uuid)?
            != Answer::Force {
            return Ok(None);
        }

        warn!("retrying with degraded=very");
        Ok(Some(append_opt(retry.fs_opts.clone(), Answer::Force.opt())))
    }
}

/// Resolve degraded=ask into an explicit option, appended to the fs options
/// the kernel is about to be given.
///
/// Returns the options unchanged whenever there's nothing to decide: all
/// devices present, or a policy the kernel can already act on.
pub fn resolve_mount_opts(
    sbs: &[(PathBuf, bch_sb_handle)],
    cli_opts: &bch_opts,
    fs_opts: Option<String>,
) -> Result<MountOpts> {
    let Some((_, first)) = sbs.first() else {
        return Ok(MountOpts::plain(fs_opts));
    };

    // By device, not by path: see device_scan::present_devices(). A member
    // found twice - multipath, or udev and the block scan both contributing -
    // would otherwise make up the count for one that is missing, and the
    // question below would never be asked at all.
    let expected = device_scan::expected_devices(sbs);
    let present = device_scan::present_devices(sbs).len();

    if present >= expected {
        return Ok(MountOpts::plain(fs_opts));
    }

    let missing = expected - present;

    if degraded_action(sbs, cli_opts) != c::bch_degraded_actions::BCH_DEGRADED_ask as u8 {
        // yes/very/no: the kernel acts on these itself. An explicit
        // -o degraded= is the user's decision and we don't second-guess it,
        // so there's nothing to escalate either.
        return Ok(MountOpts::plain(fs_opts));
    }

    let q = question(&fs_name(first), missing, expected);
    let uuid = first.sb().uuid().hyphenated().to_string();

    // Which devices, not just how many. The agent protocol's Message= is a
    // single line so it can't carry this, but a terminal and the log can - and
    // whoever is being asked needs to know whether it's the disk they just
    // unplugged or something they didn't know about.
    let devs = missing_devices_to_text(sbs);

    // Settle whether anyone can be asked before composing a question at them.
    // Mounting degraded is a decision about data; making it silently by default
    // is not ours to make, so with nobody there we say what we would have asked
    // and refuse.
    let Some(p) = Prompt::detect() else {
        warn!("{q}");
        for line in devs.as_deref().unwrap_or("").lines() {
            warn!("{line}");
        }
        warn!("no way to ask anyone; refusing (mount -o degraded=yes, \
               or degraded=very if data may have no remaining copy)");
        return Ok(MountOpts::plain(fs_opts));
    };

    let answer = ask(&p, Question::Degraded, &q, devs.as_deref(), &uuid)?;

    // warn, not info: the default verbosity is Warn, and everything here is a
    // decision about the user's data that we made for them. Without this the
    // only thing they see is the kernel's insufficient_devices_to_start, which
    // does not mention that a question was asked and answered on their behalf,
    // or that -o degraded=yes exists. This runs only when a device is missing,
    // so it is not chatter.
    warn!("mounting {}with {}",
          if answer == Answer::ReadOnly { "read-only " } else { "" },
          answer.opt());

    Ok(MountOpts {
        fs_opts: Some(append_opt(fs_opts.clone(), answer.opt())),
        flags: answer.flags(),
        retry: answer.escalatable().then_some(Retry { fs_opts, uuid, prompt: p }),
    })
}

/// Append one option to a mount option string, which may be absent or empty -
/// a stray leading comma is a parse error, not a cosmetic problem.
fn append_opt(fs_opts: Option<String>, opt: &str) -> String {
    match fs_opts {
        Some(o) if !o.is_empty() => format!("{o},{opt}"),
        _ => opt.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_opt_handles_absent_and_empty() {
        assert_eq!(append_opt(None, "degraded=yes"), "degraded=yes");
        assert_eq!(append_opt(Some(String::new()), "degraded=yes"), "degraded=yes");
        assert_eq!(append_opt(Some("ro".into()), "degraded=yes"), "ro,degraded=yes");
        assert_eq!(
            append_opt(Some("ro,noatime".into()), "degraded=no"),
            "ro,noatime,degraded=no"
        );
    }

    #[test]
    fn answer_parses_both_force_levels() {
        for s in ["y", "Y", "yes", "Yes", " y \n"] {
            assert_eq!(Question::Degraded.parse(s), Answer::IfReadable, "{s:?}");
        }
        for s in ["f", "F", "force", "Force", " f \n", "very"] {
            assert_eq!(Question::Degraded.parse(s), Answer::Force, "{s:?}");
        }
        for s in ["r", "R", "ro", "readonly"] {
            assert_eq!(Question::Degraded.parse(s), Answer::ReadOnly, "{s:?}");
        }
    }

    /// The safety property: this is a decision about the user's data, so
    /// anything we don't positively recognise as consent is a refusal --
    /// including an empty answer, which is what a bare Enter and a timed-out
    /// systemd prompt both produce. True of both questions.
    #[test]
    fn answer_defaults_to_refusing() {
        for s in ["", "\n", "n", "N", "no", "q", "yolo", "degraded"] {
            assert_eq!(Question::Degraded.parse(s), Answer::No, "{s:?}");
            assert_eq!(Question::Force.parse(s), Answer::No, "{s:?}");
        }
    }

    /// The escalation question has no read-only rung - by the time it is asked,
    /// the choice is whether to mount data that has no remaining copy, and
    /// there is nothing cautious to offer. So `r`, which is the *most* careful
    /// answer to the question immediately before it, must not read as consent
    /// here. One parser shared between the two questions is what made it.
    #[test]
    fn the_cautious_answer_is_not_consent_to_force() {
        for s in ["r", "R", "ro", "readonly"] {
            assert_eq!(Question::Force.parse(s), Answer::No, "{s:?}");
        }
        for s in ["y", "yes", "f", "force", "very"] {
            assert_eq!(Question::Force.parse(s), Answer::Force, "{s:?}");
        }
    }

    /// escalate() must decide *not* to ask before it touches a terminal, so
    /// these two cases never prompt - which is what makes them testable, and
    /// is also the property that matters: a mount failing for some unrelated
    /// reason must not start interrogating the user about degraded mounts.
    #[test]
    fn escalate_does_not_ask_when_we_never_asked() {
        let o = MountOpts::plain(Some("ro".into()));
        let e = BchError::from_errcode(errcode::insufficient_devices_to_start);

        assert!(o.escalate(&e).unwrap().is_none());
    }

    #[test]
    fn escalate_does_not_ask_for_an_unrelated_failure() {
        let o = MountOpts {
            fs_opts: Some("degraded=yes".into()),
            flags: 0,
            retry: Some(Retry { fs_opts: None, uuid: "x".into(),
                                prompt: Prompt::Terminal }),
        };

        for e in [
            BchError::from_errcode(errcode::EINVAL_opt_parse_str_required),
            // A flattened errno, which is what the mount(2) fallback gives us:
            // the same EINVAL a typo'd option produces, so it must not be read
            // as an answer to the degraded question.
            BchError::from_raw(libc::EINVAL),
            BchError::from_raw(0),
            // A code from a kernel module newer than this binary - the normal
            // state of affairs for a filesystem that ships DKMS-only. The
            // errcode parent-chain walk BUG_ON()s rather than bounds-checking,
            // so what's being asserted here is as much "doesn't abort" as
            // "doesn't match".
            BchError::from_raw(c::bch_errcode::BCH_ERR_MAX as i32 + 1),
        ] {
            assert!(o.escalate(&e).unwrap().is_none(), "{e:?}");
        }
    }

    #[test]
    fn answer_maps_to_the_option_the_kernel_acts_on() {
        assert_eq!(Answer::No.opt(), "degraded=no");
        assert_eq!(Answer::IfReadable.opt(), "degraded=yes");
        assert_eq!(Answer::Force.opt(), "degraded=very");
    }

    #[test]
    fn question_counts_what_is_present_not_what_is_missing() {
        assert!(question("tank", 1, 3).contains("2 of 3"));
        assert!(question("tank", 1, 3).contains("device?"));
        assert!(question("tank", 2, 3).contains("1 of 3"));
        assert!(question("tank", 2, 3).contains("devices?"));
        // Whose filesystem it is, not just how many disks: at boot there can
        // be several, and the systemd prompt is one line with no other context.
        assert!(question("tank", 1, 3).contains("tank"));
    }
}
