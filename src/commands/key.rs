use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use bcachefs_kernel::c::{bch_key, bch_encrypted_key};
use bch_bindgen::c;
use bch_bindgen::fs::FsExt;
use bcachefs_kernel::fs::{Fs, OpenFs, SbLockGuard};
use bcachefs_kernel::opt_set;
use bch_bindgen::sb::io as sb_io;
use clap::Parser;

use crate::key::{sb_is_encrypted, KeyHandle, Keyring, Passphrase, PassphraseCorrect};

// ---- unlock ----

#[derive(Parser, Debug)]
#[command(about = "Unlock an encrypted filesystem prior to running/mounting")]
pub struct UnlockCli {
    /// Report the device's encryption and lock state, then exit
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

    let encrypted = sb_is_encrypted(&sb);

    // --check reports state without prompting or unlocking. "Unlocked" means
    // the key for this filesystem's UUID is already present in a keyring.
    if cli.check {
        if !encrypted {
            println!("Device has no encryption");
            // Exit nonzero on the unencrypted case. Boot scripts (notably the
            // NixOS initramfs) branch on --check's exit code to decide whether
            // to prompt for unlock; returning 0 here makes an unencrypted root
            // read as "needs unlocking" and prompt for a nonexistent passphrase.
            // Encrypted (locked or unlocked) still exits 0, matching the
            // behaviour from before --check grew the three-state output.
            std::process::exit(1);
        } else if KeyHandle::new_from_search(&sb.sb().uuid()).is_ok() {
            println!("Device is encrypted and unlocked");
        } else {
            println!("Device is encrypted and locked");
        }
        return Ok(());
    }

    if !encrypted {
        bail!("{} is not encrypted", cli.device);
    }

    let passphrase_correct = match cli.file {
        Some(ref file) => Passphrase::read_from_file(file)?
            .check(&sb)
            .ok_or_else(|| anyhow!("incorrect passphrase"))?,
        None => Passphrase::ask_and_check(&sb, None)?,
    };
    KeyHandle::new(&passphrase_correct, cli.keyring)?;

    Ok(())
}

// ---- shared helpers for set/remove-passphrase ----

fn parse_device_list(args: &[String]) -> Vec<PathBuf> {
    if args.len() == 1 && args[0].contains(':') {
        args[0].split(':').map(PathBuf::from).collect()
    } else {
        args.iter().map(PathBuf::from).collect()
    }
}

/// Open a filesystem for superblock modification: never started.
fn open_nostart(devs: &[PathBuf], user_key: Option<&bch_key>) -> Result<OpenFs> {
    let mut opts = c::bch_opts::default();
    opt_set!(opts, nostart, 1);
    opt_set!(opts, will_not_start, 1);
    Fs::open_with_key(devs, opts, user_key)
        .map_err(|e| anyhow::anyhow!("Error opening {:?}: {}", devs, e))
}

/// Open filesystem, verify encryption is enabled, and obtain the raw key.
///
/// If the key is encrypted (passphrase-protected), prompts for and verifies
/// the current passphrase. If the key is unencrypted (formatted with
/// --no_passphrase), reads the raw key directly.
///
/// Asks before opening, from a superblock read ahead: opening a
/// passphrase-protected filesystem needs the key, and without one the open
/// asks for the passphrase itself - reading the line from stdin that we were
/// going to read, so ours got EOF.
fn open_and_verify(devs: &[PathBuf]) -> Result<(OpenFs, bch_key)> {
    let dev_str = devs.iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    let sbs = crate::device_scan::scan_sbs(&dev_str, &c::bch_opts::default())?;
    let sb_handle = &sbs.first()
        .ok_or_else(|| anyhow!("no bcachefs superblock found on {dev_str}"))?.1;

    if sb_handle.sb().crypt().is_none() {
        bail!("Filesystem does not have encryption enabled");
    }

    let (user_key, raw_key) = if sb_is_encrypted(sb_handle) {
        let PassphraseCorrect { passphrase_key, cleartext_sb_key, .. } =
            Passphrase::ask_and_check(sb_handle, None)?;
        (Some(passphrase_key), cleartext_sb_key.into_key())
    } else {
        (None, sb_handle.sb().crypt().unwrap().key().key.clone())
    };

    let paths: Vec<PathBuf> = sbs.iter().map(|(p, _)| p.clone()).collect();
    let fs = open_nostart(&paths, user_key.as_ref())?;
    Ok((fs, raw_key))
}

