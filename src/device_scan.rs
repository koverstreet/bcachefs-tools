//! Finding a filesystem's member devices, all of which a mount needs.
//!
//! Three ways, in cost order. udev's database is fast but only knows devices
//! it has already tagged, which at boot may be none of them. A block scan
//! reads every superblock on the machine, which is slow but needs nothing
//! running - /proc/partitions when udev is unavailable (#344).
//!
//! Neither helps with a device that has not appeared yet, which is what
//! mounting by UUID at boot looks like, so a short search waits for one to
//! arrive and looks again, bounded by missing_dev_timeout (#308, #393). Only
//! when searching for a filesystem: a caller who names paths has already
//! decided what exists.

use std::{
    collections::HashSet,
    ffi::{CStr, CString, c_char, OsString, OsStr},
    fs,
    os::fd::{AsRawFd, BorrowedFd},
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    thread::sleep,
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use bch_bindgen::fs::FsExt;
use bcachefs_kernel::{c, opt_defined, opt_get, opt_set};
use bcachefs_kernel::errcode::BchError;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::util::darray::DarrayVec;
use c::bch_sb_handle;
use c::bch_opts;
use uuid::Uuid;
use log::{debug, warn};

use crate::device_multipath::{
    find_multipath_holder, preferred_multipath_devnode, warn_multipath_component,
};

pub fn read_super_silent(path: impl AsRef<Path>, mut opts: bch_opts) -> Result<bch_sb_handle, BchError> {
    opt_set!(opts, noexcl, 1);
    opt_set!(opts, nochanges, 1);
    opt_set!(opts, no_version_check, 1);

    bch_bindgen::sb::io::read_super_silent(path.as_ref(), opts)
}

pub fn should_skip_multipath_component(dev: &udev::Device) -> bool {
    // Set by multipath's udev rule; fall back to sysfs if not present.
    if dev
        .property_value("DM_MULTIPATH_DEVICE_PATH")
        .is_some_and(|v| v == "1")
    {
        if let Some(devnode) = dev.devnode() {
            debug!("Skipping multipath component device: {}", devnode.display());
        }
        return true;
    }

    if let Some(devnode) = dev.devnode() {
        if find_multipath_holder(devnode).is_some() {
            debug!(
                "Skipping multipath component device via sysfs holders: {}",
                devnode.display()
            );
            return true;
        }
    }

    false
}

fn get_devices_by_uuid_udev(uuid: Uuid) -> anyhow::Result<Vec<PathBuf>> {
    debug!("Walking udev db!");

    let mut enumerator = udev::Enumerator::new()?;
    enumerator.match_is_initialized()?;
    enumerator.match_subsystem("block")?;
    enumerator.match_property("ID_FS_TYPE", "bcachefs")?;

    Ok(enumerator
        .scan_devices()?
        .filter(|dev| {
            dev.property_value("ID_FS_UUID")
                .and_then(OsStr::to_str)
                .and_then(|s| Uuid::parse_str(s).ok())
                .is_some_and(|dev_uuid| dev_uuid == uuid)
                && !should_skip_multipath_component(dev)
        })
        .filter_map(|dev| dev.devnode().map(Path::to_path_buf))
        .collect::<Vec<_>>())
}

fn get_bcachefs_devnodes_udev() -> anyhow::Result<Vec<PathBuf>> {
    let mut enumerator = udev::Enumerator::new()?;
    enumerator.match_is_initialized()?;
    enumerator.match_subsystem("block")?;
    enumerator.match_property("ID_FS_TYPE", "bcachefs")?;

    Ok(enumerator
        .scan_devices()?
        .filter(|dev| !should_skip_multipath_component(dev))
        .filter_map(|dev| dev.devnode().map(Path::to_path_buf))
        .collect::<Vec<_>>())
}

fn get_all_block_devnodes_udev() -> anyhow::Result<Vec<PathBuf>> {
    let mut udev = udev::Enumerator::new()?;
    udev.match_is_initialized()?;
    udev.match_subsystem("block")?;

    let devices = udev
        .scan_devices()?
        .filter_map(|dev| dev.devnode().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    Ok(devices)
}

/// Scan /proc/partitions for block devices. Works without udev.
fn get_all_block_devnodes_procfs() -> anyhow::Result<Vec<PathBuf>> {
    let contents = fs::read_to_string("/proc/partitions")?;
    let devices = contents
        .lines()
        .skip(2) // skip header lines
        .filter_map(|line| {
            let name = line.split_whitespace().nth(3)?;
            let path = Path::new("/dev").join(name);
            if path.exists() {
                Some(path.to_path_buf())
            } else {
                None
            }
        })
        .collect();
    Ok(devices)
}

fn get_all_block_devnodes() -> anyhow::Result<Vec<PathBuf>> {
    match get_all_block_devnodes_udev() {
        Ok(devs) if !devs.is_empty() => Ok(devs),
        Ok(_) => {
            debug!("udev returned no block devices, falling back to /proc/partitions");
            get_all_block_devnodes_procfs()
        }
        Err(e) => {
            debug!("udev block scan failed ({}), falling back to /proc/partitions", e);
            get_all_block_devnodes_procfs()
        }
    }
}

fn read_sbs_matching_uuid(
    uuid: Uuid,
    devices: &[PathBuf],
    opts: &bch_opts,
    filter_multipath: bool,
) -> Result<Vec<(PathBuf, bch_sb_handle)>, BchError> {
	let sbs = devices
		.iter()
		.filter(|dev| {
			// When not using udev (which already filters), skip multipath components
			if filter_multipath && find_multipath_holder(dev).is_some() {
				debug!(
					"Skipping multipath component device in fallback scan: {}",
					dev.display()
				);
				return false;
			}
			true
		})
		.filter_map(|dev| {
			read_super_silent(dev, *opts)
				.ok()
				.map(|sb| {
					let path = preferred_multipath_devnode(dev).unwrap_or_else(|| dev.to_path_buf());
					(path, sb)
				})
		})
		.filter(|(_, sb)| sb.sb().uuid() == uuid)
		.collect::<Vec<_>>();

	filter_current_sbs(sbs, opts)
}

fn sb_label_matches(sb: &c::bch_sb, label: &str) -> bool {
    let label_len = sb.label.iter()
        .position(|&b| b == 0)
        .unwrap_or(sb.label.len());

    &sb.label[..label_len] == label.as_bytes()
}

fn read_sbs_matching_label(
    label: &str,
    devices: &[PathBuf],
    opts: &bch_opts,
    filter_multipath: bool,
) -> Result<Vec<(PathBuf, bch_sb_handle)>, BchError> {
    let sbs = devices
        .iter()
        .filter(|dev| {
            if filter_multipath && find_multipath_holder(dev).is_some() {
                debug!(
                    "Skipping multipath component device in fallback scan: {}",
                    dev.display()
                );
                return false;
            }
            true
        })
        .filter_map(|dev| {
            read_super_silent(dev, *opts)
                .ok()
                .map(|sb| (PathBuf::from(dev), sb))
        })
        .filter(|(_, sb)| sb_label_matches(sb.sb(), label))
        .collect::<Vec<_>>();

    filter_current_sbs(sbs, opts)
}

fn sb_handle_path(sb: &bch_sb_handle) -> PathBuf {
	if sb.sb_name.is_null() {
		PathBuf::new()
	} else {
		unsafe {
			PathBuf::from(OsString::from_vec(
				CStr::from_ptr(sb.sb_name).to_bytes().to_vec()))
		}
	}
}

pub fn filter_current_sbs(
	sbs: Vec<(PathBuf, bch_sb_handle)>,
	opts: &bch_opts,
) -> Result<Vec<(PathBuf, bch_sb_handle)>, BchError> {
	let mut opts = *opts;

	// Ahead of bch2_sbs_filter_dead(), which frees the superblock of every
	// device it drops - and drops one that diverged the same way it drops one
	// that was properly removed, leaving nothing to tell them apart afterwards.
	// The short device count then reads as "a disk is missing", so mount asks
	// whether to go degraded, and yes silently picks one of two histories.
	if opt_get!(opts, no_splitbrain_check) == 0 {
		let divergent = crate::splitbrain::find(&sbs, &opts);
		if !divergent.is_empty() {
			// One warning, not one per line: the report is a single
			// account with blank lines in it for readability, and a
			// warn! per line stamps file:line on every one of them -
			// including the blanks.
			warn!("{}", crate::splitbrain::report(&sbs, &divergent).trim_end());
			return Err(BchError::from_errcode(c::bch_errcode::BCH_ERR_device_splitbrain));
		}
	}

	let handles = sbs.into_iter()
		.map(|(_, sb)| sb)
		.collect::<Vec<_>>();
	let mut handles = DarrayVec::<c::bch_sb_handles, bch_sb_handle>::from_vec(handles);

	let ret = unsafe {
		c::bch2_sbs_filter_dead(handles.as_mut(), &mut opts, std::ptr::null_mut())
	};
	if ret != 0 {
		return Err(BchError::from_raw(-ret));
	}

	let handles = handles.into_vec();
	let mut filtered = Vec::with_capacity(handles.len());
	for sb in handles {
		let path = sb_handle_path(&sb);
		let path = preferred_multipath_devnode(&path).unwrap_or(path);
		filtered.push((path, sb));
	}

	Ok(filtered)
}

pub fn get_devices_by_uuid(
    uuid: Uuid,
    opts: &bch_opts,
    use_udev: bool
) -> anyhow::Result<Vec<(PathBuf, bch_sb_handle)>> {
    if use_udev {
        let devs_from_udev = get_devices_by_uuid_udev(uuid)?;

        if !devs_from_udev.is_empty() {
	    let sbs = read_sbs_matching_uuid(uuid, &devs_from_udev, opts, false)?;

            // Check if udev found all expected devices. During early boot,
            // udev may not have finished processing all devices yet — if we
            // got fewer than expected, fall back to scanning all block devices.
            if have_every_device(&sbs) {
                return Ok(sbs);
            }

            debug!("udev found {}/{} devices for UUID {}, falling back to block scan",
                present_devices(&sbs).len(), expected_devices(&sbs), uuid);
        }
    }

    // Falls back to /proc/partitions if udev is unavailable, so this works
    // without udevd running.
    let all_devs = get_all_block_devnodes()?;
    Ok(read_sbs_matching_uuid(uuid, &all_devs, opts, true)?)
}

/// How long to wait for member devices before any of them have been found.
///
/// The filesystem's own missing_dev_timeout is the number we want, but it's on
/// a disk we can't read yet, so the first stretch of the wait has to run on a
/// built-in. Once any member turns up, that filesystem's value takes over.
const DEFAULT_MISSING_DEV_TIMEOUT: Duration = Duration::from_secs(30);

/// Only for the case where there is nothing to be notified by: udev isn't
/// trusted (-o mount_trusts_udev=0) or isn't running, so a rescan is the only
/// way to learn anything. Slow on purpose - a rescan reads the superblock of
/// every block device on the machine.
const NO_UDEV_RESCAN_INTERVAL: Duration = Duration::from_secs(1);

/// Members the filesystem should have, according to what we found. Zero when
/// we found nothing at all - not "no devices", but "don't know yet".
pub fn expected_devices(sbs: &[(PathBuf, bch_sb_handle)]) -> usize {
    sbs.first()
        .map(|(_, sb)| sb.sb().number_of_devices() as usize)
        .unwrap_or(0)
}

/// By dev_idx, because a scan produces paths and the same device turns up
/// under more than one - multipath, or udev and the block scan both
/// contributing. Counting paths calls the set complete with a member missing.
pub fn present_devices(sbs: &[(PathBuf, bch_sb_handle)]) -> HashSet<u8> {
    sbs.iter().map(|(_, sb)| sb.sb().dev_idx).collect()
}

/// Are all the members here?
fn have_every_device(sbs: &[(PathBuf, bch_sb_handle)]) -> bool {
    let expected = expected_devices(sbs);

    expected != 0 && present_devices(sbs).len() >= expected
}

/// How long to keep waiting.
///
/// -o missing_dev_timeout wins: the option is OPT_MOUNT, so someone who passes
/// it means this mount, not this filesystem. Otherwise the filesystem's own
/// value, if we've found enough of one to read it. Zero on disk means "unset",
/// since every filesystem written before the option existed reads back zero, so
/// it falls through to the same built-in as having found nothing at all.
fn missing_dev_timeout(sbs: &[(PathBuf, bch_sb_handle)], cli_opts: &bch_opts) -> Duration {
    if opt_defined!(cli_opts, missing_dev_timeout) != 0 {
        return Duration::from_secs(opt_get!(cli_opts, missing_dev_timeout) as u64);
    }

    let Some((_, sb)) = sbs.first() else {
        return DEFAULT_MISSING_DEV_TIMEOUT;
    };

    let mut sb_opts: bch_opts = Default::default();
    if unsafe { c::bch2_opts_from_sb(&mut sb_opts, sb.sb) } != 0 {
        return DEFAULT_MISSING_DEV_TIMEOUT;
    }

    match opt_get!(sb_opts, missing_dev_timeout) {
        0 => DEFAULT_MISSING_DEV_TIMEOUT,
        secs => Duration::from_secs(secs as u64),
    }
}

/// udevd's own test for whether it is running - libudev checks this same
/// socket in udev_queue_get_udev_is_active().
fn udevd_running() -> bool {
    Path::new("/run/udev/control").exists()
}

/// The two netlink groups are not interchangeable: `new()` subscribes to the
/// one udevd writes after processing an event, `new_kernel()` to the one the
/// kernel writes (kobject_uevent). In an initramfs - the case this whole wait
/// exists for - a `new()` monitor is a socket nobody writes to.
///
/// Prefer udevd's when it is running: its events arrive after device-mapper
/// and md names are set up, where a kernel event for those can land before
/// there is anything to find. Callers rescan rather than trust the devnode in
/// the event, so either source carries all they need.
fn block_device_monitor() -> Option<udev::MonitorSocket> {
    let builder = if udevd_running() {
        udev::MonitorBuilder::new()
    } else {
        udev::MonitorBuilder::new_kernel()
    };

    builder.ok()?.match_subsystem("block").ok()?.listen().ok()
}

/// Event-driven rather than polled: a rescan reads the superblock of every
/// block device on the machine, so polling one would keep every spun-down disk
/// awake for the length of a boot to learn nothing.
///
/// The monitor is built before the first scan on purpose. A device arriving in
/// the gap would otherwise be in neither - too late for the scan, too early
/// for a socket that did not exist yet - and we would wait out the whole
/// timeout with it sitting there.
///
/// Returns a short set rather than failing: what to do about missing members
/// is the degraded action's question, not this one's.
fn scan_waiting_for_devices<F>(cli_opts: &bch_opts, scan: F)
    -> Result<Vec<(PathBuf, bch_sb_handle)>>
where
    F: Fn() -> Result<Vec<(PathBuf, bch_sb_handle)>>,
{
    let socket = block_device_monitor();

    let start = Instant::now();
    let mut sbs = scan()?;
    let mut announced = None;

    while !have_every_device(&sbs) {
        let timeout = missing_dev_timeout(&sbs, cli_opts);
        let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
            return Ok(sbs);
        };

        // Not per event - a boot that pauses here should say why, but the
        // console is not the place for a progress bar. Keyed on the timeout
        // rather than a bare flag because the timeout changes: until the first
        // member turns up we are working off the built-in, and the number we
        // said out loud would otherwise be one nobody is waiting for.
        if announced != Some(timeout) {
            announced = Some(timeout);
            match expected_devices(&sbs) {
                0 => warn!("no devices found yet, waiting up to {}s", timeout.as_secs()),
                n => warn!("found {} of {n} devices, waiting up to {}s for the rest",
                           present_devices(&sbs).len(), timeout.as_secs()),
            }
        }

        match &socket {
            Some(socket) => {
                let fd = unsafe { BorrowedFd::borrow_raw(socket.as_raw_fd()) };
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];

                let deadline = Timespec {
                    tv_sec:  remaining.as_secs() as _,
                    tv_nsec: remaining.subsec_nanos() as _,
                };

                poll(&mut fds, Some(&deadline))?;
                if fds.iter().any(|fd| fd.revents().contains(PollFlags::ERR)) {
                    bail!("error on udev socket fd");
                }

                // Nothing drained means the poll timed out.
                if socket.iter().count() != 0 {
                    sbs = scan()?;
                }
            }
            None => {
                sleep(remaining.min(NO_UDEV_RESCAN_INTERVAL));
                sbs = scan()?;
            }
        }
    }

    if announced.is_some() {
        warn!("all {} devices found after {:.1}s",
              expected_devices(&sbs), start.elapsed().as_secs_f32());
    }

    Ok(sbs)
}

