use std::{
    ffi::{CStr, CString},
    fs,
    io::{self, stdin, IsTerminal},
    mem,
    os::fd::{AsFd, BorrowedFd},
    path::Path,
    process::{Command, Stdio},
    ptr, thread,
    time::Duration,
};

use anyhow::{anyhow, ensure, Result};
use bch_bindgen::{
    bcachefs::{self, bch_key, bch_sb_handle},
    c::{bch2_chacha20, bch_encrypted_key, bch_sb_field_crypt},
    keyutils::{self, keyctl_search},
};
use log::{debug, info};
use rustix::termios;
use uuid::Uuid;
use zeroize::{ZeroizeOnDrop, Zeroizing};

use crate::ErrnoError;

const BCH_KEY_MAGIC: &[u8; 8] = b"bch**key";

/// Check if a superblock has an encrypted passphrase set.
pub fn sb_is_encrypted(sb: &bch_sb_handle) -> bool {
    let bch_key_magic = u64::from_le_bytes(*BCH_KEY_MAGIC);
    sb.sb()
        .crypt()
        .map(|c| c.key().magic != bch_key_magic)
        .unwrap_or(false)
}

/// Create an unencrypted (plaintext) key for the crypt field (remove-passphrase).
pub fn unencrypted_key(key: &bch_key) -> bch_encrypted_key {
    bch_encrypted_key {
        magic: u64::from_le_bytes(*BCH_KEY_MAGIC),
        key: *key,
    }
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
    pub fn id(self) -> i32 {
        match self {
            Keyring::Session => keyutils::KEY_SPEC_SESSION_KEYRING,
            Keyring::User => keyutils::KEY_SPEC_USER_KEYRING,
            Keyring::UserSession => keyutils::KEY_SPEC_USER_SESSION_KEYRING,
        }
    }
}

#[derive(Clone, Debug, clap::ValueEnum, strum::Display)]
pub enum UnlockPolicy {
    /// Don't ask for passphrase, if the key cannot be found in the keyring just
    /// fail
    Fail,
    /// Wait for passphrase to become available before mounting
    Wait,
    /// Interactively prompt the user for a passphrase
    Ask,
    /// Try to read the passphrase from `stdin` without prompting
    Stdin,
}

impl UnlockPolicy {
    pub fn apply(&self, sb: &bch_sb_handle) -> Result<KeyHandle> {
        let uuid = sb.sb().uuid();

        info!("Using filesystem unlock policy '{self}' on {uuid}");

        match self {
            Self::Fail => KeyHandle::new_from_search(&uuid),
            Self::Wait => Ok(KeyHandle::wait_for_unlock(&uuid)?),
            Self::Ask => Passphrase::new_from_prompt(&uuid).and_then(|p| KeyHandle::new(sb, &p, Keyring::User)),
            Self::Stdin => Passphrase::new_from_stdin().and_then(|p| KeyHandle::new(sb, &p, Keyring::User)),
        }
    }
}

impl Default for UnlockPolicy {
    fn default() -> Self {
        Self::Ask
    }
}

/// Proof that a bcachefs key has been added to or found in the kernel keyring.
pub struct KeyHandle;

impl KeyHandle {
    pub fn format_key_name(uuid: &Uuid) -> CString {
        CString::new(format!("bcachefs:{uuid}")).unwrap()
    }

    pub fn new(sb: &bch_sb_handle, passphrase: &Passphrase, keyring: Keyring) -> Result<Self> {
        let key_name = Self::format_key_name(&sb.sb().uuid());
        let key_name = CStr::as_ptr(&key_name);
        let key_type = c"user";

        let (passphrase_key, _sb_key) = passphrase.check(sb)?;

        let key_id = unsafe {
            keyutils::add_key(
                key_type.as_ptr(),
                key_name,
                ptr::addr_of!(passphrase_key).cast(),
                mem::size_of_val(&passphrase_key),
                keyring.id(),
            )
        };

        if key_id > 0 {
            info!("Added key to keyring");
            Ok(KeyHandle)
        } else {
            Err(anyhow!("failed to add key to keyring: {}", errno::errno()))
        }
    }

