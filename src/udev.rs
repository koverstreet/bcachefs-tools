//! What device_scan asks of udev, read where libudev reads it (systemd's
//! sd-device): the block devices udevd has processed, with the properties it
//! gave them, and a wakeup when a block device's event arrives.
//!
//! A block device is in /sys/class/block, and its uevent there has its
//! MAJOR, MINOR and DEVNAME. udevd's database for it is
//! /run/udev/data/b<major>:<minor> - "K:value" lines, its properties the E:
//! ones - and that it exists is what initialized means.
//!
//! Events are uevents, on a NETLINK_KOBJECT_UEVENT socket: the kernel's group
//! 1, as "action@devpath\0" then the device's NUL separated KEY=VALUEs, and
//! udevd's group 2, the same KEY=VALUEs after its own header - "libudev\0",
//! a big endian 0xfeedcafe, and where the properties start, in host order at
//! byte 16. Anyone can send to either group, which libudev guards against by
//! the sender's uid: an event here is only a reason to look again, at sysfs
//! and the database, so a forged one costs a rescan.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::io::Errno;
use rustix::net::{bind, netlink, recv, socket_with, AddressFamily, RecvFlags, SocketFlags, SocketType};

/// A block device udevd has processed.
pub struct BlockDevice {
    pub devnode: PathBuf,
    properties:  HashMap<String, String>,
}

impl BlockDevice {
    pub fn property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).map(String::as_str)
    }
}

/// None if udevd hasn't processed it, or it's gone.
fn block_device(sysdir: &Path) -> io::Result<Option<BlockDevice>> {
    let uevent = match fs::read_to_string(sysdir.join("uevent")) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let uevent: HashMap<_, _> = uevent.lines().filter_map(|l| l.split_once('=')).collect();
    let (Some(major), Some(minor), Some(devname)) =
        (uevent.get("MAJOR"), uevent.get("MINOR"), uevent.get("DEVNAME")) else {
        return Ok(None);
    };

    let db = match fs::read_to_string(format!("/run/udev/data/b{major}:{minor}")) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let properties = db.lines()
        .filter_map(|l| l.strip_prefix("E:"))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    Ok(Some(BlockDevice { devnode: Path::new("/dev").join(devname), properties }))
}

/// Every block device udevd has processed: libudev's enumeration of the
/// block subsystem, initialized devices only.
pub fn block_devices() -> io::Result<Vec<BlockDevice>> {
    let mut ret = Vec::new();
    for entry in fs::read_dir("/sys/class/block")? {
        if let Some(dev) = block_device(&entry?.path())? {
            ret.push(dev);
        }
    }
    Ok(ret)
}

/// udevd's own test for whether it is running - libudev checks this same
/// socket in udev_queue_get_udev_is_active().
fn udevd_running() -> bool {
    Path::new("/run/udev/control").exists()
}

const UDEV_MONITOR_MAGIC: u32 = 0xfeedcafe;

/// Whether uevent @msg is a block device's.
fn is_block_event(msg: &[u8]) -> bool {
    let properties = if let Some(header) = msg.strip_prefix(b"libudev\0") {
        let word = |i: usize| header.get(i..i + 4).and_then(|b| <[u8; 4]>::try_from(b).ok());
        let (Some(magic), Some(off)) = (word(0), word(8)) else { return false };
        if u32::from_be_bytes(magic) != UDEV_MONITOR_MAGIC {
            return false;
        }
        msg.get(u32::from_ne_bytes(off) as usize..)
    } else {
        msg.iter().position(|&b| b == 0).map(|i| &msg[i + 1..])
    };

    properties.is_some_and(|p| p.split(|&b| b == 0).any(|kv| kv == b"SUBSYSTEM=block"))
}

/// Block device events. The two groups are not interchangeable: udevd sends
/// to its own after processing an event, the kernel to its own
/// (kobject_uevent). In an initramfs - the case device_scan's wait exists
/// for - udevd's is a group nobody sends to.
///
/// So udevd's when it is running: its events arrive after device-mapper and
/// md names are set up, where a kernel event for those can land before there
/// is anything to find. Callers rescan rather than trust the devnode in the
/// event, so either source carries all they need.
pub struct BlockMonitor(OwnedFd);

impl BlockMonitor {
    pub fn new() -> io::Result<Self> {
        let group = if udevd_running() { 2 } else { 1 };
        let fd = socket_with(AddressFamily::NETLINK, SocketType::DGRAM,
                             SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
                             Some(netlink::KOBJECT_UEVENT))?;
        bind(&fd, &netlink::SocketAddrNetlink::new(0, group))?;
        Ok(BlockMonitor(fd))
    }

    /// Whether a block device's event has arrived since the last call,
    /// reading all that have. Events lost to a full socket might have been.
    pub fn drain(&self) -> io::Result<bool> {
        let mut buf = vec![0u8; 16 << 10];
        let mut ret = false;
        loop {
            match recv(&self.0, &mut buf[..], RecvFlags::empty()) {
                Ok((n, _)) => ret |= is_block_event(&buf[..n]),
                Err(Errno::AGAIN) => return Ok(ret),
                Err(Errno::NOBUFS) => ret = true,
                Err(Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl AsFd for BlockMonitor {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::is_block_event;

    #[test]
    fn kernel_events() {
        assert!(is_block_event(b"add@/devices/virtual/block/loop0\0ACTION=add\0SUBSYSTEM=block\0DEVNAME=loop0\0"));
        assert!(!is_block_event(b"add@/devices/pci0000:00/usb1\0ACTION=add\0SUBSYSTEM=usb\0"));
        // the header isn't a property
        assert!(!is_block_event(b"SUBSYSTEM=block\0"));
    }

    #[test]
    fn udev_events() {
        let msg = |magic: u32, props: &[u8]| {
            let mut m = b"libudev\0".to_vec();
            m.extend_from_slice(&magic.to_be_bytes());
            m.extend_from_slice(&40u32.to_ne_bytes());          // header_size
            m.extend_from_slice(&40u32.to_ne_bytes());          // properties_off
            m.extend_from_slice(&(props.len() as u32).to_ne_bytes());
            m.resize(40, 0);                                    // the filter hashes
            m.extend_from_slice(props);
            m
        };
        assert!(is_block_event(&msg(0xfeedcafe, b"ACTION=add\0SUBSYSTEM=block\0")));
        assert!(!is_block_event(&msg(0xfeedcafe, b"ACTION=add\0SUBSYSTEM=net\0")));
        assert!(!is_block_event(&msg(0xdeadbeef, b"ACTION=add\0SUBSYSTEM=block\0")));
        assert!(!is_block_event(b"libudev\0short"));
    }
}