fn get_devices_by_label(
    label: &str,
    opts: &bch_opts,
    use_udev: bool,
) -> anyhow::Result<Vec<(PathBuf, bch_sb_handle)>> {
    let sbs = if use_udev {
        let devs_from_udev = get_bcachefs_devnodes_udev()?;
        if devs_from_udev.is_empty() {
            Vec::new()
        } else {
            read_sbs_matching_label(label, &devs_from_udev, opts, false)?
        }
    } else {
        Vec::new()
    };

    let sbs = if sbs.is_empty() {
        let all_devs = get_all_block_devnodes()?;
        read_sbs_matching_label(label, &all_devs, opts, true)?
    } else {
        sbs
    };

    let mut uuids = sbs.iter()
        .map(|(_, sb)| sb.sb().uuid())
        .collect::<Vec<_>>();
    uuids.sort();
    uuids.dedup();

    match uuids.as_slice() {
        [] => Ok(Vec::new()),
        [uuid] => get_devices_by_uuid(*uuid, opts, use_udev),
        _ => anyhow::bail!("multiple bcachefs filesystems found with label '{}'", label),
    }
}

fn devs_str_sbs_from_device(
    device: &Path,
    opts: &bch_opts,
    use_udev: bool,
    wait: bool,
) -> anyhow::Result<Vec<(PathBuf, bch_sb_handle)>> {
    if let Ok(metadata) = fs::metadata(device) {
        if metadata.is_dir() {
            return Err(anyhow::anyhow!("'{}' is a directory, not a block device", device.display()));
        }
    }

    // Honor explicit user-supplied paths, but warn when a path appears to be
    // a multipath component because that is typically unintended.
    if let Some(mpath_dev) = find_multipath_holder(device) {
        warn_multipath_component(device, &mpath_dev);
    }

    let dev_sb = read_super_silent(device, *opts)?;

    if dev_sb.sb().number_of_devices() == 1 {
        return Ok(vec![(device.to_path_buf(), dev_sb)]);
    }

    let uuid = dev_sb.sb().uuid();
    drop(dev_sb);

    // This is the path a multi-device root actually takes: mount(8) resolves
    // an fstab UUID= to a devnode itself and execs us with a single path, so
    // it never reaches the UUID= branch. Unlike that branch we have already
    // read a superblock, so we know how many members to expect from the first
    // iteration rather than falling back to the built-in timeout.
    search(opts, wait, || get_devices_by_uuid(uuid, opts, use_udev))
}