/// Write a new encrypted key to the crypt superblock field.
fn set_crypt_key(sb: &mut c::bch_sb_handle, key: c::bch_encrypted_key) {
    sb.field_mut::<c::bch_sb_field_crypt>()
        .expect("filesystem has no crypt field")
        .key = key;
}

/// Give the crypt field scrypt parameters, for a key that has never been
/// wrapped with a passphrase: --no_passphrase leaves them unset.
fn init_crypt_kdf(sb: &mut c::bch_sb_handle) {
    let crypt = sb.field_mut::<c::bch_sb_field_crypt>()
        .expect("filesystem has no crypt field");
    unsafe { c::bch_crypt_kdf_init(crypt) };
}

/// Write the changed crypt field out. The open was will_not_start, so the
/// in-memory superblock is what's on disk plus our edit.
fn write_crypt_super(fs: &Fs, sb: &mut SbLockGuard) -> Result<()> {
    fs.write_super_ret(sb)
        .map_err(|e| anyhow!("error writing superblock: {e}"))
}

// ---- set-passphrase ----

#[derive(Parser, Debug)]
#[command(about = "Change passphrase on an existing encrypted (unmounted) filesystem")]
pub struct SetPassphraseCli {
    /// Devices (colon-separated or multiple arguments)
    #[arg(required = true)]
    devices: Vec<String>,
}

fn cmd_set_passphrase(cli: SetPassphraseCli) -> Result<()> {
    let (fs, raw_key) = open_and_verify(&parse_device_list(&cli.devices))?;

    let new_passphrase = Passphrase::ask_for_new_passphrase()
        .context("reading new passphrase")?;

    let mut sb = fs.sb_lock();
    if !sb_is_encrypted(&sb) {
        init_crypt_kdf(&mut sb);
    }

    let encrypted_key = new_passphrase.encrypt_key(&sb, raw_key)?;

    set_crypt_key(&mut sb, encrypted_key);
    unsafe { c::bch2_revoke_key(sb.sb) };
    write_crypt_super(&fs, &mut sb)
}

// ---- remove-passphrase ----

#[derive(Parser, Debug)]
#[command(about = "Remove passphrase protection from an existing encrypted (unmounted) filesystem")]
pub struct RemovePassphraseCli {
    /// Devices (colon-separated or multiple arguments)
    #[arg(required = true)]
    devices: Vec<String>,
}

fn cmd_remove_passphrase(cli: RemovePassphraseCli) -> Result<()> {
    let (fs, raw_key) = open_and_verify(&parse_device_list(&cli.devices))?;

    let mut sb = fs.sb_lock();
    set_crypt_key(&mut sb, bch_encrypted_key::new_unencrypted(raw_key));
    write_crypt_super(&fs, &mut sb)
}

pub const CMD_UNLOCK: super::CmdDef = typed_cmd!("unlock", "Unlock an encrypted filesystem", UnlockCli, cmd_unlock);
pub const CMD_SET_PASSPHRASE: super::CmdDef = typed_cmd!("set-passphrase", "Set or change encryption passphrase", SetPassphraseCli, cmd_set_passphrase);
pub const CMD_REMOVE_PASSPHRASE: super::CmdDef = typed_cmd!("remove-passphrase", "Remove encryption passphrase", RemovePassphraseCli, cmd_remove_passphrase);
