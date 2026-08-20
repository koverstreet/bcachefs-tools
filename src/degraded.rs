//! Answering degraded=ask.
//!
//! `degraded` is a superblock option with four values - ask/yes/very/no - and
//! the kernel implements three of them. `bch2_fs_may_start()` switches on
//! very and yes, and everything else, `ask` included, falls to the default
//! arm and refuses. That's correct for a kernel: it has no console and no
//! user to put a question to.
//!
//! So `ask` is ours. We turn it into a yes or a no here and pass that down
//! explicitly, and the kernel never sees an `ask` it would have to treat as a
//! refusal. Since `ask` is also the *default*, a multi-device filesystem that
//! loses a member is refused outright until something does this.
//!
//! Where the answer comes from, in order:
//!   - an explicit -o degraded=... on the command line wins; the user already
//!     answered, don't ask twice
//!   - a terminal: ask on it
//!   - no terminal (systemd's /dev/null stdin at boot): systemd-ask-password,
//!     which is also how the passphrase prompt reaches a user during boot, and
//!     which can be pre-answered by a credential on an unattended machine
//!   - nothing to ask with: refuse, and say why. Mounting degraded is a
//!     decision about data; making it silently by default is not ours to make.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Result;
use bcachefs_kernel::{c, opt_defined, opt_get};
use c::bch_opts;
use c::bch_sb_handle;
use log::{debug, warn};

use crate::key::StdinType;

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

fn question(missing: usize, expected: usize) -> String {
    format!(
        "Only {} of {expected} devices are present. \
         Mount degraded, without the missing {}?",
        expected - missing,
        if missing == 1 { "device" } else { "devices" },
    )
}

fn ask_on_terminal(q: &str) -> Result<bool> {
    use std::io::{stdin, stdout, Write};

    print!("{q} [y/N] ");
    stdout().flush()?;

    let mut answer = String::new();
    stdin().read_line(&mut answer)?;

    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// Ask through systemd, the way the passphrase prompt does. --timeout is a
/// real number here rather than the passphrase path's 0: a boot that stops to
/// ask a question nobody is there to answer has to end up somewhere, and for
/// this question the safe somewhere is "don't".
fn ask_via_systemd(q: &str, uuid: &str) -> Result<bool> {
    let out = Command::new("systemd-ask-password")
        .arg("--icon=drive-harddisk")
        .arg(format!("--id=bcachefs:UUID={uuid}"))
        .arg(format!("--timeout={}", PROMPT_TIMEOUT.as_secs()))
        .arg("-n")
        .arg(format!("{q} [y/N]"))
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;

    if !out.status.success() {
        debug!("systemd-ask-password declined or timed out");
        return Ok(false);
    }

    let answer = String::from_utf8_lossy(&out.stdout);
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
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
) -> Result<Option<String>> {
    let Some((_, first)) = sbs.first() else {
        return Ok(fs_opts);
    };

    let expected = first.sb().number_of_devices() as usize;
    if sbs.len() >= expected {
        return Ok(fs_opts);
    }

    let missing = expected - sbs.len();

    if degraded_action(sbs, cli_opts) != c::bch_degraded_actions::BCH_DEGRADED_ask as u8 {
        // yes/very/no: the kernel acts on these itself.
        return Ok(fs_opts);
    }

    let q = question(missing, expected);
    let uuid = first.sb().uuid().hyphenated().to_string();

    let yes = match StdinType::detect() {
        StdinType::Terminal => ask_on_terminal(&q)?,
        StdinType::DevNull => ask_via_systemd(&q, &uuid)?,
        StdinType::Other => {
            warn!("{q}");
            warn!("no terminal to ask on; refusing (mount -o degraded=yes to override)");
            false
        }
    };

    // warn, not info: the default verbosity is Warn, and everything here is a
    // decision about the user's data that we made for them. Without this the
    // only thing they see is the kernel's insufficient_devices_to_start, which
    // does not mention that a question was asked and answered on their behalf,
    // or that -o degraded=yes exists. This runs only when a device is missing,
    // so it is not chatter.
    warn!("mounting with degraded={}", if yes { "yes" } else { "no" });

    Ok(Some(append_opt(fs_opts, if yes { "degraded=yes" } else { "degraded=no" })))
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
    fn question_counts_what_is_present_not_what_is_missing() {
        assert!(question(1, 3).contains("2 of 3"));
        assert!(question(1, 3).contains("device?"));
        assert!(question(2, 3).contains("1 of 3"));
        assert!(question(2, 3).contains("devices?"));
    }
}
