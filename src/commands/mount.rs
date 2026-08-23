use std::{
    collections::HashSet,
    ffi::{CStr, CString, OsString},
    io::{stdout, IsTerminal},
    os::fd::{AsFd, AsRawFd, OwnedFd},
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr, str,
};

use anyhow::{ensure, Result};
use bcachefs_kernel::c::{bch_opts, bch_sb_handle};
use bcachefs_kernel::errcode::BchError;
use bcachefs_kernel::{c, opt_get, opt_set, path_to_cstr};
use clap::Parser;
use log::{debug, error, info, warn};
use uuid::Uuid;
use crate::device_scan;
use crate::recovery_display::RecoveryDisplay;
use crate::thread_with_file::{self, StatusDisplay};
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs::DevInfo;

use crate::{
    fs_context::{self, FsContext, Level, Message},
    key::{KeyHandle, Keyring, Passphrase, UnlockPolicy},
    logging,
};

/// Carry the mount's side of the conversation while it's coming up: the
/// filesystem to stderr, stdin back to the filesystem, and recovery progress
/// drawn below both.
///
/// Detached. This thread is inside fsconfig(2) until the mount is done, so
/// there's no point in the sequence where joining would be right - and the
/// kernel marks the channel done on every path out, which ends the relay.
///
/// Whether we draw decides whether we poll at all: the kernel stops logging
/// progress to dmesg as soon as anything reads BCH_IOCTL_RECOVERY_STATUS, on
/// the grounds that whoever read it is showing it. Polling from somewhere with
/// nowhere to draw would take progress out of both places at once - so the
/// display decides, in RecoveryDisplay::new(), and returning None here means
/// leaving it to dmesg.
fn spawn_status_relay(fd: OwnedFd, source: String, devs: Vec<DevInfo>) {
    std::thread::spawn(move || {
        let err = std::io::stderr();

        let mut display = RecoveryDisplay::new(fd.as_fd(), source, devs);

        let ret = thread_with_file::relay(
            fd.as_fd(),
            err.as_fd(),
            display.as_mut().map(|d| d as &mut dyn StatusDisplay),
        );

        if let Err(e) = ret {
            debug!("status channel: {e}");
        }
    });
}

/// What came of an attempt on the fs_context path.
enum Mounted {
    /// Mounted.
    Yes,
    /// The fs_context path can't carry this mount; the caller should use
    /// mount(2). Either the kernel has no fsopen(2), or the mount says
    /// something fsconfig(2) has no room for.
    UseLegacy,
    /// The device is write-protected and a read-write mount wasn't explicitly
    /// asked for. mount(8) retries these read-only rather than failing.
    WriteProtected,
}

/// Show warnings and notices; return the error-level messages so the caller can
/// fold them into its error, where they'll be reported exactly once.
fn report_log(msgs: Vec<Message>) -> Vec<String> {
    let mut errors = Vec::new();

    for m in msgs {
        match m.level {
            Level::Error   => errors.push(m.text),
            Level::Warning => eprintln!("mount: warning: {}", m.text),
            Level::Notice  => info!("{}", m.text),
        }
    }

    errors
}

/// A mount that didn't happen, and both halves of what the kernel said about it.
///
/// The code is the half we can act on. bch2_fs_get_tree() hands back bcachefs's
/// own error codes rather than flattening them to an errno, so
/// insufficient_devices_to_start and device_splitbrain arrive as distinct
/// values instead of two EINVALs that can only be told apart by reading their
/// prose - which is what [`crate::degraded::MountOpts::escalate`] used to have
/// to do.
///
/// The text is the half for the user: whatever the filesystem logged against
/// the context, which is the whole reason for mounting through fs_context.
/// Keeping both means neither has to serve as the other.
#[derive(Debug)]
struct MountError {
    code: BchError,
    text: String,
}

