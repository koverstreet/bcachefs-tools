use std::{fmt, fs, io::{stdin, IsTerminal}};

use log::info;
use bch_bindgen::bcachefs::bch_sb_handle;
use clap::builder::PossibleValue;
use crate::c_str;
use anyhow::anyhow;

#[derive(Clone, Debug)]
pub enum UnlockPolicy {
    None,
    Fail,
    Wait,
    Ask,
}

impl std::str::FromStr for UnlockPolicy {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            ""|"none" => Ok(UnlockPolicy::None),
            "fail"    => Ok(UnlockPolicy::Fail),
            "wait"    => Ok(UnlockPolicy::Wait),
            "ask"     => Ok(UnlockPolicy::Ask),
            _         => Err(anyhow!("Invalid unlock policy provided")),
        }
    }
}

impl clap::ValueEnum for UnlockPolicy {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            UnlockPolicy::None,
            UnlockPolicy::Fail,
            UnlockPolicy::Wait,
            UnlockPolicy::Ask,
        ]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(match self {
            Self::None => PossibleValue::new("none").alias(""),
            Self::Fail => PossibleValue::new("fail"),
            Self::Wait => PossibleValue::new("wait"),
            Self::Ask => PossibleValue::new("ask"),
        })
    }
}

impl fmt::Display for UnlockPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnlockPolicy::None => write!(f, "None"),
            UnlockPolicy::Fail => write!(f, "Fail"),
            UnlockPolicy::Wait => write!(f, "Wait"),
            UnlockPolicy::Ask => write!(f, "Ask"),
        }
    }
}

fn check_for_key(key_name: &std::ffi::CStr) -> anyhow::Result<bool> {
    use bch_bindgen::keyutils::{self, keyctl_search};
    let key_name = key_name.to_bytes_with_nul().as_ptr() as *const _;
    let key_type = c_str!("user");

    let key_id = unsafe { keyctl_search(keyutils::KEY_SPEC_USER_KEYRING, key_type, key_name, 0) };
    if key_id > 0 {
        info!("Key has become available");
        Ok(true)
    } else {
        match errno::errno().0 {
            libc::ENOKEY | libc::EKEYREVOKED => Ok(false),
            _ => Err(crate::ErrnoError(errno::errno()).into()),
        }
    }
}

fn wait_for_unlock(uuid: &uuid::Uuid) -> anyhow::Result<()> {
    let key_name = std::ffi::CString::new(format!("bcachefs:{}", uuid)).unwrap();
    loop {
        if check_for_key(&key_name)? {
            break Ok(());
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn ask_for_passphrase(sb: &bch_sb_handle) -> anyhow::Result<()> {
    let passphrase = if stdin().is_terminal() {
        rpassword::prompt_password("Enter passphrase: ")?
    } else {
        let mut line = String::new();
        stdin().read_line(&mut line)?;
        line
    };
    unlock_master_key(sb, &passphrase)
}

const BCH_KEY_MAGIC: &str = "bch**key";
fn unlock_master_key(sb: &bch_sb_handle, passphrase: &String) -> anyhow::Result<()> {
    use bch_bindgen::bcachefs::{self, bch2_chacha_encrypt_key, bch_encrypted_key, bch_key};
    use byteorder::{LittleEndian, ReadBytesExt};
    use std::os::raw::c_char;

    let key_name = std::ffi::CString::new(format!("bcachefs:{}", sb.sb().uuid())).unwrap();
    if check_for_key(&key_name)? {
        return Ok(());
    }

    let bch_key_magic = BCH_KEY_MAGIC.as_bytes().read_u64::<LittleEndian>().unwrap();
    let crypt = sb.sb().crypt().unwrap();
    let passphrase = std::ffi::CString::new(passphrase.trim_end())?; // bind to keep the CString alive
    let mut output: bch_key = unsafe {
        bcachefs::derive_passphrase(
            crypt as *const _ as *mut _,
            passphrase.as_c_str().to_bytes_with_nul().as_ptr() as *const _,
        )
    };

    let mut key = crypt.key().clone();
    let ret = unsafe {
        bch2_chacha_encrypt_key(
            &mut output as *mut _,
            sb.sb().nonce(),
            &mut key as *mut _ as *mut _,
            std::mem::size_of::<bch_encrypted_key>() as usize,
        )
    };
    if ret != 0 {
        Err(anyhow!("chacha decryption failure"))
    } else if key.magic != bch_key_magic {
        Err(anyhow!("failed to verify the password"))
    } else {
        let key_type = c_str!("user");
        let ret = unsafe {
            bch_bindgen::keyutils::add_key(
                key_type,
                key_name.as_c_str().to_bytes_with_nul() as *const _ as *const c_char,
                &output as *const _ as *const _,
                std::mem::size_of::<bch_key>() as usize,
                bch_bindgen::keyutils::KEY_SPEC_USER_KEYRING,
            )
        };
        if ret == -1 {
            Err(anyhow!("failed to add key to keyring: {}", errno::errno()))
        } else {
            Ok(())
        }
    }
}

pub fn unlock_master_key_using_passphrase_file(block_device: &bch_sb_handle, passphrase_file: &std::path::Path) -> anyhow::Result<()> {
    // Attempts to unlock the master key by passphrase_file
    // Return OK() if unlock was successful, Err() otherwise
    info!("Attempting to unlock master key for filesystem {}, using passphrase from file {}", block_device.sb().uuid(), passphrase_file.display());
    // Read the contents of the passphrase_file into a string
    let passphrase = fs::read_to_string(passphrase_file)?;
    // Call decrypt_master_key with the read string
    unlock_master_key(block_device, &passphrase)
}

pub fn apply_key_unlocking_policy(block_device: &bch_sb_handle, unlock_policy: UnlockPolicy) -> anyhow::Result<()> {
    info!("Attempting to unlock master key for filesystem {}, using unlock policy {}", block_device.sb().uuid(), unlock_policy);
    match unlock_policy {
        UnlockPolicy::Fail => Err(anyhow!("no passphrase available")),
        UnlockPolicy::Wait => Ok(wait_for_unlock(&block_device.sb().uuid())?),
        UnlockPolicy::Ask => ask_for_passphrase(block_device),
        _ => Err(anyhow!("no unlock policy specified for locked filesystem")),
    }
}
