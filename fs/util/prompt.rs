//! Putting a question to whoever is at the machine.
//!
//! A question is a [`Question`], answered by one of its [`Choice`]s; where it
//! goes is a [`Prompt`]. In userspace that's the terminal, or systemd's
//! password agents. In the kernel it's the stdio redirect of whoever's running
//! us, the question rendered and framed as a record ([`Question::record()`]):
//! the redirect's other end, src/thread_with_file.rs's relay, shows it as the
//! tools would and writes back what was said, for the kernel to parse.

use core::time::Duration;

use crate::prt;
use crate::util::Printbuf;

#[cfg(kernel)]
mod in_kernel;
#[cfg(kernel)]
pub use in_kernel::*;
#[cfg(not(kernel))]
mod userspace;
#[cfg(not(kernel))]
pub use userspace::*;

pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// The only place an answer is written down: both rendered forms and the parse
/// derive from this, so a question cannot offer a letter it will not accept.
pub struct Choice<A> {
    /// Matched exactly, and failing that case-insensitively, as are the
    /// aliases: `y` and `Y` can be two answers, and are one where only one is.
    pub key: char,
    pub aliases: &'static [&'static str],
    /// For the bracketed summary; "" shows the bare key.
    pub short: &'static str,
    /// For a terminal, which has room for a sentence; "" for none.
    pub blurb: &'static str,
    pub answer: A,
}

/// Answers live per question rather than in one shared parser, so that two
/// questions cannot cross-parse each other's vocabulary.
pub struct Question<'a, A> {
    /// One line: the agent protocol's `Message=` is one line, and at boot
    /// behind a splash that line is the whole of what anyone sees. So it says
    /// everything the answer turns on - there is no second place to put it.
    pub prompt: &'a str,
    pub choices: &'a [Choice<A>],
    /// Also what a bare Enter and a timed-out boot prompt mean. Shown
    /// capitalised in the summary. None: Enter asks again, and a timeout goes
    /// unanswered.
    pub silence: Option<A>,
    /// This question is about losing data, and the prompt says so. Drawn in
    /// red where there is a terminal to draw it on; everywhere else the words
    /// have to carry it alone, which is why they are in `prompt` and not in a
    /// decoration of their own.
    pub alarm: bool,
    pub uuid: &'a str,
    /// `None` waits indefinitely - what `--timeout=0` means to
    /// systemd-ask-password.
    pub timeout: Option<Duration>,
}

/// What came of a question. A reply that's none of its answers is never one:
/// the question is asked again.
pub enum Reply<A> {
    Answer(A),
    /// A watch's: see Watch.
    Settled,
    /// Input ended, or a timeout with no silence to take.
    Unanswered,
}

/// A question's framing, in a stdio redirect: SOH, its fields separated by US,
/// ETX.
pub const RECORD_START: u8 = 0x01;
pub const RECORD_END: u8 = 0x03;
const US: char = '\x1f';
/// The reply line that's no answer: EOT.
pub const UNANSWERED: &str = "\x04";

/// @s, if it's one character.
fn single(s: &str) -> Option<char> {
    let mut c = s.chars();
    let k = c.next()?;
    c.next().is_none().then_some(k)
}

impl<A: Copy + PartialEq> Question<'_, A> {
    /// @reply as one of the answers: None if it's none of them.
    pub fn parse(&self, reply: &str) -> Option<A> {
        let reply = reply.trim();
        if reply.is_empty() {
            return self.silence;
        }

        let key = single(reply);
        self.choices.iter()
            .find(|c| key == Some(c.key))
            .or_else(|| self.choices.iter().find(|c| {
                key.is_some_and(|k| k.eq_ignore_ascii_case(&c.key))
                    || c.aliases.iter().any(|a| reply.eq_ignore_ascii_case(a))
            }))
            .map(|c| c.answer)
    }

    /// The choices with a blurb, a line each.
    pub fn lines(&self, out: &mut Printbuf) {
        for (i, c) in self.choices.iter().filter(|c| !c.blurb.is_empty()).enumerate() {
            if i > 0 {
                prt!(out, "\n");
            }
            prt!(out, "  {}  {}", c.key, c.blurb);
        }
    }

    /// The answer silence resolves to is the one shown capitalised.
    pub fn brief(&self, out: &mut Printbuf) {
        prt!(out, "[");
        for (i, c) in self.choices.iter().enumerate() {
            if i > 0 {
                prt!(out, " / ");
            }
            if Some(c.answer) == self.silence {
                prt!(out, "{}", c.key.to_ascii_uppercase());
            } else if c.short.is_empty() {
                prt!(out, "{}", c.key);
            } else {
                prt!(out, "{}={}", c.key, c.short);
            }
        }
        prt!(out, "]");
    }

    /// Framed, for a stdio redirect's relay to show: each field a name, '='
    /// and a value. The answer comes back as a line: what was said, empty for
    /// silence, or UNANSWERED.
    pub fn record(&self, out: &mut Printbuf) {
        out.write_bytes(&[RECORD_START]);
        prt!(out, "prompt={}{US}lines=", self.prompt);
        self.lines(out);
        prt!(out, "{US}brief=");
        self.brief(out);
        if self.silence.is_some() {
            prt!(out, "{US}silence=1");
        }
        if self.alarm {
            prt!(out, "{US}alarm=1");
        }
        prt!(out, "{US}uuid={}", self.uuid);
        if let Some(t) = self.timeout {
            prt!(out, "{US}timeout={}", t.as_secs());
        }
        out.write_bytes(&[RECORD_END]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHOICES: &[Choice<u8>] = &[
        Choice { key: 'y', aliases: &["yes"], short: "", blurb: "", answer: 0 },
        Choice { key: 'n', aliases: &["no"], short: "", blurb: "", answer: 1 },
        Choice { key: 'Y', aliases: &[], short: "all", blurb: "", answer: 2 },
    ];

    fn question(silence: Option<u8>) -> Question<'static, u8> {
        Question { prompt: "fix?", choices: CHOICES, silence, alarm: false, uuid: "u", timeout: None }
    }

    /// y and Y are two answers here; N is n, there being no N.
    #[test]
    fn exact_case_first() {
        let q = question(None);
        assert!(q.parse("y") == Some(0));
        assert!(q.parse("Y") == Some(2));
        assert!(q.parse("N") == Some(1));
        assert!(q.parse("YES") == Some(0));
        assert!(q.parse("maybe").is_none());
        assert!(q.parse("").is_none());
        assert!(question(Some(1)).parse("\n") == Some(1));
    }
}
