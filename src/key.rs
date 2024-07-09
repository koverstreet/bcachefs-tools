use std::{
    ffi::{c_long, CStr, CString},
    fs,
    io::{stdin, IsTerminal},
    mem,
    path::Path,
    ptr, thread,
    time::Duration,
};

use anyhow::{anyhow, ensure, Result};
use bch_bindgen::{
    bcachefs::{self, bch_key, bch_sb_handle},
    c::{bch2_chacha_encrypt_key, bch_encrypted_key, bch_sb_field_crypt},
    keyutils::{self, keyctl_search},
};
use byteorder::{LittleEndian, ReadBytesExt};
use log::info;
use rustix::termios;
use uuid::Uuid;
use zeroize::{ZeroizeOnDrop, Zeroizing};

use crate::{c_str, ErrnoError};

const BCH_KEY_MAGIC: &str = "bch**key";

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
            Self::Ask => Passphrase::new_from_prompt().and_then(|p| KeyHandle::new(sb, &p)),
            Self::Stdin => Passphrase::new_from_stdin().and_then(|p| KeyHandle::new(sb, &p)),
        }
    }
}

impl Default for UnlockPolicy {
    fn default() -> Self {
        Self::Ask
    }
}

/// A handle to an existing bcachefs key in the kernel keyring
pub struct KeyHandle {
    // FIXME: Either these come in useful for something or we remove them
    _uuid: Uuid,
    _id:   c_long,
}

impl KeyHandle {
    pub fn format_key_name(uuid: &Uuid) -> CString {
        CString::new(format!("bcachefs:{uuid}")).unwrap()
    }

    pub fn new(sb: &bch_sb_handle, passphrase: &Passphrase) -> Result<Self> {
        let key_name = Self::format_key_name(&sb.sb().uuid());
        let key_name = CStr::as_ptr(&key_name);
        let key_type = c_str!("user");

        let (passphrase_key, _sb_key) = passphrase.check(sb)?;

        let key_id = unsafe {
            keyutils::add_key(
                key_type,
                key_name,
                ptr::addr_of!(passphrase_key).cast(),
                mem::size_of_val(&passphrase_key),
                keyutils::KEY_SPEC_USER_KEYRING,
            )
        };

        if key_id > 0 {
            info!("Added key to keyring");
            Ok(KeyHandle {
                _uuid: sb.sb().uuid(),
                _id:   c_long::from(key_id),
            })
        } else {
            Err(anyhow!("failed to add key to keyring: {}", errno::errno()))
        }
    }

    fn search_keyring(keyring: i32, key_name: &CStr) -> Result<c_long> {
        let key_name = CStr::as_ptr(key_name);
        let key_type = c_str!("user");

        let key_id = unsafe { keyctl_search(keyring, key_type, key_name, 0) };

        if key_id > 0 {
            info!("Found key in keyring");
            Ok(key_id)
        } else {
            Err(ErrnoError(errno::errno()).into())
        }
    }

    pub fn new_from_search(uuid: &Uuid) -> Result<Self> {
        let key_name = Self::format_key_name(uuid);

        Self::search_keyring(keyutils::KEY_SPEC_SESSION_KEYRING, &key_name)
            .or_else(|_| Self::search_keyring(keyutils::KEY_SPEC_USER_KEYRING, &key_name))
            .or_else(|_| Self::search_keyring(keyutils::KEY_SPEC_USER_SESSION_KEYRING, &key_name))
            .map(|id| Self {
                _uuid: *uuid,
                _id:   id,
            })
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

    pub fn new() -> Result<Self> {
        if stdin().is_terminal() {
            Self::new_from_prompt()
        } else {
            Self::new_from_stdin()
        }
    }

    // blocks indefinitely if no input is available on stdin
    pub fn new_from_prompt() -> Result<Self> {
        let old = termios::tcgetattr(stdin())?;
        let mut new = old.clone();
        new.local_modes.remove(termios::LocalModes::ECHO);
        termios::tcsetattr(stdin(), termios::OptionalActions::Flush, &new)?;

        eprint!("Enter passphrase: ");

        let mut line = Zeroizing::new(String::new());
        let res = stdin().read_line(&mut line);
        termios::tcsetattr(stdin(), termios::OptionalActions::Flush, &old)?;
        eprintln!("");
        res?;

        Ok(Self(CString::new(line.trim_end_matches('\n'))?))
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

    pub fn check(&self, sb: &bch_sb_handle) -> Result<(bch_key, bch_encrypted_key)> {
        let bch_key_magic = BCH_KEY_MAGIC.as_bytes().read_u64::<LittleEndian>().unwrap();

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

        let ret = unsafe {
            bch2_chacha_encrypt_key(
                ptr::addr_of_mut!(passphrase_key),
                sb.sb().nonce(),
                ptr::addr_of_mut!(sb_key).cast(),
                mem::size_of_val(&sb_key),
            )
        };
        ensure!(ret == 0, "error encrypting key");
        ensure!(sb_key.magic == bch_key_magic, "incorrect passphrase");

        Ok((passphrase_key, sb_key))
    }
}
