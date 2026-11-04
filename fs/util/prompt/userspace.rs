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
//! src/key.rs keeps its own path - a passphrase needs termios echo-off with the
//! ICRNL/ICANON repair for an unconfigured initramfs console, zeroizing, and
//! keyring caching.

use std::io::{stdin, IsTerminal, Read};
use std::os::fd::{AsFd, BorrowedFd, RawFd};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use std::io::Result;
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use log::debug;

use super::{Question, Reply};

const ASK_PASSWORD_DIR: &str = "/run/systemd/ask-password";

/// Its label, or its UUID - several filesystems come up at once at boot, and
/// someone facing two questions needs to see whether they are the same disk.
pub fn fs_name(sb: &crate::c::bch_sb_buf) -> String {
    let label = String::from_utf8_lossy(sb.sb().label());

    if label.is_empty() {
        sb.sb().uuid().hyphenated().to_string()
    } else {
        label.into_owned()
    }
}

impl<A: Copy + PartialEq> Question<'_, A> {
    pub fn lines(&self) -> Vec<String> {
        self.choices.iter()
            .filter(|c| !c.blurb.is_empty())
            .map(|c| format!("  {}  {}", c.key, c.blurb))
            .collect()
    }

    /// The answer silence resolves to is the one shown capitalised.
    pub fn brief(&self) -> String {
        let parts = self.choices.iter().map(|c| {
            if Some(c.answer) == self.silence {
                c.key.to_uppercase().to_string()
            } else if c.short.is_empty() {
                c.key.to_string()
            } else {
                format!("{}={}", c.key, c.short)
            }
        });

        format!("[{}]", parts.collect::<Vec<_>>().join(" / "))
    }
}

/// The rendered question, in both the forms its destinations need.
struct Ask<'a> {
    prompt: &'a str,
    choices: Vec<String>,
    brief: String,
    alarm: bool,
    id: String,
    timeout: Option<Duration>,
}

/// Something that can settle a question by happening.
///
/// Someone asked whether to mount without a disk, who responds by plugging the
/// disk in, has answered - and so has someone who supplies the passphrase over
/// a socket while the terminal prompt is up. Either way the question is over,
/// and what settled it is the watch's to say.
pub trait Watch {
    fn raw_fd(&self) -> RawFd;
    /// Readiness alone is not an answer - any block device arriving wakes a
    /// device watch, and most of them are somebody else's - so this decides
    /// whether *this* wakeup settled anything.
    fn settled(&mut self) -> bool;
}

/// What ended a [`wait`].
pub enum Waited {
    Readable,
    Settled,
    Timeout,
}

enum Answer {
    Said(String),
    /// Timed out, or declined.
    Silence,
    /// Input ended.
    Ended,
    Settled,
}

/// Copy so one detection serves a later question too - see degraded.rs's Retry.
#[derive(Clone, Copy)]
pub enum Prompt {
    Agent,
    /// stdin: a terminal, or a script's pipe or file with its answers.
    Stdin,
}

/// Plymouth answering a ping is an agent itself; stdin on /dev/null is what a
/// unit started by init gets, which puts us in the boot where the agents live.
fn agent_plausibly_listening(tty: bool) -> bool {
    (tty || stdin_is_dev_null()) && ask_password_installed()
}

/// Why [`Prompt::detect`] came back `None`, for the caller to put in front of
/// its own refusal.
///
/// There is one such reason because there is one such return: reaching it
/// means stdin is /dev/null - a terminal, a pipe or a file would have been
/// asked on - and that no agent was plausibly listening either. Somebody
/// reading this in a boot log needs the concrete condition, not "nobody was
/// there": the fix is a console or an installed systemd-ask-password, and which
/// one is what they have to work out.
pub const NO_ONE_TO_ASK: &str = "nothing on stdin to answer, and no systemd password agent";

impl Prompt {
    /// `None` when nobody can be reached; callers take their safe answer
    /// without composing a question, and say [`NO_ONE_TO_ASK`] when they do.
    ///
    /// Decided once: what we were started with doesn't change.
    pub fn detect() -> Option<Prompt> {
        static DETECTED: OnceLock<Option<Prompt>> = OnceLock::new();
        *DETECTED.get_or_init(Self::detect_uncached)
    }

    fn detect_uncached() -> Option<Prompt> {
        let tty = stdin().is_terminal();

        if tty && !crate::util::plymouth::active() {
            debug!("asking on the terminal");
            return Some(Prompt::Stdin);
        }

        if agent_plausibly_listening(tty) {
            debug!("asking via systemd's password agents");
            return Some(Prompt::Agent);
        }

        // Under the splash with no agent to draw on it: printing here is poor,
        // but better than refusing without asking.
        if tty {
            debug!("asking on the terminal (plymouth is up but has no agent)");
            return Some(Prompt::Stdin);
        }

        // A script's: its answers are on stdin.
        if rustix::fs::fstat(stdin().as_fd()).is_ok() && !stdin_is_dev_null() {
            debug!("asking on stdin");
            return Some(Prompt::Stdin);
        }

        debug!("{NO_ONE_TO_ASK}");
        None
    }

