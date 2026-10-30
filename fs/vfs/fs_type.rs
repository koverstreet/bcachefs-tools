// SPDX-License-Identifier: GPL-2.0

//! bcachefs, the filesystem type: what mounting, and reconfiguring, a
//! filesystem is. The type is util/fs_type.rs's FsType, over the safe
//! fs_context and superblock interface of util/fs_context.rs and
//! util/super_block.rs. This file is bcachefs's policy on top of it.
//!
//! A mount (get_tree):
//! - The devices it names, to the running filesystem they belong to if they
//!   all belong to one, and that filesystem's superblock if it has one: a
//!   mount of a filesystem already mounted.
//! - Otherwise, open and start the filesystem - recovery - while no
//!   superblock exists, so nothing waits on one while recovery runs (sync()
//!   waits on every superblock not yet born). Then create its superblock,
//!   owning it, registered under its first online member's device; then
//!   fill it.
//!
//! The superblock owns the filesystem from create_super() on, and kill_sb
//! exits it. Before that the mount does, as an OpenFs.
//!
//! A mount's errors are returned raw - errno(), the errcode negated - not as
//! their errno class. mount.bcachefs has to tell them apart - whether to ask
//! the user about a degraded mount, whether a refusal is worth escalating -
//! and an errcode is BCH_ERR_START (2048) plus its number, far below
//! MAX_ERRNO, so it's still an error to anyone who doesn't know ours. So are
//! reconfigure's, as C's were; a parameter's are classed.

use core::ffi::{c_int, CStr};
use core::ptr::NonNull;
use core::sync::atomic::AtomicUsize;

use crate::c;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::fs::OpenFs;
use crate::opt_id;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::bitmap::AtomicBitmap;
use crate::util::fs_context::{FsContext, FsParam, ParamValue};
use crate::util::fs_type::{FsType, FsTypeTable, SuperData};
use crate::util::printbuf::Printbuf;
use crate::util::super_block::{ReconfSuper, Root};
use crate::util::vstructs::plain_bytes_mut;
use cstruct_macros::{c_export, c_extern};
use kernel::sync::aref::ARef;
use pin_init::{init, Init};

use super::inode::InodeInfo;
use super::status_fd::StatusFile;
use super::vfs_inode::VfsInode;

c_extern! {
    fn bch2_dev_to_fs(dev: c::dev_t) -> *mut c::bch_fs;
}

pub(crate) struct Bcachefs;

// SAFETY: from_raw() takes back what into_raw() gave up
unsafe impl SuperData for OpenFs {
    type Target = Fs;

    fn into_raw(self) -> NonNull<Fs> {
        OpenFs::into_raw(self)
    }

    unsafe fn from_raw(p: NonNull<Fs>) -> Self {
        unsafe { OpenFs::from_raw(p) }
    }
}

/// A mount's or reconfigure's parameters, as parse_param has them so far:
/// fc->fs_private, from new_context() until the context's free.
pub(crate) struct MountParams {
    opts:        c::bch_opts,
    /// The options that can't be parsed until the filesystem is open: open()
    /// parses them, after OpenFs::open().
    parse_later: Printbuf,
    /// The devices, from however many "source" parameters.
    devs:        KVVec<DevPath>,
    /// "status_fd"'s channel, for as long as the filesystem might still print
    /// there: see open() and status_fd.rs.
    status:      Option<StatusFile>,
    user_key:    Option<UserKey>,
}

/// A device's path, NUL-terminated.
struct DevPath(KVVec<u8>);

impl DevPath {
    fn new(path: &[u8]) -> Result<Self, BchError> {
        let mut v = KVVec::with_capacity(path.len() + 1, GFP_KERNEL)?;
        v.extend_from_slice(path, GFP_KERNEL)?;
        v.push(0, GFP_KERNEL)?;
        Ok(DevPath(v))
    }

    fn as_cstr(&self) -> &CStr {
        CStr::from_bytes_with_nul(&self.0).expect("a path from a C string, NUL-terminated once")
    }
}

/// "user_key"'s key, wiped when it's dropped, however the mount went: made in
/// place, so no copy of it is left behind.
struct UserKey(c::bch_key);

impl Drop for UserKey {
    fn drop(&mut self) {
        wipe(plain_bytes_mut(&mut self.0));
    }
}

/// A secret, gone: volatile writes, which the compiler can't drop as dead
/// before the memory's freed.
fn wipe(b: &mut [u8]) {
    for x in b {
        unsafe { core::ptr::write_volatile(x, 0) };
    }
}