impl MountError {
    /// @what is what we were attempting, @err what the kernel returned, and
    /// @log what it had to say first - which supersedes @err in the text, since
    /// strerror() on a bcachefs errcode is "Unknown error 2300".
    fn new(what: &str, err: std::io::Error, log: Vec<Message>) -> Self {
        let code = BchError::from(err);

        let text = match report_log(log) {
            e if e.is_empty() => format!("{what}: {code}"),
            errors            => format!("{what}: {}", errors.join("; ")),
        };

        MountError { code, text }
    }
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::error::Error for MountError {}

/// Mount via the fs_context API, so that whatever bcachefs has to say about a
/// failure reaches the user instead of being flattened to an errno.
fn mount_fs_context(
    src: &str,
    target: &CStr,
    fstype: &str,
    mountflags: libc::c_ulong,
    data: Option<&str>,
    devs: &[DevInfo],
) -> Result<Mounted, MountError> {
    let fc = match FsContext::open(fstype) {
        Ok(Some(fc)) => fc,
        Ok(None) => {
            debug!("no fs_context mount API, falling back to mount(2)");
            return Ok(Mounted::UseLegacy);
        }
        Err(e) => return Err(MountError::new(&format!("fsopen({fstype})"), e, Vec::new())),
    };

    // The kernel's account of a failure is the entire reason for being here, so
    // it becomes the error rather than being printed alongside one.
    let fail = |fc: &FsContext, what: String, err: std::io::Error| -> MountError {
        MountError::new(&what, err, fc.drain_log())
    };

    // One device per parameter. fsconfig(2) copies a value with
    // strndup_user(_value, 256), which returns -EINVAL rather than truncating,
    // so the whole colon-separated list doesn't fit past ~25 devices - and that
    // -EINVAL arrives with an empty fs_context log, since it happens in the
    // syscall wrapper before the VFS sees the parameter. bch2_fs_parse_param()
    // splits each "source" value on ':' and appends, so there is no ceiling on
    // the list, and a device the kernel refuses is named on its own.
    for dev in src.split(':') {
        // A path that doesn't fit can't be said in this API at all, and
        // PATH_MAX is 4096. That alone still needs mount(2), whose
        // copy_mount_string() allows the full length.
        if dev.len() > fs_context::PARAM_VALUE_MAX {
            debug!(
                "device path is {} bytes, over fsconfig()'s {}; using mount(2)",
                dev.len(),
                fs_context::PARAM_VALUE_MAX
            );
            return Ok(Mounted::UseLegacy);
        }

        fc.set("source", Some(dev))
            .map_err(|e| fail(&fc, format!("source {dev}"), e))?;
    }

    for name in fs_context::sb_flag_params(mountflags) {
        fc.set(name, None)
            .map_err(|e| fail(&fc, format!("option {name}"), e))?;
    }

    // One at a time, so a rejected option is named in the error.
    for opt in data.unwrap_or("").split(',').filter(|o| !o.is_empty()) {
        let (key, value) = match opt.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None         => (opt, None),
        };

        fc.set(key, value)
            .map_err(|e| fail(&fc, format!("option {opt}"), e))?;
    }

    // Before creating the superblock: that's the call that blocks for the whole
    // of recovery, which is what there is to report on.
    //
    // Both outcomes are logged: a working channel with nothing to say looks
    // exactly like a kernel that never offered one.
    match fc.status_fd() {
        Some(fd) => {
            info!("status channel on fd {}", fd.as_raw_fd());
            spawn_status_relay(fd, src.to_string(), devs.to_vec());
        }
        None => info!("no status channel from this kernel; mounting without one"),
    }

    info!("creating superblock");
    if let Err(e) = fc.create() {
        // Write-protected device: retry read-only, as mount(8) does. Say nothing
        // yet - the retry produces the real outcome, and this context's log dies
        // with it.
        if matches!(e.raw_os_error(), Some(libc::EACCES | libc::EROFS))
            && mountflags & libc::MS_RDONLY == 0
        {
            return Ok(Mounted::WriteProtected);
        }

        return Err(fail(&fc, "mount".to_string(), e));
    }

    // A mount can succeed and still have things to say - degraded, recovery
    // notes - so drain the log here too.
    report_log(fc.drain_log());

