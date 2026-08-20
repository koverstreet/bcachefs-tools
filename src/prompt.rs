//! Putting a question to whoever is at the machine, during a mount.
//!
//! Not "is stdin a terminal": at boot stdin *is* one - it's /dev/console - but
//! plymouth owns the screen and a question written there is never seen.
//! systemd's password agents are for exactly this (plymouth, the console
//! agent, wall, and the credential store for unattended machines), and
//! --no-tty is what reaches them when we do have a tty.
//!
//! [`Prompt::detect`] gives `None` when nobody can be reached, and callers
//! resolve that before composing a question: a mount can ask twice - degraded,
//! then escalation - and deciding per question lets the second one silently
//! have no audience after the first was answered.
//!
//! [`Ask::detail`] reaches a terminal and nothing else; the agent protocol's
//! `Message=` is one line.
//!
//! key.rs keeps its own path: a passphrase needs termios echo-off with the
//! ICRNL/ICANON repair for an unconfigured initramfs console, zeroizing, and
//! keyring caching. A policy question needs none of it.

use std::io::{stdin, IsTerminal};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Result;
use bcachefs_kernel::c::bch_sb_handle;
use log::debug;

/// systemd creates this when its password-agent machinery is running.
const ASK_PASSWORD_DIR: &str = "/run/systemd/ask-password";

/// What to call a filesystem when asking about it: its label, or its UUID.
///
/// Here rather than with the superblock code because the reason is about
/// prompting: several filesystems come up at once at boot, and someone facing
/// two questions needs to see whether they're about the same disk.
pub fn fs_name(sb: &bch_sb_handle) -> String {
    let label = String::from_utf8_lossy(sb.sb().label());

    if label.is_empty() {
        sb.sb().uuid().hyphenated().to_string()
    } else {
        label.into_owned()
    }
}

/// One question, in both the forms its destinations need.
pub struct Ask<'a> {
    /// The question. One line: the agent protocol's `Message=` is one line.
    pub prompt: &'a str,
    /// Detail that only a multi-line destination can show. Dropped on the
    /// agent path - see the module header.
    pub detail: Option<&'a str>,
    /// One line per answer, spelled out. A terminal has room for these.
    pub choices: &'a [&'a str],
    /// The bracketed summary, for a destination that only gets one line.
    pub brief: &'a str,
    /// What identifies this filesystem to the agent, for `--id`.
    pub id: &'a str,
    /// How long to wait for a person before giving up.
    pub timeout: Duration,
}

/// Somewhere a question can actually reach someone.
///
/// Copy so a caller can keep it for a later question - see degraded.rs's
/// Retry. Resolving once and carrying it is the point.
#[derive(Clone, Copy)]
pub enum Prompt {
    /// systemd's password agents.
    Agent,
    /// A terminal we can write to and read from directly.
    Terminal,
}

impl Prompt {
    /// `None` when there is nobody to ask: no agent framework and no terminal.
    /// Callers must have a safe answer for that and should take it without
    /// composing a question.
    pub fn detect() -> Option<Prompt> {
        if Path::new(ASK_PASSWORD_DIR).is_dir() && have_ask_password() {
            debug!("asking via systemd's password agents");
            return Some(Prompt::Agent);
        }

        if stdin().is_terminal() {
            debug!("asking on the terminal");
            return Some(Prompt::Terminal);
        }

        debug!("no password agent and no terminal: nobody to ask");
        None
    }

    /// Put the question. `None` means nobody answered - declined, or timed
    /// out. Callers treat that as their safe answer, not as an error.
    pub fn ask(&self, ask: &Ask<'_>) -> Result<Option<String>> {
        match self {
            Prompt::Agent    => ask_via_agent(ask),
            Prompt::Terminal => ask_on_terminal(ask).map(Some),
        }
    }
}

fn have_ask_password() -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .any(|dir| dir.join("systemd-ask-password").is_file())
        })
        .unwrap_or(false)
}

/// --echo=yes because this isn't a password: the default is `masked`, an
/// asterisk per character plus a lock-and-key emoji.
fn ask_via_agent(ask: &Ask<'_>) -> Result<Option<String>> {
    let out = Command::new("systemd-ask-password")
        .arg("--no-tty")
        .arg("--echo=yes")
        .arg("--icon=drive-harddisk")
        .arg(format!("--id={}", ask.id))
        .arg(format!("--timeout={}", ask.timeout.as_secs()))
        .arg("-n")
        .arg(format!("{} {}", ask.prompt, ask.brief))
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;

    if !out.status.success() {
        debug!("systemd-ask-password declined or timed out");
        return Ok(None);
    }

    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

fn ask_on_terminal(ask: &Ask<'_>) -> Result<String> {
    use std::io::{stdout, Write};

    println!("{}", ask.prompt);
    if let Some(detail) = ask.detail {
        print!("{detail}");
    }
    for choice in ask.choices {
        println!("{choice}");
    }
    print!("{} ", ask.brief);
    stdout().flush()?;

    let mut answer = String::new();
    stdin().read_line(&mut answer)?;

    Ok(answer)
}
