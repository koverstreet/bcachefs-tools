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
//! A passphrase is [`read_passphrase`]'s: read with echo off and the
//! ICRNL/ICANON repair for an unconfigured initramfs console, zeroized, and
//! through the agents with their keyring caching.

use std::io::{self, stdin, BufRead, IsTerminal, Read};
use std::os::fd::{AsFd, BorrowedFd, RawFd};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use std::io::Result;
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::termios;
use log::debug;
use zeroize::Zeroizing;

use super::{Question, Reply, UNANSWERED, US};
use crate::util::Printbuf;

const ASK_PASSWORD_DIR: &str = "/run/systemd/ask-password";

/// Its label, or its UUID - several filesystems come up at once at boot, and
/// someone facing two questions needs to see whether they are the same disk.
pub fn fs_name(sb: &crate::c::bch_sb) -> String {
    let label = String::from_utf8_lossy(sb.label());

    if label.is_empty() {
        sb.uuid().hyphenated().to_string()
    } else {
        label.into_owned()
    }
}

/// The reply line to a question the kernel framed - @rec, between
/// RECORD_START and RECORD_END - put as any of ours are.
pub fn answer_record(rec: &[u8]) -> Result<String> {
    let rec = std::str::from_utf8(rec).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let mut ask = Ask::default();
    let mut silence = false;
    for f in rec.split(US) {
        let Some((name, v)) = f.split_once('=') else { continue };
        match name {
            "prompt"  => ask.prompt = v,
            "lines"   => ask.lines = v,
            "brief"   => ask.brief = v,
            "silence" => silence = true,
            "alarm"   => ask.alarm = true,
            "uuid"    => ask.uuid = v,
            "timeout" => ask.timeout = v.parse().ok().map(Duration::from_secs),
            // a newer kernel's
            _ => {}
        }
    }

    let answer = match Prompt::detect() {
        Some(p) => p.ask(&ask, None)?,
        None    => Answer::Ended,
    };
    Ok(match answer {
        Answer::Said(reply)        => reply.trim_end_matches(['\n', '\r']).to_string(),
        Answer::Silence if silence => String::new(),
        _                          => UNANSWERED.to_string(),
    })
}

/// A question as it's shown, by us or by the kernel's relay.
#[derive(Default)]
struct Ask<'a> {
    prompt:  &'a str,
    lines:   &'a str,
    brief:   &'a str,
    alarm:   bool,
    uuid:    &'a str,
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
enum Waited {
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
        let (mut lines, mut brief) = (Printbuf::new(), Printbuf::new());
        q.lines(&mut lines);
        q.brief(&mut brief);
        let ask = Ask {
            prompt:  q.prompt,
            lines:   lines.as_str(),
            brief:   brief.as_str(),
            alarm:   q.alarm,
            uuid:    q.uuid,
            timeout: q.timeout,
        };