    let mnt = fc
        .fsmount(fs_context::mount_attrs(mountflags))
        .map_err(|e| fail(&fc, "fsmount".to_string(), e))?;

    fs_context::move_mount(&mnt, target).map_err(|e| {
        MountError::new(&format!("attaching to {}", target.to_string_lossy()), e, Vec::new())
    })?;

    Ok(Mounted::Yes)
}

/// Bring in members that turned up while we were deciding to mount.
///
/// There is a window nothing else covers. scan_waiting_for_devices() drops its
/// udev monitor when it returns, and the udev rule won't act until
/// /sys/fs/bcachefs/<uuid> exists, which is bch2_fs_online() partway through
/// the mount. Everything between is nobody's: too late for one, too early for
/// the other.
///
/// That window contains the degraded prompt, so it can be a minute long - and
/// it is exactly when a slow disk is most likely to finish coming up, since we
/// have just spent missing_dev_timeout waiting for it and given up.
///
/// So look once more, now that the mount is done. This tests state rather than
/// waiting for an event, which is what lets it overlap the udev rule instead of
/// abutting it: by the time this runs the rule is already live, both may try,
/// and __bch2_dev_attach_bdev() turns the loser away with device_already_online
/// rather than doing anything to it.
///
/// Best effort. The filesystem is mounted; failing to pick up a straggler is
/// worth saying out loud but is not a reason to fail the mount.
fn online_late_devices(uuid: Uuid, mounted: &HashSet<u8>, opts: &bch_opts) {
    let use_udev = opt_get!(opts, mount_trusts_udev) != 0;

    let found = match device_scan::get_devices_by_uuid(uuid, opts, use_udev) {
        Ok(found) => found,
        Err(e) => {
            debug!("rescanning for late devices: {e:#}");
            return;
        }
    };

    let late: Vec<&PathBuf> = found
        .iter()
        .filter(|(_, sb)| !mounted.contains(&sb.sb().dev_idx))
        .map(|(path, _)| path)
        .collect();

    if late.is_empty() {
        return;
    }

    let handle = match BcachefsHandle::open(uuid.hyphenated().to_string()) {
        Ok(handle) => handle,
        Err(e) => {
            warn!("{} device(s) turned up after mounting, but opening the \
                   filesystem to bring them online failed: {e}", late.len());
            return;
        }
    };

    for dev in late {
        match handle.disk_online(&path_to_cstr(dev)) {
            Ok(())  => warn!("{} turned up after we mounted; brought it online",
                             dev.display()),
            // Includes the benign case where the udev rule got there first;
            // the ioctl reports device_already_online as a plain EINVAL, so we
            // cannot tell that apart from a real refusal here. The kernel log
            // has the reason either way.
            Err(e) => warn!("{} turned up after we mounted, but bringing it \
                             online failed ({e}); it may already be online",
                            dev.display()),
        }
    }
}

fn mount_inner(
    src: OsString,
    target: &std::path::Path,
    fstype: &str,
    mountflags: libc::c_ulong,
    data: Option<String>,
    devs: Vec<DevInfo>,
) -> Result<(), MountError> {
    // Reconfiguring an existing mount is fspick(2) + FSCONFIG_CMD_RECONFIGURE,
    // which fs_context doesn't implement; leave remount on mount(2).
    //
    // A device path that isn't UTF-8 also goes the old way rather than being
    // lossily mangled into one: mount(2) takes arbitrary bytes.
    if mountflags & libc::MS_REMOUNT == 0 {
        if let Some(src_str) = src.to_str() {
            let c_target = path_to_cstr(target);
            let mut flags = mountflags;

            loop {
                info!("mounting filesystem");
                match mount_fs_context(src_str, &c_target, fstype, flags, data.as_deref(), &devs)? {
                    Mounted::Yes      => return Ok(()),
                    Mounted::UseLegacy => break,
                    Mounted::WriteProtected => {
                        println!("mount: device write-protected, mounting read-only");
                        flags |= libc::MS_RDONLY;
                    }
                }
            }
        }
    }

    mount_legacy(src, target, fstype, mountflags, data)
}

