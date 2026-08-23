//! Answering degraded=ask.
//!
//! bch2_fs_may_start() acts on yes and very and refuses everything else, `ask`
//! included - correct for a kernel, which has no user to ask. So `ask` is ours
//! to resolve, and since it is the default, a filesystem that loses a member is
//! refused outright until something here answers.
//!
//! The question is put *after* a refusal rather than before the mount, and that
//! is the whole shape of this file. What there is to consent to depends on
//! whether the data on the missing devices has another copy, which is a reading
//! of the replicas table against the devices actually here - the kernel's to
//! make, at the moment it decides. So we attempt the mount and let
//! bch2_fs_may_start() classify its own refusal, as
//! insufficient_devices_data_intact or insufficient_devices_data_lost.
//!
//! Attempting is cheap: bch2_fs_may_start() is the first thing
//! __bch2_fs_start() does, before recovery reads and sorts the journal, which
//! after an unclean shutdown is where the time goes. This would not be worth
//! doing if it were the other way round.
//!
//! The user is then told which of the two situations they are in, and asked
//! only what is theirs to decide: mount, mount read-only, or don't. Which
//! degraded= value that needs is ours to work out, not theirs.

use std::path::PathBuf;
use crate::prompt::{fs_name, Choice, Prompt, Question, Watch, NO_ONE_TO_ASK, PROMPT_TIMEOUT};

use anyhow::Result;
use bcachefs_kernel::errcode::{self, BchError};
use bcachefs_kernel::{c, opt_defined, opt_get};
use c::bch_opts;
use c::bch_sb_handle;
use log::warn;
use uuid::Uuid;

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

/// Which refusal we are answering, and so what is at stake.
///
/// The kernel decided this; we only relay it. Recomputing it here would mean
/// reading the superblock's replicas table against the devices we found and
/// getting the same answer - or, one version skew later, a different one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Situation {
    /// Every replica set still has a readable copy. Less redundancy, no holes.
    DataIntact,
    /// Some does not.
    DataLost,
}

impl Situation {
    fn of(err: &BchError) -> Option<Situation> {
        if err.matches(errcode::insufficient_devices_data_intact) {
            Some(Situation::DataIntact)
        } else if err.matches(errcode::insufficient_devices_data_lost) {
            Some(Situation::DataLost)
        } else {
            None
        }
    }

    /// What the kernel has to be told before it will go ahead - the half of the
    /// decision the user should not have to know about.
    fn opt(self) -> &'static str {
        match self {
            Situation::DataIntact => "degraded=yes",
            Situation::DataLost   => "degraded=very",
        }
    }

    /// What it costs, and the question that follows from it. Neither says which
    /// devices: the count is in front of this and bch2_fs_may_start() has
    /// already listed them, with everything it knows about each.
    fn line(self) -> (&'static str, &'static str) {
        match self {
            Situation::DataIntact =>
                ("All your data is still readable, with less redundancy than it should have.",
                 "Mount?"),
            Situation::DataLost =>
                ("Some of your data has no other copy, and reads of it will fail.",
                 "Mount anyway?"),
        }
    }
}

/// What the person at the machine gets to decide. Not *whether* data is
/// missing - we know that, and telling them is our job, not theirs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    No,
    Yes,
    /// Mounting read-write re-replicates onto the surviving devices, which can
    /// fill them and is not easy to back out of; and someone salvaging what is
    /// left does not need the filesystem writable to copy it off.
    ReadOnly,
}

const CHOICES: &[Choice<Answer>] = &[
    Choice { key: 'y', aliases: &["yes"], short: "",
             blurb: "mount", answer: Answer::Yes },
    Choice { key: 'r', aliases: &["ro", "readonly"], short: "read-only",
             blurb: "mount read-only: nothing gets written or re-replicated",
             answer: Answer::ReadOnly },
    Choice { key: 'n', aliases: &["no"], short: "",
             blurb: "don't mount", answer: Answer::No },
];

/// What to do with the mount that was refused.
pub enum Outcome {
    /// Attempt it again with these.
    Mount {
        fs_opt:    &'static str,
        read_only: bool,
    },
    /// A member turned up while the question was up. Scan again - the device
    /// list we were mounting with is now short one device that is here.
    Rescan,
    /// Leave it refused: they said no, nobody was there to ask, or the refusal
    /// was not one of ours to put to them.
    No,
}

