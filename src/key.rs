use log::{info};
use bch_bindgen::bcachefs::bch_sb_handle;
use clap::builder::PossibleValue;
use crate::c_str;
use anyhow::anyhow;

#[derive(Clone, Debug)]
pub enum KeyLocation {
    None,
    Fail,
    Wait,
    Ask,
}

impl std::str::FromStr for KeyLocation {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            ""|"none" => Ok(KeyLocation::None),
            "fail"    => Ok(KeyLocation::Fail),
            "wait"    => Ok(KeyLocation::Wait),
            "ask"     => Ok(KeyLocation::Ask),
            _         => Err(anyhow!("invalid password option")),
        }
    }
}

impl clap::ValueEnum for KeyLocation {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            KeyLocation::None,
            KeyLocation::Fail,
            KeyLocation::Wait,
            KeyLocation::Ask,
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

fn check_for_key(key_name: &std::ffi::CStr) -> anyhow::Result<bool> {
    use bch_bindgen::keyutils::{self, keyctl_search};
    let key_name = key_name.to_bytes_with_nul().as_ptr() as *const _;
    let key_type = c_str!("user");

    let key_id = unsafe { keyctl_search(keyutils::KEY_SPEC_USER_KEYRING, key_type, key_name, 0) };
    if key_id > 0 {
        info!("Key has became available");
        Ok(true)
    } else {
        match errno::errno().0 {
            libc::ENOKEY | libc::EKEYREVOKED => Ok(false),
            _ => Err(crate::ErrnoError(errno::errno()).into()),
        }
    }
}

fn wait_for_key(uuid: &uuid::Uuid) -> anyhow::Result<()> {
    let key_name = std::ffi::CString::new(format!("bcachefs:{}", uuid)).unwrap();
    loop {
        if check_for_key(&key_name)? {
            break Ok(());
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

const BCH_KEY_MAGIC: &str = "bch**key";
fn ask_for_key(sb: &bch_sb_handle) -> anyhow::Result<()> {
    use bch_bindgen::bcachefs::{self, bch2_chacha_encrypt_key, bch_encrypted_key, bch_key};
    use byteorder::{LittleEndian, ReadBytesExt};
    use std::os::raw::c_char;

    let key_name = std::ffi::CString::new(format!("bcachefs:{}", sb.sb().uuid())).unwrap();
    if check_for_key(&key_name)? {
        return Ok(());
    }

    let bch_key_magic = BCH_KEY_MAGIC.as_bytes().read_u64::<LittleEndian>().unwrap();
    let crypt = sb.sb().crypt().unwrap();
    let pass = if atty::is(atty::Stream::Stdin) {
        rpassword::prompt_password("Enter passphrase: ")?
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line
    };
    let pass = std::ffi::CString::new(pass.trim_end())?; // bind to keep the CString alive
    let mut output: bch_key = unsafe {
        bcachefs::derive_passphrase(
            crypt as *const _ as *mut _,
            pass.as_c_str().to_bytes_with_nul().as_ptr() as *const _,
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

pub fn prepare_key(sb: &bch_sb_handle, password: KeyLocation) -> anyhow::Result<()> {
    info!("checking if key exists for filesystem {}", sb.sb().uuid());
    match password {
        KeyLocation::Fail => Err(anyhow!("no key available")),
        KeyLocation::Wait => Ok(wait_for_key(&sb.sb().uuid())?),
        KeyLocation::Ask => ask_for_key(sb),
        _ => Err(anyhow!("no keyoption specified for locked filesystem")),
    }
}