fn mount_legacy(
    src: OsString,
    target: &std::path::Path,
    fstype: &str,
    mut mountflags: libc::c_ulong,
    data: Option<String>,
) -> Result<(), MountError> {
    // An interior NUL can't be expressed in a C string at all. Nothing has been
    // attempted at this point, so the errno is ours rather than the kernel's.
    let bad_arg = |what: &str| MountError {
        code: BchError::from_raw(libc::EINVAL),
        text: format!("{what} contains a NUL byte"),
    };

    // bind the CStrings to keep them alive
    let c_src = CString::new(src.clone().into_vec()).map_err(|_| bad_arg("device path"))?;
    let c_target = path_to_cstr(target);
    let data = data.map(CString::new).transpose().map_err(|_| bad_arg("mount options"))?;
    let fstype = CString::new(fstype).map_err(|_| bad_arg("filesystem type"))?;

    // convert to pointers for ffi
    let c_src = c_src.as_ptr();
    let c_target = c_target.as_ptr();
    let data_ptr = data.as_ref().map_or(ptr::null(), |data| data.as_ptr().cast());
    let fstype = fstype.as_ptr();

    let mut ret;
    loop {
        ret = {
            info!("mounting filesystem");
            // REQUIRES: CAP_SYS_ADMIN
            unsafe { libc::mount(c_src, c_target, fstype, mountflags, data_ptr) }
        };

        let err = errno::errno().0;

        if ret == 0
            || (err != libc::EACCES && err != libc::EROFS)
            || (mountflags & libc::MS_RDONLY) != 0
        {
            break;
        }

        println!("mount: device write-protected, mounting read-only");
        mountflags |= libc::MS_RDONLY;
    }

    drop(data);

    if ret != 0 {
        let err = errno::errno();

        // mount(2) has nowhere to put an explanation, so an errno is all there
        // is to report - which is the reason mount_fs_context() exists. EBUSY
        // is worth translating: what it means here isn't obvious from the word.
        Err(MountError {
            code: BchError::from_raw(err.0),
            text: if err.0 == libc::EBUSY {
                format!("{}: {src:?} already mounted or mount point busy",
                        target.to_string_lossy())
            } else {
                format!("{src:?}: {}", crate::ErrnoError(err))
            },
        })
    } else {
        Ok(())
    }
}

/// A comma-separated mount option string split into its consumers.
///
/// The same option vocabulary feeds three places - the mount(2) syscall
/// (`flags`), the FUSE mount (`fuse_options`), and the filesystem itself
/// (`fs_opts`, handed to parse_mount_opts later) - so it's tabulated once in
/// [`parse_mountflag_options`] rather than re-derived per caller.
#[derive(Default)]
pub(crate) struct ParsedMountOptions {
    /// Filesystem-specific options: everything not consumed as a kernel flag.
    pub fs_opts:      Option<String>,
    /// Kernel mount flags for mount(2).
    pub flags:        libc::c_ulong,
    /// `flags` expressed as fuser options, for the FUSE path. Flags with no
    /// fuser equivalent are omitted here but still apply via `flags`.
    #[cfg(feature = "fuse")]
    pub fuse_options: Vec<fuser::MountOption>,
}