/// @opts' defined options, as set by the mount: what show_options lists.
fn set_mount_opts(fs: &Fs, opts: &c::bch_opts) {
    let mount_opts = &fs.mount_opts.d;
    // SAFETY: unsigned longs, which C changes with set_bit()
    let words = unsafe {
        core::slice::from_raw_parts(mount_opts.as_ptr().cast::<AtomicUsize>(), mount_opts.len())
    };
    let bits = AtomicBitmap::from_words(words);

    for id in (0..crate::opts::NR).filter(|&id| opts.defined(c::bch_opt_id(id as u32))) {
        bits.set(id);
    }
}

/// The running filesystem @path is a device of, if there is one: unpinned,
/// for comparing with the superblocks' - which pin theirs.
fn dev_to_fs(path: &CStr) -> Option<NonNull<Fs>> {
    let mut dev: c::dev_t = 0;
    if unsafe { c::lookup_bdev(path.as_ptr().cast(), &mut dev) } != 0 {
        return None;
    }

    let fs = NonNull::new(unsafe { bch2_dev_to_fs(dev) })?;
    unsafe { c::closure_put(&raw mut (*fs.as_ptr()).cl) };
    Some(fs.cast())
}

/// The running filesystem every device in @p is in, if they're all in one:
/// dev_to_fs()'s, unpinned.
fn devs_fs(p: &MountParams) -> Option<NonNull<Fs>> {
    let mut fs = None;
    for d in p.devs.iter() {
        let dev_fs = dev_to_fs(d.as_cstr())?;
        if fs.is_some_and(|fs| fs != dev_fs) {
            return None;
        }
        fs = Some(dev_fs);
    }
    fs
}

const ROOT_SUBVOL_INUM: c::subvol_inum = c::subvol_inum {
    subvol: c::BCACHEFS_ROOT_SUBVOL as u64,
    inum:   c::BCACHEFS_ROOT_INO as u64,
};

impl Bcachefs {
    /// A mount of a filesystem already mounted: the one every device named is
    /// in, if they're all in one and it has a superblock.
    fn mounted(fc: &mut FsContext<Self>) -> Result<Option<Root<Self>>, BchError> {
        let Some(fs) = devs_fs(fc.ctx()) else { return Ok(None) };
        let Some(sb) = fc.find_super(fs.as_ptr())? else { return Ok(None) };

        // @fs was unpinned until a superblock pinned it: in between it may
        // have been stopped, and another filesystem made at its address. The
        // one found can't go now, so the devices are asked again.
        if devs_fs(fc.ctx()) != Some(NonNull::from(sb.sb().data())) {
            return Ok(None);
        }

        // Don't summarily change the RO/RW state
        if fc.rdonly() != sb.sb().rdonly() {
            return Err(c::EBUSY.into());
        }
        Ok(Some(sb.into_root()))
    }

    /// Open and start the filesystem, no superblock existing yet.
    fn open(fc: &mut FsContext<Self>) -> Result<OpenFs, BchError> {
        let rdonly = fc.rdonly();
        let p = fc.ctx();
        let mut opts = p.opts;

        opts.set_read_only(rdonly);
        opts.set_nostart(true);

        let fs = OpenFs::open(p.devs.iter().map(DevPath::as_cstr), &mut opts,
                              p.user_key.as_ref().map(|k| &k.0))?;

        if opts.defined(opt_id::Opt_discard) {
            fs.set_flag(c::bch_fs_flags::BCH_FS_discard_mount_opt_set);
        }
        set_mount_opts(&fs, &opts);

        // Some options can't be parsed until after the fs is started:
        let mut later: c::bch_opts = Default::default();
        crate::opts::parse_mount_opts_into(Some(&fs), &mut later, None, p.parse_later.as_bytes(), false)?;
        fs.opts_writer(&mut fs.sb_lock()).apply(&later);
        set_mount_opts(&fs, &later);

        // The status channel is scoped to recovery: c->stdio mustn't outlive
        // the reference free drops on the file it points into, and the
        // channel's filesystem mustn't outlive @fs, which an error tears down.
        // stdio_user_only describes that redirect, so it dies with it: an
        // online fsck later installs its own c->stdio.
        let attached = p.status.as_ref().map(|s| s.attach(&fs));
        let ret = fs.start();
        drop(attached);
        ret?;

        Ok(fs)
    }