    fn search_keyring(keyring: i32, key_name: &CStr) -> Result<()> {
        let key_name = CStr::as_ptr(key_name);
        let key_type = c"user";

        let key_id = unsafe { keyctl_search(keyring, key_type.as_ptr(), key_name, 0) };

        if key_id > 0 {
            info!("Found key in keyring");
            Ok(())
        } else {
            Err(ErrnoError(errno::errno()).into())
        }
    }

    pub fn new_from_search(uuid: &Uuid) -> Result<Self> {
        let key_name = Self::format_key_name(uuid);

        Self::search_keyring(keyutils::KEY_SPEC_SESSION_KEYRING, &key_name)
            .or_else(|_| Self::search_keyring(keyutils::KEY_SPEC_USER_KEYRING, &key_name))
            .or_else(|_| Self::search_keyring(keyutils::KEY_SPEC_USER_SESSION_KEYRING, &key_name))
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
pub struct Passphrase(CString);

impl Passphrase {
    fn get(&self) -> &CStr {
        &self.0
    }

    pub fn new(uuid: &Uuid) -> Result<Self> {
        match get_stdin_type() {
            StdinType::Terminal => Self::new_from_prompt(uuid),
            StdinType::DevNull => Self::new_from_askpassword(uuid)?,
            StdinType::Other => Self::new_from_stdin(),
        }
    }

    // The outer result represents a failure when trying to run systemd-ask-password,
    // it is non-critical and will cause the password to be asked internally.
    // The inner result represent a successful request that returned an error
    // this one results in an error.
    fn new_from_askpassword(uuid: &Uuid) -> Result<Result<Self>> {
        let output = Command::new("systemd-ask-password")
            .arg("--icon=drive-harddisk")
            .arg(format!("--id=bcachefs:{}", uuid.as_hyphenated()))
            .arg(format!("--keyname={}", uuid.as_hyphenated()))
            .arg("--accept-cached")
            .arg("-n")
            .arg("Enter passphrase: ")
            .stdin(Stdio::inherit())
            .stderr(Stdio::inherit())
            .output()?;
        Ok(if output.status.success() {
            match CString::new(output.stdout) {
                Ok(cstr) => Ok(Self(cstr)),
                Err(e) => Err(e.into()),
            }
        } else {
            Err(anyhow!("systemd-ask-password returned an error"))
        })
    }

    /// Prompt for a passphrase with echo disabled.
    fn prompt_hidden(prompt: &str) -> Result<Self> {
        let old = termios::tcgetattr(stdin())?;
        let mut new = old.clone();
        new.local_modes.remove(termios::LocalModes::ECHO);
        termios::tcsetattr(stdin(), termios::OptionalActions::Flush, &new)?;

        eprint!("{}", prompt);

        let mut line = Zeroizing::new(String::new());
        let res = stdin().read_line(&mut line);
        termios::tcsetattr(stdin(), termios::OptionalActions::Flush, &old)?;
        eprintln!();
        res?;

        Ok(Self(CString::new(line.trim_end_matches('\n'))?))
    }

    // blocks indefinitely if no input is available on stdin
    pub fn new_from_prompt(uuid: &Uuid) -> Result<Self> {
        match Self::new_from_askpassword(uuid) {
            Ok(phrase) => return phrase,
            Err(_) => debug!("Failed to start systemd-ask-password, doing the prompt ourselves"),
        }
        Self::prompt_hidden("Enter passphrase: ")
    }

    /// Prompt for a new passphrase twice and verify they match.
    pub fn new_from_prompt_twice() -> Result<Self> {
        if !stdin().is_terminal() {
            return Self::new_from_stdin();
        }
        let pass1 = Self::prompt_hidden("Enter new passphrase: ")?;
        let pass2 = Self::prompt_hidden("Enter same passphrase again: ")?;
        ensure!(pass1.get().to_bytes() == pass2.get().to_bytes(), "Passphrases do not match");
        Ok(pass1)
    }

    // blocks indefinitely if no input is available on stdin
    pub fn new_from_stdin() -> Result<Self> {
        info!("Trying to read passphrase from stdin...");

        let mut line = Zeroizing::new(String::new());
        stdin().read_line(&mut line)?;

        Ok(Self(CString::new(line.trim_end_matches('\n'))?))
    }

    pub fn new_from_file(passphrase_file: impl AsRef<Path>) -> Result<Self> {
        let passphrase_file = passphrase_file.as_ref();

        info!(
            "Attempting to unlock key with passphrase from file {}",
            passphrase_file.display()
        );

        let passphrase = Zeroizing::new(fs::read_to_string(passphrase_file)?);

        Ok(Self(CString::new(passphrase.trim_end_matches('\n'))?))
    }

    fn derive(&self, crypt: &bch_sb_field_crypt) -> bch_key {
        let crypt_ptr = (crypt as *const bch_sb_field_crypt).cast_mut();

        unsafe { bcachefs::derive_passphrase(crypt_ptr, self.get().as_ptr()) }
    }

    /// Re-encrypt a filesystem key with this passphrase.
    /// Returns the encrypted key suitable for writing to crypt->key.
    pub fn encrypt_key(
        &self,
        sb: &bch_sb_handle,
        key: &bch_key,
    ) -> bch_encrypted_key {
        let crypt = sb.sb().crypt().expect("called on encrypted fs");
        let mut new_key = bch_encrypted_key {
            magic: u64::from_le_bytes(*BCH_KEY_MAGIC),
            key: *key,
        };

        let mut passphrase_key: bch_key = self.derive(crypt);

        unsafe {
            bch2_chacha20(
                ptr::addr_of_mut!(passphrase_key),
                sb.sb().nonce(),
                ptr::addr_of_mut!(new_key).cast(),
                mem::size_of_val(&new_key),
            )
        };

        new_key
    }

    pub fn check(&self, sb: &bch_sb_handle) -> Result<(bch_key, bch_encrypted_key)> {
        let bch_key_magic = u64::from_le_bytes(*BCH_KEY_MAGIC);

        let crypt = sb
            .sb()
            .crypt()
            .ok_or_else(|| anyhow!("filesystem is not encrypted"))?;
        let mut sb_key = *crypt.key();

        ensure!(
            sb_key.magic != bch_key_magic,
            "filesystem does not have encryption key"
        );

        let mut passphrase_key: bch_key = self.derive(crypt);

        unsafe {
            bch2_chacha20(
                ptr::addr_of_mut!(passphrase_key),
                sb.sb().nonce(),
                ptr::addr_of_mut!(sb_key).cast(),
                mem::size_of_val(&sb_key),
            )
        };
        ensure!(sb_key.magic == bch_key_magic, "incorrect passphrase");

        Ok((passphrase_key, sb_key))
    }
}

fn is_dev_null(fd: BorrowedFd) -> io::Result<bool> {
    let stat = rustix::fs::fstat(fd)?;
    let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
    if file_type != rustix::fs::FileType::CharacterDevice {
        return Ok(false);
    }
    let major = rustix::fs::major(stat.st_rdev);
    let minor = rustix::fs::minor(stat.st_rdev);
    Ok(major == 1 && minor == 3)
}

enum StdinType {
    Terminal,
    DevNull,
    Other,
}

fn get_stdin_type() -> StdinType {
    let stdin = stdin();
    if stdin.is_terminal() {
        StdinType::Terminal
    } else if is_dev_null(stdin.as_fd()).unwrap_or(false) {
        StdinType::DevNull
    } else {
        StdinType::Other
    }
}