/// Parse a comma-separated mount option string, splitting kernel mount flags
/// (and their fuser equivalents) from filesystem-specific options.
pub(crate) fn parse_mountflag_options(options: impl AsRef<str>) -> ParsedMountOptions {
    debug!("parsing mount options: {}", options.as_ref());

    let mut parsed = ParsedMountOptions::default();
    let mut fs_opts: Vec<&str> = Vec::new();

    // A kernel flag, optionally paired with its fuser option. The fuser arm is
    // only referenced under the `fuse` feature, so its tokens must live inside
    // the cfg - hence the macro rather than a plain match value.
    macro_rules! flag {
        ($ms:expr) => {{ parsed.flags |= $ms; }};
        ($ms:expr, $fuse:expr) => {{
            parsed.flags |= $ms;
            #[cfg(feature = "fuse")]
            parsed.fuse_options.push($fuse);
        }};
    }

    for opt in options.as_ref().split(',') {
        match opt {
            "dirsync"     => flag!(libc::MS_DIRSYNC, fuser::MountOption::DirSync),
            "lazytime"    => flag!(fs_context::MS_LAZYTIME),
            "mand"        => flag!(libc::MS_MANDLOCK),
            "noatime"     => flag!(libc::MS_NOATIME, fuser::MountOption::NoAtime),
            "nodev"       => flag!(libc::MS_NODEV, fuser::MountOption::NoDev),
            "nodiratime"  => flag!(libc::MS_NODIRATIME),
            "noexec"      => flag!(libc::MS_NOEXEC, fuser::MountOption::NoExec),
            "nosuid"      => flag!(libc::MS_NOSUID, fuser::MountOption::NoSuid),
            "relatime"    => flag!(libc::MS_RELATIME),
            "remount"     => flag!(libc::MS_REMOUNT),
            "ro"          => flag!(libc::MS_RDONLY, fuser::MountOption::RO),
            "rw" | ""     => {}
            "strictatime" => flag!(libc::MS_STRICTATIME),
            "sync"        => flag!(libc::MS_SYNCHRONOUS, fuser::MountOption::Sync),
            // Userspace-only fstab options - not passed to the kernel:
            "auto" | "noauto" | "nofail" | "_netdev"
            | "user" | "nouser" | "users" | "group" | "owner" => {}
            o if o.starts_with("x-") || o.starts_with("comment=") => {}
            o => fs_opts.push(o),
        }
    }

    parsed.fs_opts = (!fs_opts.is_empty()).then(|| fs_opts.join(","));
    parsed
}

#[cfg(test)]
mod tests {
    use super::{is_splitbrain, parse_mountflag_options};
    use bcachefs_kernel::c;
    use bcachefs_kernel::errcode::BchError;

    /// The prompt only happens if this recognises the error, and a failure to
    /// recognise it is silent - mount just refuses, exactly as it would with
    /// no split brain at all. So pin the two shapes it has to survive:
    /// straight through `?`, and behind a context someone adds later.
    #[test]
    fn splitbrain_is_recognised_through_anyhow() {
        let sb = || BchError::from_errcode(c::bch_errcode::BCH_ERR_device_splitbrain);

        assert!(is_splitbrain(&anyhow::Error::from(sb())));
        assert!(is_splitbrain(&anyhow::Error::from(sb()).context("scanning for devices")));

        let other = BchError::from_errcode(c::bch_errcode::BCH_ERR_device_has_been_removed);
        assert!(!is_splitbrain(&anyhow::Error::from(other)));
        assert!(!is_splitbrain(&anyhow::anyhow!("not a bcachefs error at all")));
    }

    #[test]
    fn parse_mountflag_options_splits_kernel_and_fs_options() {
        let p = parse_mountflag_options("ro,noexec,metadata_replicas=2,norecovery");

        assert_eq!(p.fs_opts.as_deref(), Some("metadata_replicas=2,norecovery"));
        assert_ne!(p.flags & libc::MS_RDONLY, 0);
        assert_ne!(p.flags & libc::MS_NOEXEC, 0);
    }

    #[test]
    fn parse_mountflag_options_drops_userspace_fstab_options() {
        let p = parse_mountflag_options("nofail,_netdev,x-systemd.device-timeout=5");

        assert_eq!(p.fs_opts, None);
        assert_eq!(p.flags, 0);
    }
}

