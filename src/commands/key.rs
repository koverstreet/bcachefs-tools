use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use bch_bindgen::bcachefs::bch_key;
use bch_bindgen::c;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use bch_bindgen::sb::io as sb_io;
use clap::Parser;

use crate::key::{
    sb_is_encrypted, unencrypted_key, CorrectPassphrase, KeyHandle, Keyring, Passphrase,
};

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

fn cmd_unlock(cli: UnlockCli) -> Result<()> {

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
        Passphrase::new(&uuid)?
    };

    if let Some(correct) = passphrase.check(&sb)? {
        KeyHandle::new(&correct, cli.keyring)?;
        return Ok(());
    }

    // Retry up to 2 more times, always interactive
    for _ in 0..2 {
        eprintln!("incorrect passphrase");
        let passphrase = Passphrase::new_from_prompt(&uuid, true)?;
        if let Some(correct) = passphrase.check(&sb)? {
            KeyHandle::new(&correct, cli.keyring)?;
            return Ok(());
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
    crate::device_scan::open_scan(devs, opts)
        .map_err(|e| anyhow::anyhow!("Error opening {:?}: {}", devs, e))
}

/// Open filesystem, verify encryption is enabled, and obtain the raw key.
///
/// If the key is encrypted (passphrase-protected), prompts for and verifies
/// the current passphrase. If the key is unencrypted (formatted with
/// --no_passphrase), reads the raw key directly.
fn open_and_verify(devs: &[PathBuf]) -> Result<(Fs, bch_key)> {
    let fs = open_nostart(devs)?;
    let sb_handle = fs.sb_handle();

    if sb_handle.sb().crypt().is_none() {
        bail!("Filesystem does not have encryption enabled");
    }

    if sb_is_encrypted(sb_handle) {
        let uuid = sb_handle.sb().uuid();
        let old_passphrase =
            Passphrase::new_from_prompt(&uuid, true).context("reading current passphrase")?;
        let CorrectPassphrase { sb_key, .. } = old_passphrase
            .check(sb_handle)?
            .context("verifying current passphrase")?;
        Ok((fs, sb_key.key))
    } else {
        let raw_key = sb_handle.sb().crypt().unwrap().key().key;
        Ok((fs, raw_key))
    }
}

/// Write a new encrypted key to the crypt superblock field.
///
/// # Safety
/// Caller must hold sb_lock.
unsafe fn set_crypt_key(fs: &Fs, key: c::bch_encrypted_key) {
    let disk_sb = &mut (*fs.raw).disk_sb;
    let crypt: &mut c::bch_sb_field_crypt = bch_bindgen::sb::sb_field_get_mut(disk_sb)
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

fn cmd_set_passphrase(cli: SetPassphraseCli) -> Result<()> {
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

fn cmd_remove_passphrase(cli: RemovePassphraseCli) -> Result<()> {
    let (fs, raw_key) = open_and_verify(&parse_device_list(&cli.devices))?;

    unsafe { set_crypt_key(&fs, unencrypted_key(&raw_key)); }
    fs.write_super();

    Ok(())
}

pub const CMD_UNLOCK: super::CmdDef = typed_cmd!("unlock", "Unlock an encrypted filesystem", UnlockCli, cmd_unlock);
pub const CMD_SET_PASSPHRASE: super::CmdDef = typed_cmd!("set-passphrase", "Set or change passphrase", SetPassphraseCli, cmd_set_passphrase);
pub const CMD_REMOVE_PASSPHRASE: super::CmdDef = typed_cmd!("remove-passphrase", "Remove passphrase", RemovePassphraseCli, cmd_remove_passphrase);
