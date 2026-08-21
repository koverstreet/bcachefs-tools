//! Answering degraded=ask.
//!
//! bch2_fs_may_start() acts on yes and very and refuses everything else,
//! `ask` included - correct for a kernel, which has no user to ask. So `ask`
//! is ours to resolve first, and since it is the default, a filesystem that
//! loses a member is refused outright until something here answers.

use std::fmt::Write as _;
use std::path::PathBuf;
use crate::prompt::{fs_name, Choice, Prompt, Question, Watch, PROMPT_TIMEOUT};

use anyhow::Result;
use bcachefs_kernel::c::bch_member_state::BCH_MEMBER_STATE_evacuating;
use bcachefs_kernel::errcode::{self, BchError};
use bcachefs_kernel::util::printbuf::Printbuf;
use bcachefs_kernel::{c, opt_defined, opt_get};
use c::bch_opts;
use c::bch_sb_handle;
use log::warn;

use crate::device_scan;

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

/// Answering IfReadable when data *is* lost is not silently wrong: the kernel
/// refuses anyway, and bch2_fs_may_start() says that `very` was the answer
/// that would have worked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    No,
    /// BCH_FORCE_IF_DEGRADED.
    IfReadable,
    /// The same, read-only: mounting read-write re-replicates onto the
    /// survivors, which can fill them and is not easy to back out of.
    ReadOnly,
    /// BCH_FORCE_IF_LOST too.
    Force,
}

/// Some devices are missing. Mount without them?
///
/// `r` is the *cautious* answer here and means nothing on the escalation
/// question below, which has no read-only rung - so the two stay separate
/// lists rather than one shared parser. Merging them would turn the most
/// careful answer into the most destructive one.
const DEGRADED_CHOICES: &[Choice<Answer>] = &[
    Choice { key: 'y', aliases: &["yes"], short: "if readable",
             blurb: "mount degraded, but not if any data has no remaining copy",
             answer: Answer::IfReadable },
    Choice { key: 'r', aliases: &["ro", "readonly"], short: "read-only",
             blurb: "the same, read-only: nothing gets written or re-replicated",
             answer: Answer::ReadOnly },
    // "very" because that's the option name we print at them when we refuse
    // ("or degraded=very if data may have no remaining copy"): someone who
    // read that and typed it back meant it.
    Choice { key: 'f', aliases: &["force", "very"], short: "force",
             blurb: "force: mount even if some data will be unreadable",
             answer: Answer::Force },
    Choice { key: 'n', aliases: &["no"], short: "",
             blurb: "don't mount", answer: Answer::No },
];