/// If a user explicitly specifies `unlock_policy` or `passphrase_file` then use
/// that without falling back to other mechanisms. If these options are not
/// used, then search for the key or ask for it.
fn handle_unlock(cli: &Cli, sb: &bch_sb_handle) -> Result<KeyHandle> {
    if let Some(policy) = cli.unlock_policy.as_ref() {
        return policy.apply(sb);
    }

    if let Some(path) = cli.passphrase_file.as_deref() {
        let passphrase_correct = Passphrase::read_from_file(path)?
            .check(sb)
            .ok_or_else(|| anyhow::anyhow!("incorrect passphrase"))?;
        return KeyHandle::new(&passphrase_correct, Keyring::User);
    }

    let uuid = sb.sb().uuid();
    if let Ok(handle) = KeyHandle::new_from_search(&uuid) {
        return Ok(handle);
    }

    let passphrase_correct = Passphrase::ask_and_check(sb)?;
    KeyHandle::new(&passphrase_correct, Keyring::User)
}

/// anyhow's downcast_ref searches the context chain, so this survives someone
/// wrapping the scan path in a `.context()`. Worth a test rather than an
/// assumption: failing to recognise it is silent - the mount just refuses,
/// exactly as it does when there is no split brain.
fn is_splitbrain(err: &anyhow::Error) -> bool {
    err.downcast_ref::<BchError>()
        .is_some_and(|e| e.matches(c::bch_errcode::BCH_ERR_device_splitbrain))
}

/// Scan for the filesystem's devices, and if its history has forked, ask.
///
/// The check runs deep in the scan - it has to, before bch2_sbs_filter_dead()
/// frees the divergent superblocks - but the *decision* cannot, because the
/// scan re-runs on every device arrival while waiting for members: a question
/// there would be asked repeatedly and the answer could not persist. So the
/// scan reports and refuses, and the question is put here, once.
///
/// Consenting leaves the diverged devices out of the mount, which is the
/// missing-device case, so it answers the degraded question too.
fn scan_or_ask_splitbrain(dev: &String, opts: &mut bch_opts)
    -> Result<Vec<(std::path::PathBuf, bch_sb_handle)>>
{
    let err = match device_scan::scan_sbs_for_mount(dev, opts) {
        Ok(sbs) => return Ok(sbs),
        Err(e) => e,
    };

    if !is_splitbrain(&err) {
        return Err(err);
    }

    // The report is already out - the scan logged it on the way to failing.
    // Re-scanning with the check off gives us the surviving side, which is
    // both what to name in the question and what to mount if they say yes.
    // Not the waiting scan: the one above already waited, and everything it
    // is about to find is what that wait turned up.
    opt_set!(opts, no_splitbrain_check, 1);
    let sbs = device_scan::scan_sbs(dev, opts)?;

    let Some((_, sb)) = sbs.first() else {
        return Err(err);
    };

    if !crate::splitbrain::ask(sb)? {
        return Err(err);
    }

    opt_set!(opts, degraded, c::bch_degraded_actions::BCH_DEGRADED_yes as u8);
    Ok(sbs)
}