pub fn parse_uuid_equals(s: &str) -> Result<Option<Uuid>> {
    let Some(("UUID" | "OLD_BLKID_UUID", uuid)) = s.split_once('=') else {
        return Ok(None);
    };
    Ok(Some(Uuid::parse_str(uuid)?))
}

fn parse_label_equals(s: &str) -> Option<&str> {
    let ("LABEL", label) = s.split_once('=')? else {
        return None;
    };
    Some(label)
}

/// Find a filesystem's members, without waiting for any that are absent.
pub fn scan_sbs(device: &String, opts: &bch_opts) -> Result<Vec<(PathBuf, bch_sb_handle)>> {
    scan_sbs_maybe_waiting(device, opts, false)
}

/// The same, but wait for members that have not enumerated yet.
///
/// Only mount wants this. Every other command that resolves a filesystem is
/// being run by someone at a prompt who already knows what is plugged in -
/// `bcachefs device remove` on a dead disk should not sit for
/// missing_dev_timeout before doing the thing it was asked to do.
pub fn scan_sbs_for_mount(device: &String, opts: &bch_opts)
    -> Result<Vec<(PathBuf, bch_sb_handle)>>
{
    scan_sbs_maybe_waiting(device, opts, true)
}

/// Waiting is for the search paths only: naming paths, with or without colons,
/// is a statement about what exists.
fn search<F>(opts: &bch_opts, wait: bool, f: F) -> Result<Vec<(PathBuf, bch_sb_handle)>>
where
    F: Fn() -> Result<Vec<(PathBuf, bch_sb_handle)>>,
{
    if wait { scan_waiting_for_devices(opts, f) } else { f() }
}

