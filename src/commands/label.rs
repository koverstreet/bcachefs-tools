use std::ffi::CStr;

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::wrappers::handle::BcachefsHandle;

const FSLABEL_MAX: usize = 256;
const BCH_SB_LABEL_SIZE: usize = 32;

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const FS_IOC_MAGIC: u32 = 0x94;
const FS_IOC_GETFSLABEL_NR: u32 = 49;
const FS_IOC_SETFSLABEL_NR: u32 = 50;

const fn fs_ioc<T>(dir: u32, nr: u32) -> libc::Ioctl {
    ((dir << 30) | ((std::mem::size_of::<T>() as u32) << 16) | (FS_IOC_MAGIC << 8) | nr)
        as libc::Ioctl
}

const FS_IOC_GETFSLABEL: libc::Ioctl = fs_ioc::<[u8; FSLABEL_MAX]>(IOC_READ, FS_IOC_GETFSLABEL_NR);
const FS_IOC_SETFSLABEL: libc::Ioctl = fs_ioc::<[u8; FSLABEL_MAX]>(IOC_WRITE, FS_IOC_SETFSLABEL_NR);

#[derive(Parser, Debug)]
#[command(about = "Print filesystem label")]
pub struct GetLabelCli {
    /// Mounted filesystem path
    target: String,
}

#[derive(Parser, Debug)]
#[command(about = "Set filesystem label")]
pub struct SetLabelCli {
    /// Mounted filesystem path
    target: String,

    /// New filesystem label
    label: String,
}

fn get_label(cli: GetLabelCli) -> Result<()> {
    let handle = BcachefsHandle::open(&cli.target)
        .with_context(|| format!("opening mounted filesystem '{}'", cli.target))?;
    let mut label = [0u8; FSLABEL_MAX];

    let ret = unsafe { libc::ioctl(handle.ioctl_fd_raw(), FS_IOC_GETFSLABEL, label.as_mut_ptr()) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error()).context("FS_IOC_GETFSLABEL");
    }

    let label = CStr::from_bytes_until_nul(&label)
        .context("filesystem label is not nul-terminated")?
        .to_string_lossy();
    println!("{label}");
    Ok(())
}

fn set_label(cli: SetLabelCli) -> Result<()> {
    if cli.label.as_bytes().len() >= BCH_SB_LABEL_SIZE {
        bail!(
            "filesystem label too long (max {} characters)",
            BCH_SB_LABEL_SIZE - 1
        );
    }

    let handle = BcachefsHandle::open(&cli.target)
        .with_context(|| format!("opening mounted filesystem '{}'", cli.target))?;
    let mut label = [0u8; FSLABEL_MAX];
    label[..cli.label.len()].copy_from_slice(cli.label.as_bytes());

    let ret = unsafe { libc::ioctl(handle.ioctl_fd_raw(), FS_IOC_SETFSLABEL, label.as_ptr()) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error()).context("FS_IOC_SETFSLABEL");
    }
    Ok(())
}

pub const CMD_GET_LABEL: super::CmdDef = typed_cmd!(
    "get-label",
    "Print filesystem label",
    aliases: ["label"],
    GetLabelCli,
    get_label
);

pub const CMD_SET_LABEL: super::CmdDef =
    typed_cmd!("set-label", "Set filesystem label", SetLabelCli, set_label);