/// The kernel refused even that: some data has no remaining copy. Mount
/// anyway? Yes or no - there is no cautious version of this one.
const FORCE_CHOICES: &[Choice<Answer>] = &[
    Choice { key: 'y', aliases: &["yes", "f", "force", "very"], short: "",
             blurb: "mount, and accept that some reads will fail",
             answer: Answer::Force },
    Choice { key: 'n', aliases: &["no"], short: "",
             blurb: "don't mount", answer: Answer::No },
];

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

    /// MS_RDONLY rather than the read_only filesystem option, so the mount is
    /// read-only to the VFS too and /proc/mounts says so. A user who asked for
    /// read-only should not have to take our word for it.
    fn flags(self) -> libc::c_ulong {
        match self {
            Answer::ReadOnly => libc::MS_RDONLY,
            _ => 0,
        }
    }

    /// Whether a refusal is worth escalating to degraded=very. Read-only can
    /// still be refused: bch2_fs_may_start() checks whether the filesystem is
    /// *readable* with what's present, and BCH_FORCE_IF_DEGRADED alone doesn't
    /// cover data with no remaining copy.
    fn escalatable(self) -> bool {
        matches!(self, Answer::IfReadable | Answer::ReadOnly)
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
/// bch2_fs_may_start() prints exactly this when it refuses, but `ask` has been
/// resolved to yes/very/no by the time the kernel sees it - so the person
/// answering the question is the one person who never gets told which device it
/// is about. Same formatter, run where the decision is actually made.
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

        if !counts_as_missing(&m, sb_ptr, idx) {
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

/// Whether an absent member is one the user is missing, rather than one they
/// took out on purpose. Naming the second as the first is the worst thing in
/// this file to get wrong.
///
/// Both spellings of "not a device" matter: an unused slot is zeroed, and a
/// member that was removed keeps its slot with a tombstone UUID. The
/// evacuating test is bch2_fs_may_start()'s own - a device being emptied with
/// nothing left on it is mid-removal.
fn counts_as_missing(m: &c::bch_member, sb: *mut c::bch_sb, idx: u32) -> bool {
    if !crate::wrappers::sb_display::member_alive(m) {
        return false;
    }

    if m.member_state() != BCH_MEMBER_STATE_evacuating as u64 {
        return true;
    }

    // SAFETY: @sb is the live superblock the caller read @m out of, and
    // idx < nr_devices because that is what indexed @m.
    let has_data = unsafe { c::bch2_sb_dev_has_data(sb, idx) };

    has_data != 0
}

/// @extra reaches a terminal and nothing else: the agent protocol's Message=
/// is one line.
fn ask(p: &Prompt, choices: &[Choice<Answer>], prompt: &str, extra: Option<&str>,
       uuid: &str, watch: Option<&mut dyn Watch>) -> Result<Option<Answer>> {
    p.put(&Question {
        prompt,
        detail:  extra,
        choices,
        silence: Answer::No,
        uuid,
        timeout: Some(PROMPT_TIMEOUT),
    }, watch)
}

pub struct MountOpts {
    pub fs_opts: Option<String>,
    /// To be OR'd into the caller's flags.
    pub flags: libc::c_ulong,
    retry: Option<Retry>,
}

/// Carries the Prompt rather than re-deriving one: the second question of a
/// mount must reach whoever answered the first. Holding it here is also what
/// keeps retry from being armed when there was nobody to ask.
struct Retry {
    fs_opts: Option<String>,
    uuid: String,
    prompt: Prompt,
}

impl MountOpts {
    fn plain(fs_opts: Option<String>) -> MountOpts {
        MountOpts { fs_opts, flags: 0, retry: None }
    }

    /// Retrying is cheap: bch2_fs_may_start() is the first thing
    /// __bch2_fs_start() does, before recovery, so a refusal there costs the
    /// superblock reads and nothing else - in particular it has not read and
    /// sorted the journal, which after an unclean shutdown is the expensive
    /// part. This would not be worth doing if it had.
    ///
    /// @err is what bch2_fs_get_tree() returned, which only reaches us on the
    /// fsconfig(2) path: mount(2) can carry only an errno, and the errno under
    /// insufficient_devices_to_start is EINVAL - the same one a typo'd option
    /// gives. So there we can't tell and don't try, which is half of why
    /// multi-device mounts take the fs_context path at all.
    pub fn escalate(&self, err: &BchError) -> Result<Option<String>> {
        let Some(retry) = &self.retry else { return Ok(None) };

        if !err.matches(errcode::insufficient_devices_to_start) {
            return Ok(None);
        }

        let prompt = "Some data has no remaining copy and will be unreadable. \
                      Mount anyway?";

        if ask(&retry.prompt, FORCE_CHOICES, prompt, None, &retry.uuid, None)?.unwrap_or(Answer::No)
            != Answer::Force {
            return Ok(None);
        }

        warn!("retrying with degraded=very");
        Ok(Some(append_opt(retry.fs_opts.clone(), Answer::Force.opt())))
    }
}

/// Resolve degraded=ask into an explicit option, appended to the fs options
/// the kernel is about to be given. Unchanged when there is nothing to decide.
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
        // The kernel acts on yes/very/no itself, and an explicit -o degraded=
        // is the user's decision - so there's nothing to escalate either.
        return Ok(MountOpts::plain(fs_opts));
    }

    let q = question(&fs_name(first), missing, expected);
    let uuid = first.sb().uuid().hyphenated().to_string();

    // Whoever is being asked needs to know whether it's the disk they just
    // unplugged or something they didn't know about.
    let devs = missing_devices_to_text(sbs);

    // Mounting degraded is a decision about data, so with nobody there to make
    // it we say what we would have asked, and refuse.
    let Some(p) = Prompt::detect() else {
        // One warning: this is one event - the question we would have asked,
        // who it was about, and what we did instead. Emitting it a line at a
        // time stamps file:line on each, and the device list is indented text
        // that only reads as a block.
        let mut msg = q.clone();

        if let Some(devs) = &devs {
            msg.push('\n');
            msg.push_str(devs.trim_end());
        }

        msg.push_str("\nno way to ask anyone; refusing (mount -o degraded=yes, \
                      or degraded=very if data may have no remaining copy)");

        warn!("{msg}");
        return Ok(MountOpts::plain(fs_opts));
    };

    // If the missing device turns up while the question is on screen, that is
    // the answer: stop asking and carry on with the boot.
    let use_udev = opt_get!(cli_opts, mount_trusts_udev) != 0;
    let mut dw = device_scan::DeviceWatch::new(sbs, cli_opts, use_udev);
    let watch = dw.as_mut().map(|d| d as &mut dyn Watch);

    let Some(answer) = ask(&p, DEGRADED_CHOICES, &q, devs.as_deref(), &uuid,
                           watch)?
    else {
        warn!("device turned up while asking; mounting normally");
        return Ok(MountOpts::plain(fs_opts));
    };

    // warn, not info: the default verbosity is Warn, and this is a decision
    // about the user's data that we made for them. Otherwise all they see is
    // the kernel's insufficient_devices_to_start, which mentions neither that
    // a question was answered on their behalf nor that -o degraded=yes exists.
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

    fn parse(choices: &[Choice<Answer>], reply: &str) -> Answer {
        Question {
            prompt: "", detail: None, choices,
            silence: Answer::No, uuid: "", timeout: None,
        }.parse(reply)
    }

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

    fn q(choices: &[Choice<Answer>]) -> Question<'_, Answer> {
        Question {
            prompt: "", detail: None, choices,
            silence: Answer::No, uuid: "", timeout: None,
        }
    }

    /// Deriving both rendered forms from the answer list is what stops them
    /// drifting apart, but it also means a change to the derivation silently
    /// rewords every prompt. The capitalised answer is how a reader knows what
    /// Enter will do.
    #[test]
    fn the_summary_names_the_answers_and_marks_the_default() {
        assert_eq!(q(DEGRADED_CHOICES).brief(),
                   "[y=if readable / r=read-only / f=force / N]");
        assert_eq!(q(FORCE_CHOICES).brief(), "[y / N]");

        assert_eq!(q(FORCE_CHOICES).lines(), [
            "  y  mount, and accept that some reads will fail",
            "  n  don't mount",
        ]);
    }

    #[test]
    fn answer_parses_both_force_levels() {
        for s in ["y", "Y", "yes", "Yes", " y \n"] {
            assert_eq!(parse(DEGRADED_CHOICES, s), Answer::IfReadable, "{s:?}");
        }
        for s in ["f", "F", "force", "Force", " f \n", "very"] {
            assert_eq!(parse(DEGRADED_CHOICES, s), Answer::Force, "{s:?}");
        }
        for s in ["r", "R", "ro", "readonly"] {
            assert_eq!(parse(DEGRADED_CHOICES, s), Answer::ReadOnly, "{s:?}");
        }
    }

    /// Anything not positively recognised as consent is a refusal - including
    /// an empty answer, which is what a bare Enter and a timed-out systemd
    /// prompt both produce.
    #[test]
    fn answer_defaults_to_refusing() {
        for s in ["", "\n", "n", "N", "no", "q", "yolo", "degraded"] {
            assert_eq!(parse(DEGRADED_CHOICES, s), Answer::No, "{s:?}");
            assert_eq!(parse(FORCE_CHOICES, s), Answer::No, "{s:?}");
        }
    }

    /// `r` is the most careful answer to the question immediately before this
    /// one, and must not read as consent here. A shared parser is what would
    /// make it one.
    #[test]
    fn the_cautious_answer_is_not_consent_to_force() {
        for s in ["r", "R", "ro", "readonly"] {
            assert_eq!(parse(FORCE_CHOICES, s), Answer::No, "{s:?}");
        }
        for s in ["y", "yes", "f", "force", "very"] {
            assert_eq!(parse(FORCE_CHOICES, s), Answer::Force, "{s:?}");
        }
    }

    /// A mount failing for some unrelated reason must not start interrogating
    /// the user about degraded mounts. escalate() decides not to ask before it
    /// touches a terminal, which is also what makes these two testable.
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
