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

/// Trailing 'a' leaves the splash's own spinner alone.
pub fn send(text: &str) -> io::Result<()> {
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