    /// Put the question and interpret the reply: one of the question's own
    /// answers, silence included, so a caller never has to decide what an
    /// unrecognised reply meant.
    pub fn put<A: Copy + PartialEq>(
        &self,
        q: &Question<'_, A>,
        mut watch: Option<&mut dyn Watch>,
    ) -> Result<Reply<A>> {
        let ask = Ask {
            prompt:  q.prompt,
            choices: q.lines(),
            brief:   q.brief(),
            alarm:   q.alarm,
            id:      format!("bcachefs:UUID={}", q.uuid),
            timeout: q.timeout,
        };

        loop {
            let watch = watch.as_mut().map(|w| &mut **w as &mut dyn Watch);

            return Ok(match match self {
                Prompt::Agent => ask_via_agent(&ask, watch),
                Prompt::Stdin => ask_on_terminal(&ask, watch),
            }? {
                Answer::Said(reply) => match q.parse(&reply) {
                    Some(a) => Reply::Answer(a),
                    None    => continue,
                },
                Answer::Silence => q.silence.map_or(Reply::Unanswered, Reply::Answer),
                Answer::Ended   => Reply::Unanswered,
                Answer::Settled => Reply::Settled,
            });
        }
    }
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

/// Block until @fd is readable, the deadline passes, or @watch settles the
/// question - and only then: a watch firing is not by itself an answer.
///
/// Callers that poll a terminal want it already in ICANON: then readable means a
/// whole line has been typed, and the read that follows will not block either.
pub fn wait(
    fd: BorrowedFd<'_>,
    timeout: Option<Duration>,
    mut watch: Option<&mut dyn Watch>,
) -> Result<Waited> {
    let start = Instant::now();

    loop {
        let deadline = timeout.map(|t| t.checked_sub(start.elapsed()).unwrap_or_default());

        // SAFETY: the watch outlives this call, so its descriptor does too.
        let watched = watch.as_ref()
            .map(|w| unsafe { BorrowedFd::borrow_raw(w.raw_fd()) });

        let mut fds = match watched.as_ref() {
            Some(w) => vec![PollFd::new(&fd, PollFlags::IN), PollFd::new(w, PollFlags::IN)],
            None => vec![PollFd::new(&fd, PollFlags::IN)],
        };

        let spec = deadline.map(|d| Timespec {
            tv_sec:  d.as_secs() as _,
            tv_nsec: d.subsec_nanos() as _,
        });

        if poll(&mut fds, spec.as_ref())? == 0 {
            debug!("nobody answered within the timeout");
            return Ok(Waited::Timeout);
        }

        // Read the readiness out before touching @watch again: the poll set
        // borrows its fd.
        let answered = fds[0].revents().intersects(PollFlags::IN | PollFlags::HUP);
        let stirred  = fds.len() > 1 && fds[1].revents().contains(PollFlags::IN);
        drop(fds);

        if answered {
            return Ok(Waited::Readable);
        }

        if stirred && watch.as_mut().is_some_and(|w| w.settled()) {
            debug!("a watch settled the question while it was up");
            return Ok(Waited::Settled);
        }
    }
}

/// --echo=yes because this isn't a password: the default is `masked`, an
/// asterisk per character plus a lock-and-key emoji.
///
/// Spawned rather than run to completion so it can be killed: with a watch,
/// the question can stop applying while the agent is still displaying it, and
/// leaving it on someone's screen after the fact is worse than not asking.
fn ask_via_agent(ask: &Ask<'_>, watch: Option<&mut dyn Watch>) -> Result<Answer> {
    let mut child = Command::new("systemd-ask-password")
        .arg("--no-tty")
        .arg("--echo=yes")
        .arg("--icon=drive-harddisk")
        .arg(format!("--id={}", ask.id))
        // 0 is systemd-ask-password's own spelling of "wait indefinitely".
        .arg(format!("--timeout={}", ask.timeout.map_or(0, |t| t.as_secs())))
        .arg("-n")
        .arg(format!("{} {}", ask.prompt, ask.brief))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let mut stdout = child.stdout.take().expect("stdout is piped");

    // systemd-ask-password enforces its own --timeout, so let it: passing None
    // here means we wait on the child rather than racing it.
    if let Waited::Settled = wait(stdout.as_fd(), None, watch)? {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(Answer::Settled);
    }

    let mut buf = String::new();
    stdout.read_to_string(&mut buf)?;
    if !child.wait()?.success() {
        debug!("systemd-ask-password declined or timed out");
        return Ok(Answer::Silence);
    }
    Ok(Answer::Said(buf))
}

fn ask_on_terminal(ask: &Ask<'_>, watch: Option<&mut dyn Watch>) -> Result<Answer> {
    use std::io::{stdout, Write};
    use owo_colors::OwoColorize;

    // stdout, not stdin: the question is read where it is drawn, and the two
    // can be different files. With no choices to list, the summary goes on the
    // prompt's line.
    let sep = if ask.choices.is_empty() { " " } else { "\n" };
    if ask.alarm && stdout().is_terminal() {
        print!("{}{sep}", ask.prompt.red().bold());
    } else {
        print!("{}{sep}", ask.prompt);
    }
    for choice in &ask.choices {
        println!("{choice}");
    }
    print!("{} ", ask.brief);
    stdout().flush()?;

    let answer = match wait(stdin().as_fd(), ask.timeout, watch)? {
        Waited::Readable => {
            let mut answer = String::new();
            if stdin().read_line(&mut answer)? > 0 {
                return Ok(Answer::Said(answer));
            }
            Answer::Ended
        }
        Waited::Timeout => Answer::Silence,
        Waited::Settled => Answer::Settled,
    };
    // Leave the half-written prompt behind a newline rather than letting
    // whatever comes next start mid-line.
    println!();
    Ok(answer)
}