fn scan_sbs_maybe_waiting(device: &String, opts: &bch_opts, wait: bool)
    -> Result<Vec<(PathBuf, bch_sb_handle)>>
{
    let udev = opt_get!(opts, mount_trusts_udev) != 0;

    if let Some(uuid) = parse_uuid_equals(device)? {
        return search(opts, wait, || get_devices_by_uuid(uuid, opts, udev));
    }

    if let Some(label) = parse_label_equals(device) {
        return search(opts, wait, || get_devices_by_label(label, opts, udev));
    }

    if device.contains(':') {
        let mut opts = *opts;
        opt_set!(opts, noexcl, 1);
        opt_set!(opts, nochanges, 1);
        opt_set!(opts, no_version_check, 1);

        // If the device string contains ":" we will assume the user knows the
        // entire list. If they supply a single device it could be either the FS
        // only has 1 device or it's only 1 of a number of devices which are
        // part of the FS. This appears to be the case when we get called during
        // fstab mount processing and the fstab specifies a UUID.

        return device.split(':')
            .map(PathBuf::from)
            .map(|path| {
                if let Some(mpath_dev) = find_multipath_holder(path.as_path()) {
                    warn_multipath_component(path.as_path(), &mpath_dev);
                }

                bch_bindgen::sb::io::read_super_opts(path.as_ref(), opts)
                    .map(|sb| (path, sb))
            })
            .collect::<Result<Vec<_>>>()
    }

    devs_str_sbs_from_device(Path::new(device), opts, udev, wait)
}

