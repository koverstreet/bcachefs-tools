//! Putting a question to whoever is at the machine, during a mount.
//!
//! Two facts about the boot shape the whole module. At boot stdin *is* a
//! terminal - /dev/console - but plymouth owns the screen, so a question
//! written there is never seen; systemd's password agents include one that
//! draws on the splash. And the agents are boot-time units, so on a running
//! system /run/systemd/ask-password exists with nothing listening, and a
//! question posted to it times out having shown the user nothing (measured:
//! all three agents inactive, `--no-tty` returns "Timer expired").
//!
//! Hence [`agent_plausibly_listening`]: there is no direct test, so we want
//! evidence before handing a question over.
//!
//! key.rs keeps its own path - a passphrase needs termios echo-off with the
//! ICRNL/ICANON repair for an unconfigured initramfs console, zeroizing, and
//! keyring caching.

use std::io::{stdin, IsTerminal};
use std::os::fd::AsFd;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Result;
use bcachefs_kernel::c::bch_sb_handle;
use log::debug;

const ASK_PASSWORD_DIR: &str = "/run/systemd/ask-password";

pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// Its label, or its UUID - several filesystems come up at once at boot, and
/// someone facing two questions needs to see whether they are the same disk.
pub fn fs_name(sb: &bch_sb_handle) -> String {
    let label = String::from_utf8_lossy(sb.sb().label());

    if label.is_empty() {
        sb.sb().uuid().hyphenated().to_string()
    } else {
        label.into_owned()
    }
}

/// The only place an answer is written down: both rendered forms and the parse
/// derive from this, so a question cannot offer a letter it will not accept.
pub struct Choice<A> {
    /// Matched case-insensitively, as are the aliases.
    pub key: char,
    pub aliases: &'static [&'static str],
    /// For the bracketed summary; "" shows the bare key.
    pub short: &'static str,
    /// For a terminal, which has room for a sentence.
    pub blurb: &'static str,
    pub answer: A,
}

/// Answers live per question rather than in one shared parser, so that two
/// questions cannot cross-parse each other's vocabulary.
pub struct Question<'a, A> {
    /// One line: the agent protocol's `Message=` is one line.
    pub prompt: &'a str,
    /// Reaches a terminal only, for the same reason.
    pub detail: Option<&'a str>,
    pub choices: &'a [Choice<A>],
    /// Also what a bare Enter and a timed-out boot prompt mean. Shown
    /// capitalised in the summary.
    pub silence: A,
    pub uuid: &'a str,
    /// How long to wait for a person before giving up.
    pub timeout: Duration,
}

impl<A: Copy + PartialEq> Question<'_, A> {
    pub fn lines(&self) -> Vec<String> {
        self.choices.iter()
            .map(|c| format!("  {}  {}", c.key, c.blurb))
            .collect()
    }

    /// The answer silence resolves to is the one shown capitalised.
    pub fn brief(&self) -> String {
        let parts = self.choices.iter().map(|c| {
            if c.answer == self.silence {
                c.key.to_uppercase().to_string()
            } else if c.short.is_empty() {
                c.key.to_string()
            } else {
                format!("{}={}", c.key, c.short)
            }
        });

        format!("[{}]", parts.collect::<Vec<_>>().join(" / "))
    }

    pub fn parse(&self, reply: &str) -> A {
        let reply = reply.trim();

        self.choices.iter()
            .find(|c| {
                reply.eq_ignore_ascii_case(&c.key.to_string())
                    || c.aliases.iter().any(|a| reply.eq_ignore_ascii_case(a))
            })
            .map_or(self.silence, |c| c.answer)
    }
}

/// The rendered question, in both the forms its destinations need.
struct Ask<'a> {
    prompt: &'a str,
    detail: Option<&'a str>,
    choices: Vec<String>,
    brief: String,
    id: String,
    timeout: Duration,
}

/// Copy so one detection serves a later question too - see degraded.rs's Retry.
#[derive(Clone, Copy)]
pub enum Prompt {
    Agent,
    Terminal,
}

/// Plymouth answering a ping is an agent itself; stdin on /dev/null is what a
/// unit started by init gets, which puts us in the boot where the agents live.
/// A pipe or a file is a script redirecting us, with nobody behind it.
fn agent_plausibly_listening(tty: bool) -> bool {
    (tty || stdin_is_dev_null()) && ask_password_installed()
}

impl Prompt {
    /// `None` when nobody can be reached; callers take their safe answer
    /// without composing a question.
    pub fn detect() -> Option<Prompt> {
        let tty = stdin().is_terminal();

        if tty && !plymouth_active() {
            debug!("asking on the terminal");
            return Some(Prompt::Terminal);
        }

        if agent_plausibly_listening(tty) {
            debug!("asking via systemd's password agents");
            return Some(Prompt::Agent);
        }

        // Under the splash with no agent to draw on it: printing here is poor,
        // but better than refusing without asking.
        if tty {
            debug!("asking on the terminal (plymouth is up but has no agent)");
            return Some(Prompt::Terminal);
        }

        debug!("no password agent and no terminal: nobody to ask");
        None
    }

    /// Put the question and interpret the reply.
    ///
    /// Always one of the question's own answers, silence included, so a caller
    /// never has to decide what an unrecognised reply meant.
    pub fn put<A: Copy + PartialEq>(&self, q: &Question<'_, A>) -> Result<A> {
        let ask = Ask {
            prompt:  q.prompt,
            detail:  q.detail,
            choices: q.lines(),
            brief:   q.brief(),
            id:      format!("bcachefs:UUID={}", q.uuid),
            timeout: q.timeout,
        };

        let reply = match self {
            Prompt::Agent    => ask_via_agent(&ask)?,
            Prompt::Terminal => Some(ask_on_terminal(&ask)?),
        };

        Ok(reply.map_or(q.silence, |reply| q.parse(&reply)))
    }
}

/// Is plymouth drawing over the console? `plymouth --ping` is the canonical
/// test and exits non-zero when it isn't running; not installed means not
/// covering us, so a spawn failure is the same answer.
fn plymouth_active() -> bool {
    Command::new("plymouth")
        .arg("--ping")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Installed, note - not listening. detect() supplies the evidence for that.
fn ask_password_installed() -> bool {
    Path::new(ASK_PASSWORD_DIR).is_dir()
        && std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .any(|dir| dir.join("systemd-ask-password").is_file())
            })
            .unwrap_or(false)
}

/// Stdin being /dev/null is the signature of a process started by init: a
/// terminal means a person, a pipe or a file means a script, /dev/null means
/// neither, so the question has to go wherever init's own prompts go.
pub fn stdin_is_dev_null() -> bool {
    let Ok(stat) = rustix::fs::fstat(stdin().as_fd()) else {
        return false;
    };

    rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::CharacterDevice
        && rustix::fs::major(stat.st_rdev) == 1
        && rustix::fs::minor(stat.st_rdev) == 3
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
    for choice in &ask.choices {
        println!("{choice}");
    }
    print!("{} ", ask.brief);
    stdout().flush()?;

    let mut answer = String::new();
    stdin().read_line(&mut answer)?;

    Ok(answer)
}
