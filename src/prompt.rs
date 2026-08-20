//! Putting a question to whoever is at the machine, during a mount.
//!
//! # Whether anyone can be asked is decided first
//!
//! [`Prompt::detect`] returns `None` when there is nobody to ask, and it is
//! called once, before any question exists. A caller without a `Prompt` takes
//! its safe path immediately rather than composing a question, asking it, and
//! discovering at the end that nothing was listening.
//!
//! That ordering matters for more than tidiness. A mount can need to ask more
//! than once - the degraded question, then the escalation if the kernel
//! refuses anyway - and resolving reachability per question means the second
//! one can silently have no audience after the first was answered. Deciding
//! once makes "there is a person here" a property of the mount.
//!
//! # Where the question goes
//!
//! Not "is stdin a terminal". At boot stdin *is* a terminal - it is
//! /dev/console - and writing a question there while plymouth owns the screen
//! puts it somewhere nobody will look. systemd's password agents exist exactly
//! to answer that: plymouth, the console agent, wall, and the credential store
//! for an unattended machine. So if the agent framework is running, it is the
//! way to reach a person, and `--no-tty` makes systemd-ask-password use it
//! even though we have a tty.
//!
//! # What the agent path can't carry
//!
//! The ask-password protocol is a key=value file (`/run/systemd/ask-password/
//! ask.*`) whose `Message=` is a single line, so [`Ask::detail`] - the list of
//! which devices are missing - only reaches a terminal. Verified by reading a
//! live ask file; the same file carries `Echo=1`, which is what keeps the
//! agents from masking a y/n answer as asterisks.
//!
//! # Not used by the passphrase prompt
//!
//! key.rs deliberately keeps its own path. It needs termios echo-off (with the
//! ICRNL/ICANON repair for an initramfs console no shell has configured),
//! zeroizing buffers, NUL-split multi-answer, and keyring/credential caching.
//! None of that is wanted by a policy question, and one type serving both would
//! be worse at each.

use std::io::{stdin, IsTerminal};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Result;
use log::debug;

/// systemd creates this when its password-agent machinery is running.
const ASK_PASSWORD_DIR: &str = "/run/systemd/ask-password";

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

/// --echo=yes because this is not a password. The default is `masked`, an
/// asterisk per character, and it also puts a lock-and-key emoji on the
/// prompt; someone deciding what to do about their data should be able to see
/// what they typed. --no-tty so the agents get it even though we have a tty.
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