/// Everything the degraded question needs, taken from the scan before the
/// superblocks are closed.
///
/// Held across the mount attempt that provokes it, which is why it holds no
/// `bch_sb_handle`: those are open block devices, and the kernel wants to open
/// them itself.
pub struct Ask {
    name:     String,
    uuid:     Uuid,
    missing:  usize,
    expected: usize,
    opts:     bch_opts,
    use_udev: bool,
}

impl Ask {
    /// `None` when there is nothing here for us to decide: every member is
    /// present, or the filesystem's degraded action is not `ask`, in which case
    /// the kernel acts on it itself and an explicit `-o degraded=` is the
    /// user's decision already.
    pub fn new(sbs: &[(PathBuf, bch_sb_handle)], cli_opts: &bch_opts) -> Option<Ask> {
        let (_, first) = sbs.first()?;

        // By device, not by path: see device_scan::present_devices(). A member
        // found twice - multipath, or udev and the block scan both contributing
        // - would otherwise make up the count for one that is missing.
        let expected = device_scan::expected_devices(sbs);
        let present  = device_scan::present_devices(sbs).len();

        if present >= expected
            || degraded_action(sbs, cli_opts) != c::bch_degraded_actions::BCH_DEGRADED_ask as u8
        {
            return None;
        }

        Some(Ask {
            name:     fs_name(first),
            uuid:     first.sb().uuid(),
            missing:  expected - present,
            expected,
            opts:     *cli_opts,
            use_udev: opt_get!(cli_opts, mount_trusts_udev) != 0,
        })
    }

    /// Which filesystem, how much of it is gone, and what that costs. Whose
    /// filesystem it is matters: at boot there can be several, and the systemd
    /// prompt is one line with no other context.
    fn question(&self, s: Situation) -> String {
        let (costs, q) = s.line();

        format!("Filesystem {} is missing {} of its {} devices. {costs} {q}",
                self.name, self.missing, self.expected)
    }

    /// Put the question, if @err is one we know how to ask about.
    ///
    /// The devices themselves are not named here: bch2_fs_may_start() already
    /// printed them, along with everything it knows about each, on its way to
    /// throwing @err.
    pub fn put(&self, err: &BchError) -> Result<Outcome> {
        let Some(s) = Situation::of(err) else {
            return Ok(Outcome::No);
        };

        let q = self.question(s);

        // Mounting degraded is a decision about data, so with nobody there to
        // make it we say what we would have asked, and refuse. One warning,
        // because this is one event: the question, and what we did instead.
        let Some(p) = Prompt::detect() else {
            warn!("{q}\n{NO_ONE_TO_ASK}; refusing (mount -o {} to allow it)", s.opt());
            return Ok(Outcome::No);
        };

        // If the missing device turns up while the question is on screen, that
        // is the answer: stop asking and go back for a fresh device list.
        let mut dw = device_scan::DeviceWatch::new(self.uuid, &self.opts, self.use_udev);
        let watch = dw.as_mut().map(|d| d as &mut dyn Watch);

        let id = self.uuid.hyphenated().to_string();

        let answer = p.put(&Question {
            prompt:  &q,
            choices: CHOICES,
            silence: Answer::No,
            alarm:   s == Situation::DataLost,
            uuid:    &id,
            timeout: Some(PROMPT_TIMEOUT),
        }, watch)?;

        let Some(answer) = answer else {
            warn!("device turned up while asking; mounting normally");
            return Ok(Outcome::Rescan);
        };

        if answer == Answer::No {
            return Ok(Outcome::No);
        }

        let read_only = answer == Answer::ReadOnly;

        // warn, not info: the default verbosity is Warn, and this is a decision
        // about the user's data. Otherwise all they see of it is the kernel's
        // refusal, which mentions neither that a question was answered nor what
        // it was answered with.
        warn!("mounting {}with {}",
              if read_only { "read-only " } else { "" }, s.opt());

        Ok(Outcome::Mount { fs_opt: s.opt(), read_only })
    }
}