    /// Its superblock, made and filled: the mount's root.
    fn new_super(fc: &mut FsContext<Self>, fs: OpenFs) -> Result<Root<Self>, BchError> {
        // We might be doing a RO mount because other options required it, or
        // we have no alloc info and it's a small image with no room to
        // regenerate it
        if fs.opts().read_only.get() != 0 {
            fc.set_sb_flags(c::SB_RDONLY as u32);
        }

        // XXX: create an anonymous device for multi device filesystems
        let Some(bdev) = fs.online_members().next().and_then(|ca| NonNull::new(ca.disk_sb.bdev)) else {
            crate::bch_err!(&*fs, "mounting: no online device");
            return Err(c::ENODEV.into());
        };

        let fs_ptr = fs.as_ptr();
        // SAFETY: a member's, which the filesystem holds open until kill_sb
        // exits it - unless that member's removed while it's mounted, which
        // leaves s_bdev stale, as C's did: the XXX above
        let mut sb = unsafe { fc.create_super(fs, bdev) }?;
        // SAFETY: the superblock's now, live until kill_sb
        let fs = unsafe { Fs::borrow_raw(fs_ptr) };

        let filled = (|| {
            sb.set_blocksize(fs.opts().block_size.get() as u32);
            sb.set_maxbytes(c::MAX_LFS_FILESIZE);
            sb.set_ops(&super::super_block::SUPER_OPS);
            sb.set_export_ops(&super::export::EXPORT_OPS);
            #[cfg(CONFIG_BCACHEFS_QUOTA)]
            sb.set_quotactl(&crate::quota::QUOTACTL_OPS,
                            c::QTYPE_MASK_USR | c::QTYPE_MASK_GRP | c::QTYPE_MASK_PRJ);
            // SAFETY: a static table of static handlers, NULL-terminated
            unsafe { sb.set_xattr_handlers(&crate::xattr::bch2_xattr_handlers) };
            sb.set_magic(c::BCACHEFS_STATFS_MAGIC as _);

            let units = fs.sb.time_units_per_sec.get() as i64;
            sb.set_time(fs.sb.nsec_per_time_unit.get(), i64::MIN / units + 1, i64::MAX / units);
            sb.set_uuid(&fs.sb.user_uuid.each_ref().map(|b| b.get()));

            if fs.sb.multi_device.get() {
                sb.set_sysfs_name_uuid();
            } else {
                sb.set_sysfs_name(fs.name());
            }

            sb.set_shrinker_seeks(0);
            unsafe { (*fs_ptr).vfs_sb = sb.sb().as_ptr() };
            sb.set_id(fs.name());

            sb.setup_bdi(crate::alloc::capacity::fs_ra_pages(fs) as _)?;

            unsafe { (*fs_ptr).dev = sb.sb().dev() };

            if fs.opts().acl.get() != 0 {
                sb.set_flags(c::SB_POSIXACL as _);
            }

            // Not generic_set_sb_d_ops()'s generic_ci_dentry_ops: a directory's
            // dentries get casefold ops when its inode is attached
            // (Dentry::set_casefold_ops()), keeping DCACHE_OP_COMPARE off
            // directories that aren't casefolded
            #[cfg(CONFIG_UNICODE)]
            if fs.casefold_enabled().is_ok() {
                // SAFETY: the filesystem's, unloaded at its exit: after
                // kill_sb's generic_shutdown_super()
                unsafe { sb.set_encoding(fs.cf_encoding) };
            }

            let root = InodeInfo::get_warn(fs, ROOT_SUBVOL_INUM, c"bch2_fs_get_tree")
                .inspect_err(|e| crate::bch_err!(fs, "mounting: error getting root inode: {}", e.msg()))?;

            Ok(sb.fill(ARef::<VfsInode>::from(root.vfs()))
                .inspect_err(|_| crate::bch_err!(fs, "error mounting: error allocating root dentry"))?)
        })();

        if let Err(e) = filled {
            fs.stop();
            return Err(e);
        }

        let root = sb.into_root();

        // The root has no attach site of its own: its ops from its own inode
        #[cfg(CONFIG_UNICODE)]
        {
            // SAFETY: the root's dentry, which @root holds a reference on
            let d = unsafe { super::dentry::Dentry::borrow_raw(root.as_ptr()) };
            d.set_casefold_ops(d.inode().vfs());
        }

        Ok(root)
    }
}

impl FsType for Bcachefs {
    const NAME: &'static CStr = c"bcachefs";
    const FLAGS: c_int = (c::FS_REQUIRES_DEV | c::FS_ALLOW_IDMAP | c::FS_LBS) as c_int;

    type Data = OpenFs;
    type Context = MountParams;
    type Error = BchError;

    /// Raw, not classed: see the file comment.
    fn errno(e: &BchError) -> c_int {
        -e.raw()
    }

