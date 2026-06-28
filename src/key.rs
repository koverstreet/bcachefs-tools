use std::{
    borrow::Cow,
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

use anyhow::{anyhow, bail, ensure, Result};
use bcachefs_kernel::c::{
    bch_key, bch_sb_handle,
    bch2_chacha20, bch_encrypted_key, bch_sb_field_crypt,
};
use bch_bindgen::keyutils::{self, keyctl_search};
use log::info;
use rustix::termios;
use uuid::Uuid;
use zeroize::{ZeroizeOnDrop, Zeroizing};

use crate::ErrnoError;

/// Check if a superblock has an encrypted passphrase set.
pub fn sb_is_encrypted(sb: &bch_sb_handle) -> bool {
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
    pub fn id(self) -> i32 {
        match self {
            Keyring::Session => keyutils::KEY_SPEC_SESSION_KEYRING,
            Keyring::User => keyutils::KEY_SPEC_USER_KEYRING,
            Keyring::UserSession => keyutils::KEY_SPEC_USER_SESSION_KEYRING,
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

impl UnlockPolicy {
    pub fn apply(&self, sb: &bch_sb_handle, keyring: Keyring) -> Result<KeyHandle> {
        let uuid = sb.sb().uuid();

        info!("Using filesystem unlock policy '{self}' on {uuid}");

        match self {
            Self::Fail => KeyHandle::new_from_search(&uuid),
            Self::Wait => Ok(KeyHandle::wait_for_unlock(&uuid)?),
            Self::Ask => {
                let passphrase = Passphrase::ask_in_terminal()?;
                let passphrase_correct = passphrase
                    .check(sb)
                    .ok_or_else(|| anyhow!("incorrect passphrase"))?;
                KeyHandle::new(&passphrase_correct, keyring)
            }
            Self::Stdin => {
                let passphrase = Passphrase::read_from_stdin()?;
                let passphrase_correct = passphrase
                    .check(sb)
                    .ok_or_else(|| anyhow!("incorrect passphrase"))?;
                KeyHandle::new(&passphrase_correct, keyring)
            }
        }
    }
}


/// Proof that a bcachefs key has been added to or found in the kernel keyring.
pub struct KeyHandle;

impl KeyHandle {
    pub fn format_key_name(uuid: &Uuid) -> CString {
        CString::new(format!("bcachefs:{uuid}")).unwrap()
    }

    pub fn new(passphrase_correct: &PassphraseCorrect, keyring: Keyring) -> Result<Self> {
        let key_name = Self::format_key_name(&passphrase_correct.fs_uuid);
        let key_name = CStr::as_ptr(&key_name);
        let key_type = c"user";

        let key_id = unsafe {
            keyutils::add_key(
                key_type.as_ptr(),
                key_name,
                ptr::addr_of!(passphrase_correct.passphrase_key).cast(),
                mem::size_of_val(&passphrase_correct.passphrase_key),
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
    pub(crate) fn get(&self) -> &CStr {
        &self.0
    }

    pub fn ask_and_check(sb: &bch_sb_handle) -> Result<PassphraseCorrect> {
        match StdinType::detect() {
            StdinType::Terminal => Self::ask_in_terminal()?
                .check(sb)
                .ok_or_else(|| anyhow!("incorrect passphrase")),
            StdinType::DevNull => Self::ask_from_systemd_and_check(sb),
            StdinType::Other => Self::read_from_stdin()?
                .check(sb)
                .ok_or_else(|| anyhow!("incorrect passphrase")),
        }
    }

    fn ask_from_systemd_and_check(sb: &bch_sb_handle) -> Result<PassphraseCorrect> {
        let uuid = sb.sb().uuid();
        let mut label = String::from_utf8_lossy(sb.sb().label());
        if label.is_empty() {
            label = Cow::Owned(uuid.hyphenated().to_string());
        }
        for i in 0..3 {
            let mut command = Command::new("systemd-ask-password");
            command
                .arg("--icon=drive-harddisk")
                .arg(format!("--id=cryptsetup:UUID={}", uuid.as_hyphenated()))
                .arg("--keyname=cryptsetup")
                .arg("--credential=cryptsetup.passphrase")
                .arg("--timeout=0")
                .arg("--multiple")
                .arg("-n");
            if i == 0 {
                command.arg("--accept-cached");
            }
            let output = command
                .arg(format!("Please enter passphrase for disk {label}:"))
                .stdin(Stdio::inherit())
                .stderr(Stdio::inherit())
                .output()?;
            if !output.status.success() {
                bail!("systemd-ask-password returned an error");
            }
            for passphrase in output.stdout.split(|b| *b == b'\0') {
                let p = Self(
                    CString::new(passphrase)
                        .expect("passphrase should not contain a NUL byte because the output was split on NUL bytes")
                );
                if let Some(passphrase_correct) = p.check(sb) {
                    return Ok(passphrase_correct);
                }
            }
        }
        bail!("incorrect passphrase limit reached");
    }

    /// Prompt for a passphrase with echo disabled.
    fn ask_in_terminal_with_prompt(prompt: &str) -> Result<Self> {
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
    pub fn ask_in_terminal() -> Result<Self> {
        Self::ask_in_terminal_with_prompt("Enter passphrase: ")
    }

    /// Prompt for a new passphrase twice and verify they match.
    pub fn ask_for_new_passphrase() -> Result<Self> {
        if !stdin().is_terminal() {
            return Self::read_from_stdin();
        }
        let pass1 = Self::ask_in_terminal_with_prompt("Enter new passphrase: ")?;
        let pass2 = Self::ask_in_terminal_with_prompt("Enter same passphrase again: ")?;
        ensure!(pass1.get().to_bytes() == pass2.get().to_bytes(), "Passphrases do not match");
        Ok(pass1)
    }

    // blocks indefinitely if no input is available on stdin
    pub fn read_from_stdin() -> Result<Self> {
        info!("Trying to read passphrase from stdin...");

        let mut line = Zeroizing::new(String::new());
        stdin().read_line(&mut line)?;

        Ok(Self(CString::new(line.trim_end_matches('\n'))?))
    }

    pub fn read_from_file(passphrase_file: impl AsRef<Path>) -> Result<Self> {
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

        unsafe { bch_bindgen::c::derive_passphrase(crypt_ptr, self.get().as_ptr()) }
    }

    /// Re-encrypt a filesystem key with this passphrase.
    /// Returns the encrypted key suitable for writing to crypt->key.
    pub fn encrypt_key(
        &self,
        sb: &bch_sb_handle,
        key: bch_key,
    ) -> bch_encrypted_key {
        let crypt = sb.sb().crypt().expect("called on encrypted fs");
        let mut new_key = bch_encrypted_key::new_unencrypted(key);
        
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

    pub fn check(&self, sb: &bch_sb_handle) -> Option<PassphraseCorrect> {
        let crypt = sb
            .sb()
            .crypt()
            .expect("superblock should have crypt when calling Passphrase::check");
        assert!(
            crypt.key().is_encrypted(),
            "sb_key should be encrypted when calling Passphrase::check",
        );

        let mut passphrase_key: bch_key = self.derive(crypt);

        let mut cleartext_sb_key = crypt.key().clone();
        unsafe {
            bch2_chacha20(
                ptr::addr_of_mut!(passphrase_key),
                sb.sb().nonce(),
                ptr::addr_of_mut!(cleartext_sb_key).cast(),
                mem::size_of_val(&cleartext_sb_key),
            )
        };
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

impl StdinType {
    fn detect() -> StdinType {
        let stdin = stdin();
        if stdin.is_terminal() {
            StdinType::Terminal
        } else if is_dev_null(stdin.as_fd()).unwrap_or(false) {
            StdinType::DevNull
        } else {
            StdinType::Other
        }
    }
}
