use std::{
    fmt::Write as _,
    fs,
    io::{stdin, IsTerminal},
    path::Path,
    thread,
    time::Duration,
};

use anyhow::{anyhow, bail, ensure, Result};
use bcachefs_kernel::c::{bch_key, bch_sb_handle, bch_encrypted_key};
use bcachefs_kernel::data::checksum::key_description;
use bcachefs_kernel::util::prompt::{read_passphrase, read_secret_line, without_line_ending, Given, Watch};
use bcachefs_kernel::util::vstructs::plain_bytes;
use linux_keyutils::{KeyRing, KeyRingIdentifier};
use log::info;

use crate::unlock_socket::UnlockSocket;
use uuid::Uuid;
use zeroize::{ZeroizeOnDrop, Zeroizing};

/// Check if a superblock has an encrypted passphrase set.
pub fn sb_is_encrypted(sb: &bcachefs_kernel::c::bch_sb_buf) -> bool {
    sb.sb()
        .crypt()
        .map(|c| c.key().is_encrypted())
        .unwrap_or(false)
}

/// Target keyring for key storage.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum Keyring {
    Session,
    #[default]
    User,
    UserSession,
}

impl Keyring {
    pub fn id(self) -> KeyRingIdentifier {
        match self {
            Keyring::Session     => KeyRingIdentifier::Session,
            Keyring::User        => KeyRingIdentifier::User,
            Keyring::UserSession => KeyRingIdentifier::UserSession,
        }
    }
}

#[derive(Clone, Debug, Default, clap::ValueEnum, strum::Display)]
pub enum UnlockPolicy {
    /// Don't ask for passphrase, if the key cannot be found in the keyring just
    /// fail
    Fail,
    /// Wait for passphrase to become available before mounting
    Wait,
    /// Interactively prompt the user for a passphrase
    #[default]
    Ask,
    /// Try to read the passphrase from `stdin` without prompting
    Stdin,
}

/// An unlocked filesystem, and whether we are holding the key that unlocked it.
///
/// The difference decides how the kernel gets it. A key we derived ourselves
/// can be handed over directly, which is what the mount helper does; one that
/// was already in a keyring we have never seen the bytes of, so all we can do
/// is leave it there for bch2_request_key() to find.
pub enum Unlocked {
    Key(PassphraseCorrect),
    InKeyring,
}

impl Unlocked {
    /// The key as the kernel's `user_key` mount parameter wants it, if we have
    /// it to give.
    ///
    /// The same bytes add_key() would have put in a keyring: both ends treat a
    /// struct bch_key as an opaque block, so there is no byte order here to get
    /// wrong.
    pub fn hex(&self) -> Option<Zeroizing<String>> {
        let Self::Key(k) = self else { return None };

        let bytes = plain_bytes(&k.passphrase_key);

        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        Some(Zeroizing::new(s))
    }

    /// Put it where bch2_request_key() will look, for the mount(2) path - that
    /// has nowhere to carry a parameter, so the keyring is the only channel.
    ///
    /// The kernel searches the invoking task's keyring tree, and a fresh
    /// session need not link the user keyring: add it to the session keyring
    /// the mount syscall will run under, and to the user keyring as before, for
    /// anything after us that expects it there.
    pub fn to_keyring(&self) -> Result<()> {
        match self {
            Self::Key(k)    => {
                let user    = KeyHandle::new(k, Keyring::User);
                let session = KeyHandle::new(k, Keyring::Session);
                user.or(session).map(|_| ())
            }
            Self::InKeyring => Ok(()),
        }
    }
}

impl UnlockPolicy {
    pub fn apply(&self, sb: &bch_sb_handle) -> Result<Unlocked> {
        let uuid = sb.sb().uuid();

        info!("Using filesystem unlock policy '{self}' on {uuid}");

        match self {
            // Somebody else's key, in a keyring: we never see the bytes.
            Self::Fail => KeyHandle::new_from_search(&uuid).map(|_| Unlocked::InKeyring),
            Self::Wait => KeyHandle::wait_for_unlock(&uuid).map(|_| Unlocked::InKeyring),
            Self::Ask => Ok(Unlocked::Key(Passphrase::ask_and_check(sb, None)?)),
            Self::Stdin => Ok(Unlocked::Key(Passphrase::read("")?
                .check(sb)
                .ok_or_else(|| anyhow!("incorrect passphrase"))?)),
        }
    }
}


/// Proof that a bcachefs key has been added to or found in the kernel keyring.
pub struct KeyHandle;

impl KeyHandle {
    pub fn new(passphrase_correct: &PassphraseCorrect, keyring: Keyring) -> Result<Self> {
        let key_name = key_description(&passphrase_correct.fs_uuid);
        let key = plain_bytes(&passphrase_correct.passphrase_key);

        // a user key: what the kernel's request_key() looks for
        KeyRing::from_special_id(keyring.id(), true)
            .and_then(|ring| ring.add_key(key_name.as_str(), key))
            .map_err(|e| anyhow!("failed to add key to keyring: {e}"))?;
        info!("Added key to keyring");
        Ok(KeyHandle)
    }

    fn search_keyring(keyring: KeyRingIdentifier, key_name: &str) -> Result<()> {
        KeyRing::from_special_id(keyring, false)?.search(key_name)?;
        info!("Found key in keyring");
        Ok(())
    }