        loop {
            let watch = watch.as_mut().map(|w| &mut **w as &mut dyn Watch);

            return Ok(match self.ask(&ask, watch)? {
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

    fn ask(&self, ask: &Ask<'_>, watch: Option<&mut dyn Watch>) -> Result<Answer> {
        match self {
            Prompt::Agent => ask_via_agent(ask, watch),
            Prompt::Stdin => ask_on_terminal(ask, watch),
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
fn stdin_is_dev_null() -> bool {
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
fn wait(
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

/// systemd-ask-password, as every question is put through it: the message is
/// the caller's, last.
fn ask_password(id: &str, timeout: Option<Duration>) -> Command {
    let mut command = Command::new("systemd-ask-password");
    command
        // Reached either with no tty, where this is a no-op (measured:
        // identical timeout behaviour with and without), or with one
        // plymouth is covering, where it is what stops us prompting
        // onto the invisible terminal we just declined to use.
        .arg("--no-tty")
        .arg("--icon=drive-harddisk")
        .arg(format!("--id={id}"))
        // 0 is systemd-ask-password's own spelling of "wait indefinitely".
        .arg(format!("--timeout={}", timeout.map_or(0, |t| t.as_secs())))
        .arg("-n")
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}

/// --echo=yes because this isn't a password: the default is `masked`, an
/// asterisk per character plus a lock-and-key emoji.
///
/// Spawned rather than run to completion so it can be killed: with a watch,
/// the question can stop applying while the agent is still displaying it, and
/// leaving it on someone's screen after the fact is worse than not asking.
fn ask_via_agent(ask: &Ask<'_>, watch: Option<&mut dyn Watch>) -> Result<Answer> {
    let mut child = ask_password(&format!("bcachefs:UUID={}", ask.uuid), ask.timeout)
        .arg("--echo=yes")
        .arg(format!("{} {}", ask.prompt, ask.brief))
        .stdout(Stdio::piped())
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
    let sep = if ask.lines.is_empty() { " " } else { "\n" };
    if ask.alarm && stdout().is_terminal() {
        print!("{}{sep}", ask.prompt.red().bold());
    } else {
        print!("{}{sep}", ask.prompt);
    }
    if !ask.lines.is_empty() {
        println!("{}", ask.lines);
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

/// What came of asking for a passphrase.
pub enum Given<T> {
    /// What @accept made of the one it took.
    Taken(T),
    /// A pipe's, wrong: a script gets one go.
    Rejected,
    /// The watch has it.
    Settled,
    /// End of file on stdin: not an empty passphrase.
    Ended,
    /// The agent was declined.
    Declined,
}

/// @sb's passphrase, asked for until @accept takes one - an agent can give
/// several at once, from its cache. The cache and id are cryptsetup's, so a
/// passphrase given for one disk at boot is tried for the next. A watch is the
/// terminal's.
pub fn read_passphrase<T>(
    sb: &crate::c::bch_sb,
    mut watch: Option<&mut dyn Watch>,
    mut accept: impl FnMut(&[u8]) -> Option<T>,
) -> Result<Given<T>> {
    let Some(p) = Prompt::detect() else {
        return Err(io::Error::other(NO_ONE_TO_ASK));
    };
    let mut first = true;

    loop {
        match p {
            Prompt::Stdin => {
                let watch = watch.as_mut().map(|w| &mut **w as &mut dyn Watch);
                let line = match read_secret_line("Enter passphrase: ", watch)? {
                    Given::Taken(line) => line,
                    Given::Settled     => return Ok(Given::Settled),
                    _                  => return Ok(Given::Ended),
                };
                if let Some(t) = accept(&line) {
                    return Ok(Given::Taken(t));
                }
                if !stdin().is_terminal() {
                    return Ok(Given::Rejected);
                }
                eprintln!("incorrect passphrase");
            }
            Prompt::Agent => {
                let Some(given) = ask_agent_for_passphrase(sb, first)? else {
                    return Ok(Given::Declined);
                };
                if let Some(t) = given.split(|b| *b == 0).find_map(&mut accept) {
                    return Ok(Given::Taken(t));
                }
            }
        }
        first = false;
    }
}

/// @line, without its ending: that's theirs, not part of what they said - and
/// it is CR, LF or both, because an initramfs console that nothing has
/// configured sends CR for enter.
pub fn without_line_ending(line: &[u8]) -> &[u8] {
    let n = line.iter().rposition(|b| !matches!(b, b'\n' | b'\r')).map_or(0, |i| i + 1);
    &line[..n]
}

/// A line from stdin, not echoed where it's a terminal: Taken is the line
/// read, without_line_ending().
pub fn read_secret_line(prompt: &str, watch: Option<&mut dyn Watch>) -> Result<Given<Zeroizing<Vec<u8>>>> {
    let mut line = Zeroizing::new(Vec::new());
    let trim = |line: &mut Vec<u8>| line.truncate(without_line_ending(line).len());

    if !stdin().is_terminal() {
        if stdin().lock().read_until(b'\n', &mut line)? == 0 {
            return Ok(Given::Ended);
        }
        trim(&mut line);
        return Ok(Given::Taken(line));
    }

    let old = termios::tcgetattr(stdin())?;
    let mut new = old.clone();
    new.local_modes.remove(termios::LocalModes::ECHO);
    /*
     * We may be prompting on an early-boot console (initramfs) that no
     * shell has ever configured: without ICRNL, enter sends '\r', which
     * read_line() doesn't terminate on - keystrokes appear eaten, and
     * the eventually-assembled passphrase has embedded '\r's and is
     * rejected. Ensure line-input sanity rather than inheriting it:
     */
    new.input_modes.insert(termios::InputModes::ICRNL);
    new.local_modes.insert(termios::LocalModes::ICANON);
    termios::tcsetattr(stdin(), termios::OptionalActions::Flush, &new)?;

    eprint!("{}", prompt);

    // ICANON above is what makes this safe to poll: stdin only becomes
    // readable once a whole line has been typed, so the read below has one
    // waiting and does not block either.
    let waited = wait(stdin().as_fd(), None, watch);

    let read = match waited {
        Ok(Waited::Readable) => stdin().lock().read_until(b'\n', &mut line),
        _ => Ok(0),
    };

    termios::tcsetattr(stdin(), termios::OptionalActions::Flush, &old)?;
    eprintln!();
    let read = read?;

    Ok(match waited? {
        // End of file, not an empty passphrase: stdin closed, or input
        // that arrived before the prompt and was discarded by the flush
        // above. Calling that a wrong passphrase sends whoever hits it
        // looking for the wrong thing entirely.
        Waited::Readable if read == 0 => Given::Ended,
        Waited::Readable => {
            trim(&mut line);
            Given::Taken(line)
        }
        // No timeout is passed
        _ => Given::Settled,
    })
}

/// What an agent was given, NUL-separated, or None: it was declined.
fn ask_agent_for_passphrase(sb: &crate::c::bch_sb, first: bool) -> Result<Option<Zeroizing<Vec<u8>>>> {
    let mut command = ask_password(&format!("cryptsetup:UUID={}", sb.uuid().as_hyphenated()), None);
    command.args(["--multiple", "--keyname=cryptsetup", "--credential=cryptsetup.passphrase"]);
    if first {
        command.arg("--accept-cached");
    }
    let output = command
        .arg(format!("Please enter passphrase for disk {}:", fs_name(sb)))
        .output()?;
    if !output.status.success() {
        debug!("systemd-ask-password declined");
        return Ok(None);
    }
    Ok(Some(Zeroizing::new(output.stdout)))
}
