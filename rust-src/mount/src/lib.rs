use anyhow::anyhow;
use structopt::StructOpt;

pub mod err {
	pub enum GError {
		Unknown{
			message: std::borrow::Cow<'static, String>
		 }
	}
	pub type GResult<T, E, OE> =::core::result::Result< ::core::result::Result<T, E>, OE>;
	pub type Result<T, E> = GResult<T, E, GError>;
}

#[macro_export]
macro_rules! c_str {
	($lit:expr) => {
		unsafe {
			std::ffi::CStr::from_ptr(concat!($lit, "\0").as_ptr() as *const std::os::raw::c_char)
				.to_bytes_with_nul()
				.as_ptr() as *const std::os::raw::c_char
		}
	};
}

#[derive(Debug)]
struct ErrnoError(errno::Errno);
impl std::fmt::Display for ErrnoError {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
		self.0.fmt(f)
	}
}
impl std::error::Error for ErrnoError {}

#[derive(Debug)]
pub enum KeyLocation {
	Fail,
	Wait,
	Ask,
}

#[derive(Debug)]
pub struct KeyLoc(pub Option<KeyLocation>);
impl std::ops::Deref for KeyLoc {
	type Target = Option<KeyLocation>;
	fn deref(&self) -> &Self::Target {
		&self.0
	}
}
impl std::str::FromStr for KeyLoc {
	type Err = anyhow::Error;
	fn from_str(s: &str) -> anyhow::Result<Self> {
		// use anyhow::anyhow;
		match s {
			"" => Ok(KeyLoc(None)),
			"fail" => Ok(KeyLoc(Some(KeyLocation::Fail))),
			"wait" => Ok(KeyLoc(Some(KeyLocation::Wait))),
			"ask" => Ok(KeyLoc(Some(KeyLocation::Ask))),
			_ => Err(anyhow!("invalid password option")),
		}
	}
}

#[derive(StructOpt, Debug)]
/// Mount a bcachefs filesystem by its UUID.
pub struct Options {
	/// Where the password would be loaded from.
	///
	/// Possible values are:
	/// "fail" - don't ask for password, fail if filesystem is encrypted;
	/// "wait" - wait for password to become available before mounting;
	/// "ask" -  prompt the user for password;
	#[structopt(short, long, default_value = "")]
	pub key_location: KeyLoc,

	/// External UUID of the bcachefs filesystem
	pub uuid: uuid::Uuid,

	/// Where the filesystem should be mounted. If not set, then the filesystem
	/// won't actually be mounted. But all steps preceding mounting the
	/// filesystem (e.g. asking for passphrase) will still be performed.
	pub mountpoint: Option<std::path::PathBuf>,

	/// Mount options
	#[structopt(short, default_value = "")]
	pub options: String,
}

pub mod filesystem;
pub mod key;

// pub fn mnt_in_use()