    fn new_context() -> impl Init<MountParams, BchError> {
        init!(MountParams {
            opts        <- pin_init::init_zeroed(),
            parse_later: Printbuf::new(),
            devs:        KVVec::new(),
            status:      None,
            user_key:    None,
        }? BchError)
    }

    fn parse_param(fc: &mut FsContext<Self>, param: &mut FsParam) -> Result<c_int, BchError> {
        let (key, val) = param.key_value();
        let key_str = key.to_str().unwrap_or("?");

        // Settled once, for every parameter: a value we can read as a string,
        // or a flag with none. A filename or file read as a string is strlen()
        // over an object with no reason to hold a NUL.
        let val = match val {
            ParamValue::Str(s) => Some(s),
            ParamValue::Flag => None,
            ParamValue::Other => return Err(fc.invalf(format_args!("{}: expected a string value", key_str))),
        };

        match key.to_bytes() {
            // The devices to mount: ours, not the VFS's, because fsconfig(2)
            // copies a value with strndup_user(_value, 256) - a long list
            // comes a few at a time, and vfs_parse_fs_param_source() refuses
            // the second ("Multiple sources"). Legacy mount(2) gives one
            // colon-separated value. Splitting each and appending covers both.
            b"source" => {
                let Some(s) = val else {
                    return Err(fc.invalf(format_args!("source: no device given")));
                };

                for path in s.split(|&b| b == b':') {
                    if path.is_empty() {
                        return Err(fc.invalf(format_args!("empty device name in source \"{}\"",
                                                          core::str::from_utf8(s).unwrap_or("?"))));
                    }
                    let path = DevPath::new(path)?;
                    fc.ctx().devs.push(path, GFP_KERNEL)?;
                }

                source_append(fc, s)?;
                Ok(0)
            }

            // Not a mount option: mount.bcachefs's channel to the filesystem
            // coming up. The fd number goes back as fsconfig(2)'s return
            // value, which vfs_parse_fs_param() and vfs_fsconfig_locked()
            // return verbatim.
            b"status_fd" => {
                if fc.ctx().status.is_some() {
                    return Err(fc.invalf(format_args!("status_fd requested twice")));
                }
                let (fd, file) = StatusFile::open()?;
                fc.ctx().status = Some(file);
                Ok(fd)
            }

            // The passphrase-derived key that unwraps the superblock's, for
            // callers that already have it: a parameter, not a mount option,
            // so it never reaches anyone's ps output.
            b"user_key" => {
                let Some(s) = val else {
                    return Err(fc.invalf(format_args!("user_key: no key given")));
                };
                let k = fc.ctx().user_key.insert(UserKey(Default::default()));
                let ok = hex2bin(plain_bytes_mut(&mut k.0), s);

                // the VFS frees it with kfree(), not kfree_sensitive()
                wipe(s);

                if !ok {
                    fc.ctx().user_key = None;
                    return Err(fc.invalf(format_args!("user_key: expected {} hex digits",
                                                      size_of::<c::bch_key>() * 2)));
                }
                Ok(0)
            }

            _ => {
                // for reconfigure, we already have the filesystem
                let fs = fc.root_super().map(|sb| NonNull::from(sb.data()));
                // SAFETY: the superblock being reconfigured's, which holds it
                // for the call
                let fs = fs.map(|fs| unsafe { fs.as_ref() });

                let mut err = Printbuf::new();
                let p = fc.ctx();
                let ret = crate::opts::parse_one_mount_opt(fs, Some(&mut p.opts), Some(&mut p.parse_later),
                                                           key.to_bytes(), val.as_deref(), &mut err);
                // @err describes only a bad value: the refusals returned
                // directly (not an option, quota without
                // CONFIG_BCACHEFS_QUOTA) leave it empty
                if let Err(e) = ret {
                    fc.errorfc(format_args!("option {}: {}", key_str,
                                            if err.as_str().is_empty() { e.msg() } else { err.as_str() }));
                    return Err(BchError::from(e.errno()));
                }
                Ok(0)
            }
        }
    }

