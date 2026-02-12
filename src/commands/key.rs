use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use bch_bindgen::bcachefs::bch_key;
use bch_bindgen::c;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use bch_bindgen::sb::io as sb_io;
use clap::Parser;

use crate::key::{sb_is_encrypted, unencrypted_key, KeyHandle, Keyring, Passphrase};

// ---- unlock ----

#[derive(Parser, Debug)]
#[command(about = "Unlock an encrypted filesystem prior to running/mounting")]
pub struct UnlockCli {
    /// Check if a device is encrypted
    #[arg(short, long)]
    check: bool,

    /// Keyring to add to
    #[arg(short, long, default_value = "user")]
    keyring: Keyring,

    /// Passphrase file to read from (disables passphrase prompt)
    #[arg(short, long)]
    file: Option<PathBuf>,

    /// Device
    device: String,
}

pub fn cmd_unlock(argv: Vec<String>) -> Result<()> {
    let cli = UnlockCli::parse_from(argv);

    let sb = sb_io::read_super(Path::new(&cli.device))
        .with_context(|| format!("Error opening {}", cli.device))?;

    if !sb_is_encrypted(&sb) {
        bail!("{} is not encrypted", cli.device);
    }

    if cli.check {
        return Ok(());
    }

    let uuid = sb.sb().uuid();

    // First attempt
    let passphrase = if let Some(ref file) = cli.file {
        Passphrase::new_from_file(file)?
    } else {
        Passphrase::new_from_prompt(&uuid)?
    };

    match KeyHandle::new(&sb, &passphrase, cli.keyring) {
        Ok(_) => return Ok(()),
        Err(e) if e.to_string().contains("incorrect passphrase") => {}
        Err(e) => return Err(e),
    }

    // Retry up to 2 more times, always interactive
    for _ in 0..2 {
        eprintln!("incorrect passphrase");
        let passphrase = Passphrase::new_from_prompt(&uuid)?;
        match KeyHandle::new(&sb, &passphrase, cli.keyring) {
            Ok(_) => return Ok(()),
            Err(e) if e.to_string().contains("incorrect passphrase") => continue,
            Err(e) => return Err(e),
        }
    }

    bail!("incorrect passphrase limit reached");
}

// ---- shared helpers for set/remove-passphrase ----

fn parse_device_list(args: &[String]) -> Vec<PathBuf> {
    if args.len() == 1 && args[0].contains(':') {
        args[0].split(':').map(PathBuf::from).collect()
    } else {
        args.iter().map(PathBuf::from).collect()
    }
}

/// Open a filesystem with nostart for superblock modification.
fn open_nostart(devs: &[PathBuf]) -> Result<Fs> {
    let mut opts = c::bch_opts::default();
    opt_set!(opts, nostart, 1);
    Fs::open(devs, opts)
        .map_err(|e| anyhow::anyhow!("Error opening {:?}: {}", devs, e))
}

/// Open filesystem, verify encryption is enabled, and verify the current passphrase.
/// Returns the open filesystem and the decrypted raw key.
fn open_and_verify(devs: &[PathBuf]) -> Result<(Fs, bch_key)> {
    let fs = open_nostart(devs)?;
    let sb_handle = fs.sb_handle();

    if sb_handle.sb().crypt().is_none() {
        bail!("Filesystem does not have encryption enabled");
    }

    let uuid = sb_handle.sb().uuid();
    let old_passphrase = Passphrase::new_from_prompt(&uuid)
        .context("reading current passphrase")?;
    let (_, sb_key) = old_passphrase.check(sb_handle)
        .context("verifying current passphrase")?;

    Ok((fs, sb_key.key))
}

/// Write a new encrypted key to the crypt superblock field.
unsafe fn set_crypt_key(fs: &Fs, key: c::bch_encrypted_key) {
    let crypt = bch_bindgen::sb::sb_field_get_mut::<c::bch_sb_field_crypt>(fs.sb_handle().sb)
        .expect("filesystem has no crypt field");
    crypt.key = key;
}

// ---- set-passphrase ----

#[derive(Parser, Debug)]
#[command(about = "Change passphrase on an existing (unmounted) filesystem")]
pub struct SetPassphraseCli {
    /// Devices (colon-separated or multiple arguments)
    #[arg(required = true)]
    devices: Vec<String>,
}

pub fn cmd_set_passphrase(argv: Vec<String>) -> Result<()> {
    let cli = SetPassphraseCli::parse_from(argv);
    let (fs, raw_key) = open_and_verify(&parse_device_list(&cli.devices))?;

    let new_passphrase = Passphrase::new_from_prompt_twice()
        .context("reading new passphrase")?;

    let encrypted_key = new_passphrase.encrypt_key(fs.sb_handle(), &raw_key);

    unsafe {
        set_crypt_key(&fs, encrypted_key);
        c::bch2_revoke_key(fs.sb_handle().sb);
    }
    fs.write_super();

    Ok(())
}

// ---- remove-passphrase ----

#[derive(Parser, Debug)]
#[command(about = "Remove passphrase on an existing (unmounted) filesystem")]
pub struct RemovePassphraseCli {
    /// Devices (colon-separated or multiple arguments)
    #[arg(required = true)]
    devices: Vec<String>,
}

pub fn cmd_remove_passphrase(argv: Vec<String>) -> Result<()> {
    let cli = RemovePassphraseCli::parse_from(argv);
    let (fs, raw_key) = open_and_verify(&parse_device_list(&cli.devices))?;

    unsafe { set_crypt_key(&fs, unencrypted_key(&raw_key)); }
    fs.write_super();

    Ok(())
}
