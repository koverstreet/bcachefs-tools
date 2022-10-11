use tracing::info;

fn check_for_key(key_name: &std::ffi::CStr) -> anyhow::Result<bool> {
	use bch_bindgen::keyutils::{self, keyctl_search};
	let key_name = key_name.to_bytes_with_nul().as_ptr() as *const _;
	let key_type = c_str!("logon");

	let key_id = unsafe { keyctl_search(keyutils::KEY_SPEC_USER_KEYRING, key_type, key_name, 0) };
	if key_id > 0 {
		info!("Key has became available");
		Ok(true)
	} else if errno::errno().0 != libc::ENOKEY {
		Err(crate::ErrnoError(errno::errno()).into())
	} else {
		Ok(false)
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
use crate::filesystem::FileSystem;
fn ask_for_key(fs: &FileSystem) -> anyhow::Result<()> {
	use anyhow::anyhow;
	use byteorder::{LittleEndian, ReadBytesExt};
	use bch_bindgen::bcachefs::{self, bch2_chacha_encrypt_key, bch_encrypted_key, bch_key};
	use std::os::raw::c_char;

	let key_name = std::ffi::CString::new(format!("bcachefs:{}", fs.uuid())).unwrap();
	if check_for_key(&key_name)? {
		return Ok(());
	}

	let bch_key_magic = BCH_KEY_MAGIC.as_bytes().read_u64::<LittleEndian>().unwrap();
	let crypt = fs.sb().sb().crypt().unwrap();
	let pass = rpassword::read_password_from_tty(Some("Enter passphrase: "))?;
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
			fs.sb().sb().nonce(),
			&mut key as *mut _ as *mut _,
			std::mem::size_of::<bch_encrypted_key>() as u64,
		)
	};
	if ret != 0 {
		Err(anyhow!("chacha decryption failure"))
	} else if key.magic != bch_key_magic {
		Err(anyhow!("failed to verify the password"))
	} else {
		let key_type = c_str!("logon");
		let ret = unsafe {
			bch_bindgen::keyutils::add_key(
				key_type,
				key_name.as_c_str().to_bytes_with_nul() as *const _ as *const c_char,
				&output as *const _ as *const _,
				std::mem::size_of::<bch_key>() as u64,
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

#[tracing_attributes::instrument]
pub fn prepare_key(fs: &FileSystem, password: crate::KeyLocation) -> anyhow::Result<()> {
	use crate::KeyLocation::*;
	use anyhow::anyhow;

	tracing::info!(msg = "checking if key exists for filesystem");
	match password {
		Fail => Err(anyhow!("no key available")),
		Wait => Ok(wait_for_key(fs.uuid())?),
		Ask => ask_for_key(fs),
	}
}
