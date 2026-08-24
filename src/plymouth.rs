// SPDX-License-Identifier: GPL-2.0
//! Talking to the boot splash.
//!
//! Two things in the mount path care about plymouth and for opposite reasons:
//! the status display wants to draw *on* it, and the prompt wants to know it is
//! there so as not to put a question *under* it. They asked separately, and one
//! of them asked a different question than it meant to - so one module knows
//! about plymouth and both callers ask it.
//!
//! The protocol is ply-boot-protocol.h: a command byte, a flag byte, a length
//! byte counting the NUL, then the text. The length being a single byte is
//! where [`MAX`] comes from, and plymouth asserts it rather than checking, so
//! callers truncate before asking rather than after being told.

use std::io::{self, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};

/// The most a display-message can carry, the length being one byte.
pub const MAX: usize = 254;

pub fn connect() -> io::Result<UnixStream> {
    UnixStream::connect_addr(&SocketAddr::from_abstract_name("/org/freedesktop/plymouthd")?)
}

/// As much of @s as will fit, cut on a character boundary.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }

    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Trailing 'a' leaves the splash's own spinner alone.
///
/// The limit is enforced here rather than asked of callers. It used to be a
/// line in the doc comment, and the one caller kept it by dropping any line
/// that would not fit - so a single long line left nothing to send, and an
/// empty display-message is not "no room", it is the one that *clears the
/// splash*. Truncating is both the right answer and the one that cannot be
/// forgotten somewhere else later.
pub fn send(text: &str) -> io::Result<()> {
    let text = truncate(text, MAX);

    let mut msg = vec![b'M', 0x02, (text.len() + 1) as u8];
    msg.extend_from_slice(text.as_bytes());
    msg.push(0);
    msg.extend_from_slice(b"a\0");

    connect()?.write_all(&msg)
}

/// Is plymouth drawing over the console?
///
/// `plymouth --ping` is the canonical test and exits non-zero when it isn't
/// running; not installed means not covering us, so a spawn failure is the same
/// answer.
pub fn active() -> bool {
    std::process::Command::new("plymouth")
        .arg("--ping")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{truncate, MAX};

    #[test]
    fn fits_untouched() {
        assert_eq!(truncate("hello", MAX), "hello");
        assert_eq!(truncate("", MAX), "");
    }

    /// The length is one byte, so a message plymouthd would read wrong must be
    /// short before it is a message at all.
    #[test]
    fn too_long_is_cut_to_the_limit() {
        let s = "x".repeat(MAX + 10);
        assert_eq!(truncate(&s, MAX).len(), MAX);
    }

    /// Cutting mid-character would panic on the slice rather than send a short
    /// message: the limit is in bytes and the text is not.
    ///
    /// Three-byte characters deliberately - MAX is even, so a two-byte one puts
    /// the limit on a boundary already and never exercises the search at all.
    #[test]
    fn multibyte_is_cut_on_a_boundary() {
        let s = "€".repeat(MAX);
        let cut = truncate(&s, MAX);

        // 254 is not a multiple of 3, so a correct cut is necessarily shorter
        // than the limit - which is what says the search ran.
        assert_eq!(cut.len(), MAX - MAX % 3);
        assert!(s.starts_with(cut));
    }
}