fn cmd_mount_inner(cli: &Cli) -> Result<()> {
    if cli.no_mtab {
        debug!("ignoring -n/--no-mtab; mount.bcachefs does not update /etc/mtab");
    }
    if cli.sloppy {
        debug!("ignoring -s/--sloppy; bcachefs already ignores unrecognized options");
    }

    let parsed = parse_mountflag_options(&cli.options);
    let mut opts = bcachefs_kernel::opts::parse_mount_opts(None, parsed.fs_opts.as_deref(), true)
        .unwrap_or_default();

    let sbs = scan_or_ask_splitbrain(&cli.dev, &mut opts)?;

    ensure!(!sbs.is_empty(), "No device(s) to mount specified");

    let devices = device_scan::joined_device_str(&sbs);

    let first_sb = &sbs[0].1;
    if unsafe { bch_bindgen::c::bch2_sb_is_encrypted(first_sb.sb) } {
        handle_unlock(cli, first_sb)?;
    }

    if let Some(mountpoint) = cli.mountpoint.as_deref() {
        if cli.fake {
            info!(
                "fake mount (-f/--fake): skipping the mount syscall for {}",
                mountpoint.to_string_lossy()
            );
            return Ok(());
        }

        // The scan waited for the members it could; if some never turned up,
        // the filesystem's degraded action decides what happens next, and
        // `ask` is ours to answer before the kernel sees it.
        //
        // After the -f check, and inside this branch, on purpose: asking
        // someone whether to mount without a device is only worth their time
        // if we are going to mount. -f exists to not do the thing, and an
        // invocation with no mountpoint isn't mounting either.
        let fs_opts = crate::degraded::resolve_mount_opts(&sbs, &opts, parsed.fs_opts)?;

        // Answering `r` at the degraded prompt means read-only, and it has to
        // be read-only to the VFS - not just to bcachefs - or /proc/mounts
        // says rw and the user has only our word for it.
        let flags = parsed.flags | fs_opts.flags;

        // What we're about to mount with, for the rescan afterwards. `short` is
        // false when nothing is missing, which is the common case and skips the
        // rescan entirely.
        let uuid = sbs[0].1.sb().uuid();
        let mounted = device_scan::present_devices(&sbs);
        let short = mounted.len() < device_scan::expected_devices(&sbs);

        // For the mount-time progress display, which needs to say how much
        // redundancy is left - and can only ask the superblocks, because the
        // filesystem isn't up yet. Taken before the scan is dropped.
        let devinfo = device_scan::devices_from_superblocks(&sbs);

        drop(sbs);

        info!(
            "mounting with params: device: {:?}, target: {}, options: {}",
            devices,
            mountpoint.to_string_lossy(),
            &cli.options
        );

        let mounted_ret = match mount_inner(
            devices.clone(),
            mountpoint,
            "bcachefs",
            flags,
            fs_opts.fs_opts.clone(),
            devinfo.clone(),
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Refused. If we're the ones who asked the degraded question,
                // and the user allowed only the case where everything is
                // still readable, there is a second question worth putting to
                // them rather than leaving them to reboot into the same
                // refusal. escalate() gives back None whenever there is
                // nothing to ask - a different failure, we never asked, or
                // they declined - and then the original error stands.
                match fs_opts.escalate(&e.code)? {
                    Some(retry) =>
                        mount_inner(devices, mountpoint, "bcachefs", flags, Some(retry), devinfo),
                    None => Err(e),
                }
            }
        };

        // Mounted, but we left members behind: one of them may have shown up
        // while we were asking about it.
        if mounted_ret.is_ok() && short {
            online_late_devices(uuid, &mounted, &opts);
        }

        Ok(mounted_ret?)
    } else {
        info!(
            "would mount with params: device: {:?}, options: {}",
            devices, &cli.options
        );

        Ok(())
    }
}

/// Mount a bcachefs filesystem by its UUID or label.
#[derive(Parser, Debug)]
#[command(author, version, about,
    long_about = "`mount -t bcachefs` invokes the installed mount.bcachefs helper; \
this is the same mount path exposed as `bcachefs mount`.\n\n\
Mounts a bcachefs filesystem. Devices are discovered automatically \
by scanning for the filesystem UUID or label---unlike btrfs, this is handled \
entirely in userspace.\n\n\
Use OLD_BLKID_UUID=<uuid> in fstab entries when systemd consumes \
UUID=<uuid> before the bcachefs mount helper can scan all members.\n\n\
If the filesystem is encrypted, the passphrase will be looked up in \
the kernel keyring first; if not found, the user is prompted \
interactively (or reads from stdin if not a terminal). Use -k or --passphrase-file \
to specify alternative unlock methods.")]
pub struct Cli {
    /// Path to passphrase file
    ///
    /// This can be used to optionally specify a file to read the passphrase
    /// from. An explictly specified key_location/unlock_policy overrides this
    /// argument.
    #[arg(long)]
    passphrase_file: Option<PathBuf>,

    /// Passphrase policy to use in case of an encrypted filesystem. If not
    /// specified, the password will be searched for in the keyring. If not
    /// found, the password will be prompted or read from stdin, depending on
    /// whether the stdin is connected to a terminal or not.
    #[arg(short = 'k', long = "key_location", value_enum)]
    unlock_policy: Option<UnlockPolicy>,

