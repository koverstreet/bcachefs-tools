//! Putting a question to whoever is at the machine, during a mount.

use core::time::Duration;

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
    /// A watch's: see [`Watch`].
    Settled,
    /// Input ended, or a timeout with no silence to take.
    Unanswered,
}

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