pub fn joined_device_str(sbs: &[(PathBuf, bch_sb_handle)]) -> OsString {
    sbs.iter()
        .map(|sb| sb.0.clone().into_os_string())
        .collect::<Vec<_>>()
        .join(OsStr::new(":"))
}

pub fn scan_devices(device: &String, opts: &bch_opts) -> Result<OsString> {
    let sbs = scan_sbs(device, opts)?;

    Ok(joined_device_str(&sbs))
}

/// A filesystem opened either online (mounted: talk to the kernel via
/// ioctl/sysfs) or offline (opened in userspace via libbcachefs):
pub enum OpenedFs {
    Online(crate::wrappers::handle::BcachefsHandle),
    Offline(Fs),
}

/// The standard "operate on a filesystem that may or may not be mounted"
/// open, shared by list/set-option/device-add et al: if any of the given
/// paths resolves to a mounted filesystem (mount point, member block
/// device, or UUID), returns a handle to it; otherwise opens offline with
/// the given opts, discovering other members as needed.
///
/// "Couldn't tell" (e.g. a sysfs error on a mounted filesystem) is an
/// error, never silently treated as offline - the offline fallback on a
/// live filesystem is how you corrupt it.
pub fn open_online_or_offline(devs: &[PathBuf], offline_opts: bch_opts)
    -> Result<OpenedFs, BchError>
{
    use crate::wrappers::handle::BcachefsHandle;

    Ok(match BcachefsHandle::open_if_mounted_any(devs)? {
        Some(h) => OpenedFs::Online(h),
        None    => OpenedFs::Offline(open_scan(devs, offline_opts)?),
    })
}

