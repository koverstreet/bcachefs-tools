// SPDX-License-Identifier: GPL-2.0
//! A second way to answer the passphrase prompt.
//!
//! While mount.bcachefs is waiting at the terminal for a passphrase, it also
//! listens on a unix socket, and takes whichever arrives first. That is what
//! makes remote unlock work without a second mechanism: an initramfs blocks on
//! the prompt as it always did, and someone who has sshed in over dropbear
//! writes the passphrase to the socket instead of walking to the machine.
//!
//! Why a socket rather than a fifo, which is what people build by hand: a
//! connection gives us somewhere to put the answer. A fifo takes the passphrase
//! and tells you nothing, so a typo over ssh looks exactly like a working
//! unlock until the boot fails. Here a wrong passphrase gets told so, and - the
//! point of checking here rather than upstack - it does not disturb the
//! terminal prompt at all. Somebody fat-fingering it remotely cannot blow away
//! what you have half-typed at the console, and can try again.
//!
//! There is no client. The protocol is a passphrase and a newline in, one
//! status line out, deliberately: in an initramfs with nothing installed,
//! `socat - UNIX-CONNECT:...` has to be enough.
//!
//! It is a passphrase oracle, so the directory is 0700 and root-only. It is
//! also strictly an addition - if we cannot create it we say so and go on
//! prompting, because failing a mount over a convenience would be absurd.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use anyhow::{Context, Result};
use bcachefs_kernel::c::bch_sb_handle;
use log::{debug, info};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::key::{Passphrase, PassphraseCorrect};
use crate::prompt::{Stirred, Watch};

const DIR: &str = "/run/bcachefs/unlock";

pub struct UnlockSocket<'a> {
    listener: UnixListener,
    path:     PathBuf,
    sb:       &'a bch_sb_handle,
    /// The one that checked out, waiting for [`UnlockSocket::take`].
    got:      Option<PassphraseCorrect>,
}

impl UnlockSocket<'_> {
    /// Where to tell someone to write. One per filesystem: several can be
    /// prompting at once at boot, and they want different passphrases.
    pub fn path(uuid: &Uuid) -> PathBuf {
        PathBuf::from(DIR).join(uuid.hyphenated().to_string())
    }

    /// `None` if we could not get a socket up - the caller prompts without one,
    /// exactly as it did before there was a socket to want.
    ///
    /// Not a warning. Nothing is wrong: an encrypted filesystem mounted
    /// somewhere /run is absent or read-only would otherwise say so on every
    /// mount, about a facility whoever is mounting never asked for.
    pub fn open(sb: &bch_sb_handle) -> Option<UnlockSocket<'_>> {
        match Self::try_open(sb) {
            Ok(s)  => Some(s),
            Err(e) => {
                info!("no remote unlock socket ({e:#}); the prompt is the only way in");
                None
            }
        }
    }

    fn try_open(sb: &bch_sb_handle) -> Result<UnlockSocket<'_>> {
        let path = Self::path(&sb.sb().uuid());

        std::fs::create_dir_all(DIR).with_context(|| format!("creating {DIR}"))?;
        // The directory is the gate, not the socket: bind(2) takes the mode
        // from the umask and there is no atomic way to hand it one, so a
        // socket briefly readable by the world would be a real window. A 0700
        // directory closes it without needing the socket to be anything.
        std::fs::set_permissions(DIR, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {DIR} to root"))?;

        Self::clear_stale(&path)?;

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding {}", path.display()))?;
        listener.set_nonblocking(true)?;

        info!("remote unlock: write the passphrase to {}", path.display());

        Ok(UnlockSocket { listener, path, sb, got: None })
    }

    /// A socket left behind by a mount that died holds the address, and we
    /// would rather listen than refuse. Connecting is what distinguishes a
    /// corpse from a live mount also prompting for this filesystem - only the
    /// corpse refuses.
    fn clear_stale(path: &PathBuf) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }

        if UnixStream::connect(path).is_ok() {
            anyhow::bail!("{} is already being listened on", path.display());
        }

        debug!("removing a socket left behind by an earlier mount");
        std::fs::remove_file(path)
            .with_context(|| format!("removing stale {}", path.display()))
    }

    /// The passphrase that checked out, once [`Watch::stirred`] has said one
    /// did.
    pub fn take(&mut self) -> Option<PassphraseCorrect> {
        self.got.take()
    }

    /// One connection: a passphrase, a verdict, and the connection is done. We
    /// never hold it open - the caller is a shell script with socat, and having
    /// to know when to hang up would be another thing to get wrong.
    fn serve(&mut self, mut conn: UnixStream) -> Result<bool> {
        let mut line = Zeroizing::new(String::new());
        BufReader::new(&conn).read_line(&mut line)?;

        let passphrase = Passphrase::from_line(&line)?;

        match passphrase.check(self.sb) {
            Some(correct) => {
                let _ = conn.write_all(b"ok\n");
                self.got = Some(correct);
                Ok(true)
            }
            None => {
                // Say so, and stay up: whoever typed it is right there and can
                // try again, which is the whole reason this is a socket.
                let _ = conn.write_all(b"incorrect passphrase\n");
                info!("remote unlock: incorrect passphrase, still listening");
                Ok(false)
            }
        }
    }
}

impl Watch for UnlockSocket<'_> {
    fn raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    fn stirred(&mut self) -> Stirred {
        // Non-blocking, so a spurious wakeup costs us an EAGAIN and nothing
        // else. A connection that goes wrong is that caller's problem, not a
        // reason to stop listening or to disturb the prompt.
        let conn = match self.listener.accept() {
            Ok((conn, _)) => conn,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    debug!("remote unlock: accept failed: {e}");
                }
                return Stirred::Nothing;
            }
        };

        match self.serve(conn) {
            Ok(true)  => Stirred::Answered,
            Ok(false) => Stirred::Nothing,
            Err(e) => {
                debug!("remote unlock: {e:#}");
                Stirred::Nothing
            }
        }
    }
}

impl Drop for UnlockSocket<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