/// Append one option to a mount option string, which may be absent or empty -
/// a stray leading comma is a parse error, not a cosmetic problem.
pub fn append_opt(fs_opts: Option<String>, opt: &str) -> String {
    match fs_opts {
        Some(o) if !o.is_empty() => format!("{o},{opt}"),
        _ => opt.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(prompt: &str) -> Question<'_, Answer> {
        Question {
            prompt, choices: CHOICES,
            silence: Answer::No, alarm: false, uuid: "", timeout: None,
        }
    }

    fn parse(reply: &str) -> Answer {
        q("").parse(reply)
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

    /// Deriving both rendered forms from the answer list is what stops them
    /// drifting apart, but it also means a change to the derivation silently
    /// rewords every prompt. The capitalised answer is how a reader knows what
    /// Enter will do.
    #[test]
    fn the_summary_names_the_answers_and_marks_the_default() {
        assert_eq!(q("").brief(), "[y / r=read-only / N]");

        assert_eq!(q("").lines(), [
            "  y  mount",
            "  r  mount read-only: nothing gets written or re-replicated",
            "  n  don't mount",
        ]);
    }

    #[test]
    fn answer_parses_the_three_things_there_are_to_say() {
        for s in ["y", "Y", "yes", "Yes", " y \n"] {
            assert_eq!(parse(s), Answer::Yes, "{s:?}");
        }
        for s in ["r", "R", "ro", "readonly", "ReadOnly"] {
            assert_eq!(parse(s), Answer::ReadOnly, "{s:?}");
        }
    }

    /// Anything not positively recognised as consent is a refusal - including
    /// an empty answer, which is what a bare Enter and a timed-out systemd
    /// prompt both produce.
    ///
    /// `f`, `force` and `very` are in there because they used to be answers:
    /// the question no longer asks how hard to try, and a user typing what an
    /// older mount taught them must not get a *more* permissive mount than
    /// they would by saying yes.
    #[test]
    fn answer_defaults_to_refusing() {
        for s in ["", "\n", "n", "N", "no", "q", "yolo", "degraded",
                  "f", "force", "very"] {
            assert_eq!(parse(s), Answer::No, "{s:?}");
        }
    }

    /// The situation decides how hard the kernel is told to try; the user
    /// decides only whether to mount. Getting this backwards would either
    /// refuse a mount that was consented to, or force one that wasn't.
    #[test]
    fn how_hard_to_try_comes_from_the_situation_not_the_answer() {
        assert_eq!(Situation::DataIntact.opt(), "degraded=yes");
        assert_eq!(Situation::DataLost.opt(),   "degraded=very");
    }

    /// A mount failing for some unrelated reason must not start interrogating
    /// the user about degraded mounts - and Situation::of() is what decides
    /// that, before anything touches a terminal.
    #[test]
    fn only_the_two_refusals_we_know_about_are_ours_to_ask_about() {
        let lost   = BchError::from_errcode(errcode::insufficient_devices_data_lost);
        let intact = BchError::from_errcode(errcode::insufficient_devices_data_intact);

        assert_eq!(Situation::of(&lost),   Some(Situation::DataLost));
        assert_eq!(Situation::of(&intact), Some(Situation::DataIntact));

        for e in [
            // The parent, which a kernel that hasn't got the split still
            // throws: we cannot tell which situation it is, so we don't ask.
            BchError::from_errcode(errcode::insufficient_devices_to_start),
            BchError::from_errcode(errcode::EINVAL_opt_parse_str_required),
            // A flattened errno, which is what the mount(2) fallback gives us:
            // the same EINVAL a typo'd option produces.
            BchError::from_raw(libc::EINVAL),
            BchError::from_raw(0),
            // A code from a kernel module newer than this binary - the normal
            // state of affairs for a filesystem that ships DKMS-only. The
            // errcode parent-chain walk BUG_ON()s rather than bounds-checking,
            // so what's being asserted here is as much "doesn't abort" as
            // "doesn't match".
            BchError::from_raw(c::bch_errcode::BCH_ERR_MAX as i32 + 1),
        ] {
            assert_eq!(Situation::of(&e), None, "{e:?}");
        }
    }

    #[test]
    fn the_question_says_whose_filesystem_and_what_is_at_stake() {
        let ask = Ask {
            name: "home".into(), uuid: Uuid::nil(),
            missing: 1, expected: 3,
            opts: Default::default(), use_udev: false,
        };

        let intact = ask.question(Situation::DataIntact);
        assert!(intact.contains("home"));
        assert!(intact.contains("1 of its 3"));
        assert!(intact.contains("still readable"));

        assert!(ask.question(Situation::DataLost).contains("reads of it will fail"));
    }
}