/// One device names a filesystem, so find its members the way mount does.
/// Several were named deliberately, and pass through as-is.
pub fn open_scan(devs: &[PathBuf], fs_opts: bch_opts) -> Result<Fs, BchError> {
    let devs = if devs.len() == 1 {
        let mut dev_str = devs[0].to_string_lossy().into_owned();

        // A bare UUID isn't a path - scan for the filesystem's devices
        // (scan_sbs understands UUID= syntax):
        if Uuid::parse_str(&dev_str).is_ok() {
            dev_str = format!("UUID={}", dev_str);
        }

	let scan_opts = bcachefs_kernel::opts::parse_mount_opts(None, None, true)
            .unwrap_or_default();
        match scan_sbs(&dev_str, &scan_opts) {
            Ok(sbs) if !sbs.is_empty() => sbs.into_iter().map(|(p, _)| p).collect(),
            _ => devs.to_vec(),
        }
    } else {
        devs.to_vec()
    };

    Fs::open(&devs, fs_opts)
}

#[allow(dead_code)]
pub fn bch2_scan_devices(device: *const c_char) -> *mut c_char {
    let device = unsafe { CStr::from_ptr(device) };
    let device = device.to_string_lossy().into_owned();

    // how to initialize to default/empty?
    let opts = bcachefs_kernel::opts::parse_mount_opts(None, None, true).unwrap_or_default();

    let devs = scan_devices(&device, &opts).unwrap_or_else(|e| {
        eprintln!("bcachefs ({}): error reading superblock: {}", device, e);
        std::process::exit(-1);
    });

    CString::new(devs.into_vec()).unwrap().into_raw()
}