    fn get_tree(fc: &mut FsContext<Self>) -> Result<Root<Self>, BchError> {
        // Accumulated by parse_param() from however many "source" parameters
        if fc.ctx().devs.is_empty() {
            return Err(fc.invalf(format_args!("no device to mount")));
        }

        fc.set_sb_flags(c::SB_NOSEC as u32);

        let ret = match Self::mounted(fc) {
            Ok(Some(root)) => Ok(root),
            Ok(None) => Self::open(fc).and_then(|fs| Self::new_super(fc, fs)),
            Err(e) => Err(e),
        };

        ret.map_err(|e| {
            // errorfc(): logfc() falls back to printk with no fs_context log,
            // so a mount(2) caller gets it in dmesg, an fsconfig(2) one on
            // the terminal
            fc.errorfc(format_args!("{}", e.msg()));

            // An -EROFS derived errcode from recovery - from the journal, on
            // an inconsistency - would have util-linux retry the mount
            // read-only, which is confusing
            if e.matches(c::EROFS) && e.raw() != c::EROFS as i32 {
                c::EIO.into()
            } else {
                e
            }
        })
    }

    fn reconfigure(fc: &mut FsContext<Self>, sb: ReconfSuper<'_, Fs>) -> Result<(), BchError> {
        let fs = sb.sb().data();
        let rdonly = fc.rdonly();
        {
            let opts = &mut fc.ctx().opts;
            opts.set_read_only(rdonly);
        }

        fs.reconcile_wakeup();

        // Gone read-only unasked: an error, emergency read-only, and there's no
        // coming back from that in this mount. What was asked for is
        // fc->sb_flags, not c->opts.read_only: emergency ro stops the
        // filesystem without rewriting the option, so that would find nothing
        // to do and report success, read-only.
        if !rdonly && fs.flag(c::bch_fs_flags::BCH_FS_emergency_ro) {
            fc.errorfc(format_args!("cannot go read-write: filesystem is in emergency read-only"));
            return fs.throw(bch_errcode::BCH_ERR_emergency_ro);
        }

        let want_ro = fc.ctx().opts.read_only;
        if want_ro != fs.opts().read_only.get() {
            let _lock = fs.state_lock_write();
            // Stored as C did, not set as an option; sb_lock for the store
            // alone - going read-write writes the superblock
            let set_ro = |v: u8| fs.opts_writer(&mut fs.sb_lock()).read_only.set(v);

            if want_ro != 0 {
                sb.sync()?;
                fs.read_only();
                sb.set_rdonly(true);
            } else {
                // __bch2_fs_read_write() gates reconcile on this:
                set_ro(0);

                if let Err(e) = fs.read_write() {
                    set_ro(1);
                    // bch_err() says which filesystem, errorfc() reaches the
                    // caller; the throw discards the errcode
                    crate::bch_err!(fs, "error going read-write: {}", e.msg());
                    fc.errorfc(format_args!("error going read-write: {}", e.msg()));
                    return fs.throw(bch_errcode::BCH_ERR_EINVAL_reconfigure_read_write);
                }
                sb.set_rdonly(false);
            }

            set_ro(want_ro);
        }

        let opts = &fc.ctx().opts;
        if opts.defined(opt_id::Opt_errors) {
            fs.opts_writer(&mut fs.sb_lock()).errors.set(opts.errors);
        }
        Ok(())
    }
}

/// fc->source in step with the devices given: it's what /proc/mounts and
/// mountinfo show, and taking "source" ourselves means
/// vfs_parse_fs_param_source() never sets it. It has to end up as one
/// colon-separated source would have.
fn source_append(fc: &mut FsContext<Bcachefs>, s: &[u8]) -> Result<(), BchError> {
    let mut new = Printbuf::new();
    if let Some(old) = fc.source() {
        new.write_bytes(old.to_bytes());
        new.write_bytes(b":");
    }
    new.write_bytes(s);
    if new.allocation_failure() {
        return Err(c::ENOMEM.into());
    }
    Ok(fc.set_source(new.as_c_str())?)
}

/// @hex, exactly twice @dst's length in hex digits, as bytes into @dst.
fn hex2bin(dst: &mut [u8], hex: &[u8]) -> bool {
    fn nibble(h: u8) -> Option<u8> {
        (h as char).to_digit(16).map(|d| d as u8)
    }

    hex.len() == dst.len() * 2 &&
        dst.iter_mut().zip(hex.chunks(2)).all(|(d, h)| match (nibble(h[0]), nibble(h[1])) {
            (Some(a), Some(b)) => { *d = a << 4 | b; true }
            _ => false,
        })
}

// ── Registration ─────────────────────────────────────────────────────────

static BCACHEFS_TYPE: FsTypeTable<Bcachefs> = FsTypeTable::new();

#[c_export]
pub fn bch2_fs_type_register() -> c_int {
    BCACHEFS_TYPE.register().map_or_else(|e| e.to_errno(), |()| 0)
}

#[c_export]
pub fn bch2_fs_type_unregister() {
    BCACHEFS_TYPE.unregister()
}