    /// Device, UUID=\<UUID\>, OLD_BLKID_UUID=\<UUID\> (fstab), or LABEL=\<label\>
    dev: String,

    /// Where the filesystem should be mounted. If not set, then the filesystem
    /// won't actually be mounted. But all steps preceeding mounting the
    /// filesystem (e.g. asking for passphrase) will still be performed.
    mountpoint: Option<PathBuf>,

    /// Mount options
    #[arg(short, default_value = "")]
    options: String,

    /// Do not update /etc/mtab; accepted for mount(8) compatibility
    #[arg(short = 'n', long = "no-mtab")]
    no_mtab: bool,

    /// Fake mount: do everything except the mount syscall (mount(8) -f)
    #[arg(short = 'f', long)]
    fake: bool,

    /// Ignore unrecognized mount options instead of failing (mount(8) -s).
    /// bcachefs already ignores unknown options, so this is accepted as a no-op.
    #[arg(short = 's', long)]
    sloppy: bool,

    #[arg(short = 't', long = "type", default_value = "")]
    fs_type: String,

    // FIXME: would be nicer to have `--color[=WHEN]` like diff or ls?
    /// Force color on/off. Autodetect tty is used to define default:
    #[arg(short, long, action = clap::ArgAction::Set, default_value_t=stdout().is_terminal())]
    colorize: bool,

    /// Verbose mode
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

struct ModuleCheck {
    loaded:         bool,
    modprobe_error: Option<String>,
}

fn check_bcachefs_module() -> ModuleCheck {
    let path = Path::new("/sys/module/bcachefs");
    if path.exists() {
        return ModuleCheck { loaded: true, modprobe_error: None };
    }

    let modprobe_error = match std::process::Command::new("modprobe").arg("bcachefs").status() {
        Ok(s) if s.success() => None,
        Ok(_)  => Some("modprobe bcachefs exited unsuccessfully".to_string()),
        Err(e) => Some(format!("could not run modprobe bcachefs: {e}")),
    };

    ModuleCheck { loaded: path.exists(), modprobe_error }
}

fn mount(cli: Cli) -> std::process::ExitCode {
    let module = check_bcachefs_module();

    if cli.fs_type == "bcachefs.fuse" {
        if cli.fake {
            info!("fake mount (-f/--fake): skipping FUSE mount");
            return std::process::ExitCode::SUCCESS;
        }
        #[cfg(feature = "fuse")]
        {
            let fuse_cli = super::fusemount::Cli {
                options: if cli.options.is_empty() { None } else { Some(cli.options.clone()) },
                foreground: false,
                device: cli.dev.clone(),
                mountpoint: cli.mountpoint.as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            };
            return match super::fusemount::cmd_fusemount(fuse_cli) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    error!("FUSE mount failed: {e}");
                    std::process::ExitCode::FAILURE
                }
            };
        }
        #[cfg(not(feature = "fuse"))]
        {
            error!("FUSE support not compiled in (build with the 'fuse' feature)");
            return std::process::ExitCode::FAILURE;
        }
    }

    // TODO: centralize this on the top level CLI
    logging::setup(cli.verbose, cli.colorize);

    match cmd_mount_inner(&cli) {
        Ok(_)   => std::process::ExitCode::SUCCESS,
        Err(e)   => {
            error!("Mount failed for {}: {e}", cli.dev);
            if !module.loaded {
                error!("bcachefs module not loaded?");
                if let Some(e) = module.modprobe_error {
                    error!("{e}");
                }
            }
            std::process::ExitCode::FAILURE
        }
    }
}

pub static CMD: super::CmdDef = {
    fn __cmd() -> clap::Command { <Cli as clap::CommandFactory>::command() }
    fn __run(argv: Vec<String>) -> std::process::ExitCode {
        mount(Cli::parse_from(argv))
    }
    super::CmdDef {
        name: "mount", about: "Mount a filesystem", aliases: &[],
        kind: super::CmdKind::Typed { cmd: __cmd, run: __run },
    }
};