/// A udev monitor, and the question "have the missing members turned up?"
///
/// The degraded prompt uses this so it can stop asking when the answer arrives
/// as hardware rather than as a keystroke. Someone who is asked whether to
/// mount without a disk, and responds by plugging the disk in, has answered.
pub struct DeviceWatch {
	socket:	udev::MonitorSocket,
	uuid:	Uuid,
	opts:	bch_opts,
	use_udev: bool,
}

impl DeviceWatch {
	/// `None` when there is nothing to watch for or no way to watch: a
	/// filesystem we cannot name by UUID, or no udev to tell us about
	/// arrivals. Polling for a disk on a timer while a question is on screen
	/// is not worth the code.
	pub fn new(sbs: &[(PathBuf, bch_sb_handle)], opts: &bch_opts, use_udev: bool) -> Option<Self> {
		let uuid = sbs.first()?.1.sb().uuid();
		let socket = block_device_monitor()?;

		Some(DeviceWatch { socket, uuid, opts: *opts, use_udev })
	}

	/// Drain what woke us and look again. True once every member is present.
	///
	/// Rescans rather than trusting the devnode in the event, for the same
	/// reason scan_waiting_for_devices() does: an arriving block device only
	/// means look again, and the scan knows how - including the fallback for
	/// members udev has not tagged yet.
	pub fn every_member_present(&mut self) -> bool {
		if self.socket.iter().count() == 0 {
			return false;
		}

		get_devices_by_uuid(self.uuid, &self.opts, self.use_udev)
			.map(|sbs| have_every_device(&sbs))
			.unwrap_or(false)
	}
}

impl crate::prompt::Watch for DeviceWatch {
	fn raw_fd(&self) -> std::os::fd::RawFd {
		self.socket.as_raw_fd()
	}

	fn moot(&mut self) -> bool {
		self.every_member_present()
	}
}