    pub fn new_from_search(uuid: &Uuid) -> Result<Self> {
        let key_name = key_description(uuid);
        let key_name = key_name.as_str();

        Self::search_keyring(KeyRingIdentifier::Session, key_name)
            .or_else(|_| Self::search_keyring(KeyRingIdentifier::User, key_name))
            .or_else(|_| Self::search_keyring(KeyRingIdentifier::UserSession, key_name))
            .map(|_| KeyHandle)
    }

    fn wait_for_unlock(uuid: &Uuid) -> Result<Self> {
        loop {
            match Self::new_from_search(uuid) {
                Err(_) => thread::sleep(Duration::from_secs(1)),
                r => break r,
            }
        }
    }
}

#[derive(ZeroizeOnDrop)]
pub struct Passphrase(Vec<u8>);

impl Passphrase {
    pub(crate) fn get(&self) -> &[u8] {
        &self.0
    }

    /// From a line somebody typed or sent.
    pub fn from_line(line: &str) -> Self {
        Self(without_line_ending(line.as_bytes()).to_vec())
    }

    /// Ask, and take the passphrase from whoever supplies one first.
    ///
    /// @socket is a second way to answer for callers that want one - mounting
    /// does, because whoever is booting the machine may not be at it. It has
    /// already checked what it was given, so a passphrase it holds is right.
    pub fn ask_and_check(
        sb: &bch_sb_handle,
        mut socket: Option<&mut UnlockSocket<'_>>,
    ) -> Result<PassphraseCorrect> {
        match read_passphrase(
            sb.sb(),
            socket.as_deref_mut().map(|s| s as &mut dyn Watch),
            |p| Self(p.to_vec()).check(sb),
        )? {
            Given::Taken(p)   => Ok(p),
            // The socket answers by ending the prompt, so a passphrase
            // waiting there is why we are here.
            Given::Settled    => socket.and_then(UnlockSocket::take)
                .ok_or_else(|| anyhow!("incorrect passphrase")),
            Given::Rejected   => bail!("incorrect passphrase"),
            Given::Ended      => bail!("no passphrase read: end of file on stdin"),
            Given::Declined   => bail!("no passphrase given: systemd-ask-password was declined"),
        }
    }

    /// A line of stdin, @prompt shown where it's a terminal.
    fn read(prompt: &str) -> Result<Self> {
        match read_secret_line(prompt, None)? {
            Given::Taken(mut line) => Ok(Self(std::mem::take(&mut *line))),
            _ => bail!("no passphrase read: end of file on stdin"),
        }
    }

    /// Prompt for a new passphrase twice and verify they match.
    pub fn ask_for_new_passphrase() -> Result<Self> {
        let pass1 = Self::read("Enter new passphrase: ")?;
        if !stdin().is_terminal() {
            return Ok(pass1);
        }
        let pass2 = Self::read("Enter same passphrase again: ")?;
        ensure!(pass1.get() == pass2.get(), "Passphrases do not match");
        Ok(pass1)
    }

    pub fn read_from_file(passphrase_file: impl AsRef<Path>) -> Result<Self> {
        let passphrase_file = passphrase_file.as_ref();

        info!(
            "Attempting to unlock key with passphrase from file {}",
            passphrase_file.display()
        );

        let passphrase = Zeroizing::new(fs::read_to_string(passphrase_file)?);

        Ok(Self::from_line(&passphrase))
    }

    /// Re-encrypt a filesystem key with this passphrase.
    /// Returns the encrypted key suitable for writing to crypt->key.
    pub fn encrypt_key(
        &self,
        sb: &bcachefs_kernel::c::bch_sb_buf,
        key: bch_key,
    ) -> Result<bch_encrypted_key> {
        let crypt = sb.sb().crypt().expect("called on encrypted fs");
        let mut new_key = bch_encrypted_key::new_unencrypted(key);

        let passphrase_key = crypt.derive_passphrase(self.get())
            .map_err(|e| anyhow!("deriving key from passphrase: {e}"))?;
        new_key.chacha20(&passphrase_key, sb.sb().nonce());

        Ok(new_key)
    }

    pub fn check(&self, sb: &bcachefs_kernel::c::bch_sb_buf) -> Option<PassphraseCorrect> {
        let crypt = sb
            .sb()
            .crypt()
            .expect("superblock should have crypt when calling Passphrase::check");
        assert!(
            crypt.key().is_encrypted(),
            "sb_key should be encrypted when calling Passphrase::check",
        );

        // The key is encrypted (asserted above), so a passphrase was set on
        // it, so the KDF parameters exist. Failing here means a corrupt
        // superblock, not a wrong passphrase - returning None would report it
        // as the latter.
        let passphrase_key = crypt.derive_passphrase(self.get())
            .expect("sb key is encrypted, so its KDF parameters must be usable");

        let mut cleartext_sb_key = crypt.key().clone();
        cleartext_sb_key.chacha20(&passphrase_key, sb.sb().nonce());
        if cleartext_sb_key.is_encrypted() {
            return None;
        }

        Some(PassphraseCorrect {
            fs_uuid: sb.sb().uuid(),
            passphrase_key,
            cleartext_sb_key,
        })
    }
}

pub struct PassphraseCorrect {
    pub fs_uuid:          Uuid,
    pub passphrase_key:   bch_key,
    pub cleartext_sb_key: bch_encrypted_key,
}
