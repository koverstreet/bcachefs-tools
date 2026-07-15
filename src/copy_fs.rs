// SPDX-License-Identifier: GPL-2.0
//
// Copy a POSIX directory tree into a bcachefs filesystem.
//
// Two modes:
//   - Copy (format --source): data is written to the new filesystem
//   - Migrate (bcachefs migrate): data extents point at existing on-disk locations
//
// Converted from c_src/posix_to_bcachefs.c.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use bch_bindgen::btree;
use bch_bindgen::c;
use bch_bindgen::data::io::{block_on, MAX_IO_SIZE};
use bch_bindgen::errcode::{self, BchError, bch_errcode};
use bch_bindgen::fs::Fs;

use crate::util::AlignedBuf;

const BCACHEFS_ROOT_INO: u64 = 4096;
const BCACHEFS_ROOT_SUBVOL: u64 = 1;

/// DT_* constants (from dirent.h)
const DT_DIR: u8  = 4;
const DT_REG: u8  = 8;
const DT_LNK: u8  = 10;

/// Xattr namespace indices matching KEY_TYPE_XATTR_INDEX_* in xattr_format.h.
const XATTR_INDEX_USER: i32     = 0;
const XATTR_INDEX_ACL_ACCESS: i32 = 1;
const XATTR_INDEX_ACL_DEFAULT: i32 = 2;
const XATTR_INDEX_TRUSTED: i32  = 3;
const XATTR_INDEX_SECURITY: i32 = 4;

fn mode_to_type(mode: u32) -> u8 {
    ((mode >> 12) & 15) as u8
}

fn ret_to_result(ret: i32) -> Result<(), BchError> {
    errcode::ret_to_result(ret).map(|_| ())
}

/// Convert the last OS error (errno) into a BchError.
fn last_err() -> BchError {
    BchError::from_raw(-std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
}

/// Convert a rustix error into a BchError.
fn rustix_err(e: rustix::io::Errno) -> BchError {
    BchError::from_raw(-e.raw_os_error())
}

/// Convert a rustix FileType to a DT_* constant.
fn file_type_to_dtype(ft: rustix::fs::FileType) -> u8 {
    use rustix::fs::FileType;
    match ft {
        FileType::RegularFile     => DT_REG,
        FileType::Directory       => DT_DIR,
        FileType::Symlink         => DT_LNK,
        _                         => 0, // DT_UNKNOWN
    }
}

extern "C" {
    fn rust_link_data(
        c: *mut c::bch_fs,
        dst_inum: u64,
        sectors_delta: *mut i64,
        logical: u64,
        physical: u64,
        length: u64,
    ) -> i32;
}

/// Migrate type matching enum bch_migrate_type in posix_to_bcachefs.h.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MigrateType {
    Copy,
    Migrate,
}

/// State for a copy_fs operation.
pub struct CopyFsState {
    pub migrate_type:   MigrateType,
    pub bcachefs_inum:  u64,
    pub dev:            u64,
    pub reserve_start:  u64,
    pub extents:        Vec<(u64, u64)>,  // (start, end) byte ranges
    pub verbosity:      u32,

    pub total_files:    u64,
    pub total_input:    u64,
    pub total_wrote:    u64,
    pub total_linked:   u64,

    /// Hardlink tracking: source inode -> destination inode
    hardlinks: HashMap<u64, u64>,
}

impl CopyFsState {
    pub fn new_copy() -> Self {
        Self {
            migrate_type:   MigrateType::Copy,
            bcachefs_inum:  0,
            dev:            0,
            reserve_start:  0,
            extents:        Vec::new(),
            verbosity:      0,
            total_files:    0,
            total_input:    0,
            total_wrote:    0,
            total_linked:   0,
            hardlinks:      HashMap::new(),
        }
    }

    pub fn new_migrate(bcachefs_inum: u64, dev: u64, reserve_start: u64) -> Self {
        Self {
            migrate_type:   MigrateType::Migrate,
            bcachefs_inum,
            dev,
            reserve_start,
            extents:        Vec::new(),
            verbosity:      0,
            total_files:    0,
            total_input:    0,
            total_wrote:    0,
            total_linked:   0,
            hardlinks:      HashMap::new(),
        }
    }
}

fn make_qstr(name: &CStr) -> c::qstr {
    c::qstr {
        __bindgen_anon_1: c::qstr__bindgen_ty_1 {
            __bindgen_anon_1: c::qstr__bindgen_ty_1__bindgen_ty_1 {
                hash: 0,
                len: name.to_bytes().len() as u32,
            },
        },
        name: name.as_ptr() as *const u8,
    }
}

fn root_subvol_inum() -> c::subvol_inum {
    c::subvol_inum { subvol: BCACHEFS_ROOT_SUBVOL, inum: BCACHEFS_ROOT_INO }
}

fn subvol_inum(inum: u64) -> c::subvol_inum {
    c::subvol_inum { subvol: 1, inum }
}

fn subvol_inum_eq(a: c::subvol_inum, b: c::subvol_inum) -> bool {
    a.subvol == b.subvol && a.inum == b.inum
}

fn zeroed_subvol_inum() -> c::subvol_inum {
    c::subvol_inum { subvol: 0, inum: 0 }
}

fn unlink_and_rm(
    fs: &Fs,
    dir_inum: c::subvol_inum,
    dir: &mut c::bch_inode_unpacked,
    child_name: &CStr,
) -> Result<(), BchError> {
    let qstr = make_qstr(child_name);
    let mut child: c::bch_inode_unpacked = Default::default();

    btree::trans_commit_do(
        fs,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        c::bch_trans_commit_flags::BCH_TRANS_COMMIT_no_enospc,
        |trans| {
            ret_to_result(unsafe {
                c::bch2_unlink_trans(
                    trans.raw(),
                    dir_inum,
                    dir,
                    zeroed_subvol_inum(),
                    &mut child,
                    &qstr,
                    false,
                )
            })
        },
    )?;

    if child.bi_flags & (c::bch_inode_flags::BCH_INODE_unlinked as u32) == 0 {
        return Ok(());
    }

    let child_inum = c::subvol_inum { subvol: dir_inum.subvol, inum: child.bi_inum };
    ret_to_result(unsafe { c::bch2_inode_rm(fs.raw, child_inum) })
}

fn update_inode(fs: &Fs, inode: &c::bch_inode_unpacked) -> Result<(), BchError> {
    unsafe {
        let mut packed: c::bkey_inode_buf = Default::default();
        c::bch2_inode_pack(&mut packed, inode);
        packed.inode.__bindgen_anon_1.k.as_mut().p.snapshot = u32::MAX;
        ret_to_result(c::bch2_btree_insert(
            fs.raw,
            c::btree_id::BTREE_ID_inodes,
            packed.inode.__bindgen_anon_1.k_i.as_mut(),
            std::ptr::null_mut(),
            c::bch_trans_commit_flags(0u32),
            c::btree_iter_update_trigger_flags::BTREE_ITER_cached,
        ))
    }
}

fn create_or_update_link(
    fs: &Fs,
    dir_inum: c::subvol_inum,
    dir: &mut c::bch_inode_unpacked,
    name: &CStr,
    inum: c::subvol_inum,
) -> Result<(), BchError> {
    let mut dir_hash: c::bch_hash_info = Default::default();
    ret_to_result(unsafe { c::bch2_hash_info_init(fs.raw, dir, &mut dir_hash) })?;

    let qstr = make_qstr(name);
    let mut old_inum: c::subvol_inum = Default::default();
    let ret = unsafe { c::bch2_dirent_lookup(fs.raw, dir_inum, &dir_hash, &qstr, &mut old_inum) };
    let lookup_err = errcode::ret_to_result(ret as i32);

    match lookup_err {
        Err(e) if e.matches(bch_errcode::BCH_ERR_ENOENT_str_hash_lookup) => {
            // Fall through to create
        }
        Err(e) => return Err(e),
        Ok(_) => {
            if subvol_inum_eq(inum, old_inum) {
                return Ok(());
            }
            unlink_and_rm(fs, dir_inum, dir, name)?;
        }
    }

    let mut dir_u: c::bch_inode_unpacked = Default::default();
    let mut inode: c::bch_inode_unpacked = Default::default();

    btree::trans_commit_do(
        fs,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        c::bch_trans_commit_flags(0u32),
        |trans| {
            ret_to_result(unsafe {
                c::bch2_link_trans(trans.raw(), dir_inum, &mut dir_u, inum, &mut inode, &qstr)
            })
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn create_or_update_file(
    fs: &Fs,
    dir_inum: c::subvol_inum,
    dir: &mut c::bch_inode_unpacked,
    name: &CStr,
    uid: u32,
    gid: u32,
    mode: u32,
    rdev: u64,
) -> Result<c::bch_inode_unpacked, BchError> {
    let mut dir_hash: c::bch_hash_info = Default::default();
    ret_to_result(unsafe { c::bch2_hash_info_init(fs.raw, dir, &mut dir_hash) })?;

    let qname = make_qstr(name);
    let mut child_inum: c::subvol_inum = Default::default();

    let ret = unsafe {
        c::bch2_dirent_lookup(fs.raw, dir_inum, &dir_hash, &qname, &mut child_inum)
    };

    let mut child_inode: c::bch_inode_unpacked = Default::default();

    if errcode::ret_to_result(ret as i32).is_ok() {
        // Already exists — update
        ret_to_result(unsafe {
            c::bch2_inode_find_by_inum(fs.raw, child_inum, &mut child_inode)
        })?;

        child_inode.bi_mode = mode as u16;
        child_inode.bi_uid = uid;
        child_inode.bi_gid = gid;
        child_inode.bi_dev = rdev as u32;

        // bch2_fsck_write_inode has its own commit_do loop internally —
        // don't wrap it in trans_commit_do or it double-nests.
        {
            let trans = btree::BtreeTrans::new(fs);
            ret_to_result(unsafe {
                c::bch2_fsck_write_inode(trans.raw(), &mut child_inode)
            })?;
        }
    } else {
        unsafe { c::bch2_inode_init_early(fs.raw, &mut child_inode) };

        btree::trans_commit_do(
            fs,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            c::bch_trans_commit_flags(0u32),
            |trans| {
                ret_to_result(unsafe {
                    c::bch2_create_trans(
                        trans.raw(),
                        dir_inum,
                        dir,
                        &mut child_inode,
                        &qname,
                        uid,
                        gid,
                        mode as u16,
                        rdev,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        zeroed_subvol_inum(),
                        0,
                    )
                })
            },
        )?;
    }

    Ok(child_inode)
}

/// Resolve xattr name prefix to bcachefs xattr index.
/// Returns (index, stripped_name) or None if unsupported.
fn xattr_resolve_name(name: &[u8]) -> Option<(i32, &[u8])> {
    if let Some(rest) = name.strip_prefix(b"user.") {
        Some((XATTR_INDEX_USER, rest))
    } else if let Some(rest) = name.strip_prefix(b"trusted.") {
        Some((XATTR_INDEX_TRUSTED, rest))
    } else if let Some(rest) = name.strip_prefix(b"security.") {
        Some((XATTR_INDEX_SECURITY, rest))
    } else if let Some(rest) = name.strip_prefix(b"system.posix_acl_access") {
        Some((XATTR_INDEX_ACL_ACCESS, rest))
    } else if let Some(rest) = name.strip_prefix(b"system.posix_acl_default") {
        Some((XATTR_INDEX_ACL_DEFAULT, rest))
    } else {
        None
    }
}

fn copy_times(fs: &Fs, dst: &mut c::bch_inode_unpacked, src: &libc::stat) {
    let make_ts = |sec, nsec| c::timespec { tv_sec: sec, tv_nsec: nsec };
    unsafe {
        dst.bi_atime = c::rust_timespec_to_bch2_time(fs.raw, make_ts(src.st_atime, src.st_atime_nsec)) as u64;
        dst.bi_mtime = c::rust_timespec_to_bch2_time(fs.raw, make_ts(src.st_mtime, src.st_mtime_nsec)) as u64;
        dst.bi_ctime = c::rust_timespec_to_bch2_time(fs.raw, make_ts(src.st_ctime, src.st_ctime_nsec)) as u64;
    }
}

fn copy_xattrs(
    fs: &Fs,
    dst: &mut c::bch_inode_unpacked,
    src: &CStr,
) -> Result<(), BchError> {
    let mut attrs_buf = vec![0u8; 65536]; // XATTR_LIST_MAX
    let attrs_size = unsafe {
        libc::llistxattr(src.as_ptr(), attrs_buf.as_mut_ptr() as *mut libc::c_char, attrs_buf.len())
    };
    if attrs_size < 0 {
        return Ok(()); // silently skip if xattrs not supported
    }

    let mut pos = 0usize;
    while pos < attrs_size as usize {
        let end = attrs_buf[pos..].iter().position(|&b| b == 0).unwrap() + pos;
        let attr_name = &attrs_buf[pos..end];
        pos = end + 1;

        let (xattr_type, stripped) = match xattr_resolve_name(attr_name) {
            Some(v) => v,
            None => continue,
        };

        let mut val_buf = vec![0u8; 65536]; // XATTR_SIZE_MAX
        let attr_cstr = CString::new(attr_name).unwrap();
        let val_size = unsafe {
            libc::lgetxattr(
                src.as_ptr(),
                attr_cstr.as_ptr(),
                val_buf.as_mut_ptr() as *mut libc::c_void,
                val_buf.len(),
            )
        };
        if val_size < 0 {
            continue;
        }

        let stripped_cstr = CString::new(stripped).unwrap();

        btree::trans_commit_do(
            fs,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            c::bch_trans_commit_flags(0u32),
            |trans| {
                ret_to_result(unsafe {
                    c::bch2_xattr_set(
                        trans.raw(),
                        subvol_inum(dst.bi_inum),
                        dst,
                        stripped_cstr.as_ptr(),
                        val_buf.as_ptr() as *const std::ffi::c_void,
                        val_size as usize,
                        xattr_type,
                        0,
                    )
                })
            },
        )?;
    }

    Ok(())
}

fn write_data(
    fs: &Fs,
    dst_inode: &mut c::bch_inode_unpacked,
    offset: u64,
    data: &[u8],
) -> Result<(), BchError> {
    let result = block_on(
        fs.write(dst_inode.bi_inum, offset, 1, 1, data, u64::MAX)
    )?;
    dst_inode.bi_sectors = (dst_inode.bi_sectors as i64 + result.sectors_delta) as u64;
    Ok(())
}

fn copy_data(
    fs: &Fs,
    dst_inode: &mut c::bch_inode_unpacked,
    src_fd: BorrowedFd,
    start: u64,
    end: u64,
) -> Result<(), BchError> {
    let block_size = fs.block_bytes();
    let mut pos = start;

    while pos < end {
        let len = std::cmp::min(end - pos, MAX_IO_SIZE as u64) as usize;
        let padded = ((len as u64).div_ceil(block_size) * block_size) as usize;

        let mut buf = AlignedBuf::new(padded);
        rustix::io::pread(src_fd, &mut buf[..len], pos)
            .map_err(rustix_err)?;

        write_data(fs, dst_inode, pos, &buf)?;
        pos += len as u64;
    }

    Ok(())
}

fn link_data(
    fs: &Fs,
    dst: &mut c::bch_inode_unpacked,
    logical: u64,
    physical: u64,
    length: u64,
) -> Result<(), BchError> {
    let mut sectors_delta: i64 = 0;
    let ret = unsafe {
        rust_link_data(fs.raw, dst.bi_inum, &mut sectors_delta, logical, physical, length)
    };
    dst.bi_sectors = (dst.bi_sectors as i64 + sectors_delta) as u64;
    ret_to_result(ret)
}

fn copy_link(
    fs: &Fs,
    dst_inum: c::subvol_inum,
    dst: &mut c::bch_inode_unpacked,
    src: &CStr,
) -> Result<(), BchError> {
    let block_size = fs.block_bytes();

    let mut i_sectors_delta: i64 = 0;
    ret_to_result(unsafe {
        c::bch2_fpunch(fs.raw, dst_inum, 0, u64::MAX, &mut i_sectors_delta)
    })?;
    dst.bi_sectors = (dst.bi_sectors as i64 + i_sectors_delta) as u64;

    let target = rustix::fs::readlinkat(rustix::fs::CWD, src, Vec::new())
        .map_err(rustix_err)?;
    let target_bytes = target.as_bytes();
    let link_len = target_bytes.len() as u64;

    let mut buf = AlignedBuf::new(MAX_IO_SIZE);
    buf[..target_bytes.len()].copy_from_slice(target_bytes);
    let padded = (link_len.div_ceil(block_size) * block_size) as usize;

    write_data(fs, dst, 0, &buf[..padded])
}

fn link_file_data(
    fs: &Fs,
    s: &mut CopyFsState,
    dst: &mut c::bch_inode_unpacked,
    src_fd: BorrowedFd,
    src_path: &CStr,
    src_size: u64,
) -> Result<(), BchError> {
    let block_size = fs.block_bytes();

    // First pass: check for unknown extents and fsync if found
    let fm = fiemap::Fiemap::new(&src_fd);
    for extent in fm {
        let extent = extent.map_err(|_| BchError::from_raw(-libc::EIO))?;
        if extent.fe_flags.contains(fiemap::FiemapExtentFlags::UNKNOWN) {
            rustix::fs::fsync(src_fd).map_err(rustix_err)?;
            break;
        }
    }

    // Second pass: link or copy extents
    let fm = fiemap::Fiemap::new(&src_fd);
    for extent in fm {
        let extent = extent.map_err(|_| BchError::from_raw(-libc::EIO))?;

        s.total_input += extent.fe_length;

        let src_max = src_size.div_ceil(block_size) * block_size;
        let length = std::cmp::min(extent.fe_length, src_max - extent.fe_logical);
        let visible_len = std::cmp::min(src_size - extent.fe_logical, length);

        if (extent.fe_logical & (block_size - 1)) != 0 || (length & (block_size - 1)) != 0 {
            let msg = format!("Unaligned extent in {} - can't handle", src_path.to_string_lossy());
            eprintln!("{}", msg);
            return Err(BchError::from_raw(-libc::EINVAL));
        }

        let needs_copy = s.migrate_type == MigrateType::Copy
            || extent.fe_flags.intersects(
                fiemap::FiemapExtentFlags::UNKNOWN
                    | fiemap::FiemapExtentFlags::ENCODED
                    | fiemap::FiemapExtentFlags::NOT_ALIGNED
                    | fiemap::FiemapExtentFlags::DATA_INLINE,
            );

        if needs_copy {
            copy_data(fs, dst, src_fd, extent.fe_logical, extent.fe_logical + visible_len)?;
            s.total_wrote += visible_len;
            continue;
        }

        // If the data is in bcachefs's superblock region, copy it
        if extent.fe_physical < s.reserve_start {
            copy_data(fs, dst, src_fd, extent.fe_logical, extent.fe_logical + visible_len)?;
            s.total_wrote += visible_len;
            continue;
        }

        if (extent.fe_physical & (block_size - 1)) != 0 {
            let msg = format!("Unaligned extent in {} - can't handle", src_path.to_string_lossy());
            eprintln!("{}", msg);
            return Err(BchError::from_raw(-libc::EINVAL));
        }

        range_add(&mut s.extents, extent.fe_physical, length);
        link_data(fs, dst, extent.fe_logical, extent.fe_physical, length)?;
        s.total_linked += length;
    }

    Ok(())
}

/// Add a range to the extents list.
fn range_add(extents: &mut Vec<(u64, u64)>, start: u64, len: u64) {
    extents.push((start, start + len));
}

/// Sort and merge overlapping ranges.
fn ranges_sort_merge(extents: &mut Vec<(u64, u64)>) {
    extents.sort_by_key(|&(start, _)| start);
    let mut i = 0;
    while i + 1 < extents.len() {
        if extents[i].1 >= extents[i + 1].0 {
            extents[i].1 = std::cmp::max(extents[i].1, extents[i + 1].1);
            extents.remove(i + 1);
        } else {
            i += 1;
        }
    }
}

/// Iterate over holes in [0, end) not covered by extents.
fn for_each_hole(
    extents: &[(u64, u64)],
    end: u64,
    mut f: impl FnMut(u64, u64) -> Result<(), BchError>,
) -> Result<(), BchError> {
    let mut pos = 0u64;
    for &(start, stop) in extents {
        if pos < start {
            f(pos, start)?;
        }
        pos = stop;
    }
    if pos < end {
        f(pos, end)?;
    }
    Ok(())
}

/// Range of bytes [start, end).
struct Range {
    start: u64,
    end: u64,
}

fn align_range(r: Range, bs: u64) -> Range {
    Range {
        start: r.start / bs * bs,
        end: r.end.div_ceil(bs) * bs,
    }
}

fn seek_data(fd: BorrowedFd, i_size: u64, offset: u64) -> Range {
    use rustix::fs::{seek, SeekFrom};
    let s = match seek(fd, SeekFrom::Data(offset as i64)) {
        Ok(s) => s,
        Err(_) => return Range { start: 0, end: 0 },
    };
    let e = seek(fd, SeekFrom::Hole(s as i64)).unwrap_or(i_size);
    Range { start: s, end: e }
}

fn seek_data_aligned(fd: BorrowedFd, i_size: u64, offset: u64, bs: u64) -> Range {
    let mut r = align_range(seek_data(fd, i_size, offset), bs);
    if r.end == 0 {
        return r;
    }
    loop {
        let n = align_range(seek_data(fd, i_size, r.end), bs);
        if n.end == 0 || r.end < n.start {
            break;
        }
        r.end = n.end;
    }
    r
}

fn seek_mismatch(buf1: &[u8], buf2: &[u8], mut offset: usize, len: usize) -> Range {
    while offset < len && buf1[offset] == buf2[offset] {
        offset += 1;
    }
    if offset == len {
        return Range { start: 0, end: 0 };
    }
    let start = offset;
    while offset < len && buf1[offset] != buf2[offset] {
        offset += 1;
    }
    Range { start: start as u64, end: offset as u64 }
}

fn seek_mismatch_aligned(buf1: &[u8], buf2: &[u8], offset: usize, len: usize, bs: u64) -> Range {
    let mut r = align_range(seek_mismatch(buf1, buf2, offset, len), bs);
    if r.end != 0 {
        loop {
            let n = align_range(seek_mismatch(buf1, buf2, r.end as usize, len), bs);
            if n.end == 0 || r.end < n.start {
                break;
            }
            r.end = n.end;
        }
    }
    r
}

fn copy_sync_file_range(
    fs: &Fs,
    s: &mut CopyFsState,
    dst_inum: c::subvol_inum,
    dst: &mut c::bch_inode_unpacked,
    src_fd: BorrowedFd,
    src_size: u64,
    range: &Range,
) -> Result<(), BchError> {
    let block_size = fs.block_bytes();
    let mut start = range.start;

    while start != range.end {
        let b = std::cmp::min(range.end - start, MAX_IO_SIZE as u64) as usize;

        let mut src_buf = AlignedBuf::new(b);
        let read_len = std::cmp::min(b, (src_size - start) as usize);
        rustix::io::pread(src_fd, &mut src_buf[..read_len], start)
            .map_err(rustix_err)?;

        let mut dst_buf = AlignedBuf::new(b);
        block_on(fs.read(dst_inum, start, dst, &mut dst_buf))?;

        let mut m = Range { start: 0, end: 0 };
        loop {
            m = seek_mismatch_aligned(&src_buf, &dst_buf, m.end as usize, b, block_size);
            if m.end == 0 {
                break;
            }
            write_data(fs, dst, start + m.start, &src_buf[m.start as usize..m.end as usize])?;
            s.total_wrote += m.end - m.start;
        }

        start += b as u64;
    }

    Ok(())
}

fn copy_sync_file_data(
    fs: &Fs,
    s: &mut CopyFsState,
    dst_inum: c::subvol_inum,
    dst: &mut c::bch_inode_unpacked,
    src_fd: BorrowedFd,
    src_size: u64,
) -> Result<(), BchError> {
    let block_size = fs.block_bytes();
    let mut i_sectors_delta: i64 = 0;

    let mut prev = Range { start: 0, end: 0 };

    loop {
        let next = seek_data_aligned(src_fd, src_size, prev.end, block_size);
        if next.end == 0 {
            break;
        }

        if next.start != 0 {
            ret_to_result(unsafe {
                c::bch2_fpunch(fs.raw, dst_inum, prev.end >> 9, next.start >> 9, &mut i_sectors_delta)
            })?;
        }

        copy_sync_file_range(fs, s, dst_inum, dst, src_fd, src_size, &next)?;
        s.total_input += next.end - next.start;
        prev = next;
    }

    // End of file — truncate remaining
    ret_to_result(unsafe {
        c::bch2_fpunch(fs.raw, dst_inum, prev.end >> 9, u64::MAX, &mut i_sectors_delta)
    })?;

    Ok(())
}

/// Directory entry from bcachefs readdir.
struct DirEntry {
    inum:   u64,
    dtype:  u8,
    name:   CString,
}

/// Trampoline for bch2_readdir callback.
#[repr(C)]
struct ReadDirCtx {
    ctx:     c::dir_context,
    entries: *mut Vec<DirEntry>,
}

unsafe extern "C" fn readdir_actor(
    ctx: *mut c::dir_context,
    name: *const libc::c_char,
    name_len: i32,
    _pos: i64,
    inum: u64,
    dtype: u32,
) -> i32 {
    let rctx = ctx as *mut ReadDirCtx;
    let entries = unsafe { &mut *(*rctx).entries };
    let name_slice = unsafe { std::slice::from_raw_parts(name as *const u8, name_len as usize) };
    let cname = CString::new(name_slice).unwrap_or_else(|_| CString::new("?").unwrap());
    entries.push(DirEntry { inum, dtype: dtype as u8, name: cname });
    0
}

fn simple_readdir(
    fs: &Fs,
    dir_inum: c::subvol_inum,
    dir: &mut c::bch_inode_unpacked,
) -> Result<Vec<DirEntry>, BchError> {
    let mut hash_info: c::bch_hash_info = Default::default();
    ret_to_result(unsafe { c::bch2_hash_info_init(fs.raw, dir, &mut hash_info) })?;

    let mut entries = Vec::new();
    let mut rctx = ReadDirCtx {
        ctx: c::dir_context {
            actor: Some(readdir_actor),
            pos: 0,
        },
        entries: &mut entries,
    };

    ret_to_result(unsafe {
        c::bch2_readdir(fs.raw, dir_inum, &mut hash_info, &mut rctx.ctx)
    })?;

    // Sort by (type, name) like the C code
    entries.sort_by(|a, b| {
        a.dtype.cmp(&b.dtype).then_with(|| a.name.cmp(&b.name))
    });

    Ok(entries)
}

fn recursive_remove(
    fs: &Fs,
    dir_inum: c::subvol_inum,
    dir: &mut c::bch_inode_unpacked,
    d: &DirEntry,
) -> Result<(), BchError> {
    let child_inum = c::subvol_inum { subvol: dir_inum.subvol, inum: d.inum };

    let mut child: c::bch_inode_unpacked = Default::default();
    ret_to_result(unsafe { c::bch2_inode_find_by_inum(fs.raw, child_inum, &mut child) })?;

    if (child.bi_mode as u32 & libc::S_IFMT) == libc::S_IFDIR {
        let child_dirents = simple_readdir(fs, child_inum, &mut child)?;
        for entry in &child_dirents {
            recursive_remove(fs, child_inum, &mut child, entry)?;
        }
    }

    unlink_and_rm(fs, dir_inum, dir, &d.name)
}

fn delete_non_matching_dirents(
    fs: &Fs,
    s: &CopyFsState,
    dst_dir_inum: c::subvol_inum,
    dst_dir: &mut c::bch_inode_unpacked,
    src_dirents: &[DirEntryInfo],
) -> Result<(), BchError> {
    let dst_dirents = simple_readdir(fs, dst_dir_inum, dst_dir)?;

    let mut src_idx = 0;
    for dst_d in &dst_dirents {
        while src_idx < src_dirents.len() && dirent_cmp_key(&src_dirents[src_idx]) < dirent_cmp_key_de(dst_d) {
            src_idx += 1;
        }

        let matches = src_idx < src_dirents.len()
            && dirent_cmp_key(&src_dirents[src_idx]) == dirent_cmp_key_de(dst_d);

        if !matches {
            if subvol_inum_eq(dst_dir_inum, root_subvol_inum())
                && dst_d.name.to_bytes() == b"lost+found"
            {
                continue;
            }

            if s.verbosity > 1 {
                println!("deleting {}", dst_d.name.to_string_lossy());
            }

            recursive_remove(fs, dst_dir_inum, dst_dir, dst_d)?;
        }
    }

    Ok(())
}

/// Comparison key for sorting: (dtype, name)
fn dirent_cmp_key(d: &DirEntryInfo) -> (u8, &[u8]) {
    (d.dtype, d.name.to_bytes())
}

fn dirent_cmp_key_de(d: &DirEntry) -> (u8, &[u8]) {
    (d.dtype, d.name.to_bytes())
}

/// Source directory entry info from host filesystem.
struct DirEntryInfo {
    _inum:  u64,
    dtype:  u8,
    name:   CString,
    stat:   libc::stat,
}

fn copy_dir(
    fs: &Fs,
    s: &mut CopyFsState,
    dst: &mut c::bch_inode_unpacked,
    src_fd: OwnedFd,
    src_path: &CStr,
) -> Result<(), BchError> {
    // Dir::read_from borrows the fd (creating an internal dup for iteration),
    // so src_fd remains available for fstatat/openat/fchdir below.
    let mut dir = rustix::fs::Dir::read_from(&src_fd).map_err(rustix_err)?;

    let mut dirents = Vec::new();

    while let Some(entry) = dir.read() {
        let entry = entry.map_err(rustix_err)?;

        let name = entry.file_name();

        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::fstatat(src_fd.as_raw_fd(), name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW)
        };
        if ret < 0 {
            continue;
        }

        dirents.push(DirEntryInfo {
            _inum: entry.ino(),
            dtype: file_type_to_dtype(entry.file_type()),
            name: name.to_owned(),
            stat,
        });
    }

    drop(dir);

    // Sort by (type, name)
    dirents.sort_by(|a, b| {
        a.dtype.cmp(&b.dtype).then_with(|| a.name.cmp(&b.name))
    });

    let dir_inum = subvol_inum(dst.bi_inum);

    delete_non_matching_dirents(fs, s, dir_inum, dst, &dirents)?;

    let oflags = rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOATIME;

    for d in &dirents {
        unsafe { libc::fchdir(src_fd.as_raw_fd()) };

        let name_str = d.name.to_str().unwrap_or("?");
        if name_str == "." || name_str == ".." || name_str == "lost+found" {
            continue;
        }

        if s.migrate_type == MigrateType::Migrate && d.stat.st_ino as u64 == s.bcachefs_inum {
            continue;
        }

        s.total_files += 1;

        let child_path = CString::new(format!(
            "{}/{}",
            src_path.to_string_lossy(),
            name_str
        )).unwrap();

        if s.migrate_type == MigrateType::Migrate && d.stat.st_dev != s.dev {
            eprintln!("{} does not have correct st_dev!", child_path.to_string_lossy());
            return Err(BchError::from_raw(-libc::EINVAL));
        }

        // Hardlink handling
        if (d.stat.st_mode & libc::S_IFMT) == libc::S_IFREG && d.stat.st_nlink > 1 {
            let src_ino = d.stat.st_ino as u64;
            if let Some(&dst_ino) = s.hardlinks.get(&src_ino) {
                create_or_update_link(
                    fs, dir_inum, dst, &d.name,
                    subvol_inum(dst_ino),
                )?;
                continue;
            }
        }

        let mut inode = create_or_update_file(
            fs, dir_inum, dst, &d.name,
            d.stat.st_uid, d.stat.st_gid,
            d.stat.st_mode, d.stat.st_rdev,
        )?;

        let dst_child_inum = subvol_inum(inode.bi_inum);

        // Record hardlink destination
        if (d.stat.st_mode & libc::S_IFMT) == libc::S_IFREG && d.stat.st_nlink > 1 {
            s.hardlinks.insert(d.stat.st_ino as u64, inode.bi_inum);
        }

        let name_cstr = &d.name;
        copy_xattrs(fs, &mut inode, name_cstr)?;

        match mode_to_type(d.stat.st_mode) {
            DT_DIR => {
                let fd = rustix::fs::openat(&src_fd, &d.name, oflags, rustix::fs::Mode::empty())
                    .map_err(rustix_err)?;
                copy_dir(fs, s, &mut inode, fd, &child_path)?;
            }
            DT_REG => {
                inode.bi_size = d.stat.st_size as u64;

                let fd = rustix::fs::openat(&src_fd, &d.name, oflags, rustix::fs::Mode::empty())
                    .map_err(rustix_err)?;

                if s.migrate_type == MigrateType::Migrate {
                    link_file_data(fs, s, &mut inode, fd.as_fd(), &child_path, d.stat.st_size as u64)?;
                } else {
                    copy_sync_file_data(fs, s, dst_child_inum, &mut inode, fd.as_fd(), d.stat.st_size as u64)?;
                }
                // fd dropped here — close is automatic
            }
            DT_LNK => {
                inode.bi_size = d.stat.st_size as u64;
                copy_link(fs, dst_child_inum, &mut inode, name_cstr)?;
            }
            _ => {
                // DT_FIFO, DT_CHR, DT_BLK, DT_SOCK, DT_WHT — nothing else to copy
            }
        }

        copy_times(fs, &mut inode, &d.stat);
        update_inode(fs, &inode)?;
    }

    Ok(())
}

fn reserve_old_fs_space(
    fs: &Fs,
    root_inode: &mut c::bch_inode_unpacked,
    extents: &mut Vec<(u64, u64)>,
    reserve_start: u64,
) -> Result<(), BchError> {
    let ca_ptr = unsafe { (*fs.raw).devs[0] };
    let nbuckets = unsafe { (*ca_ptr).mi.nbuckets };
    let bucket_size = unsafe { (*ca_ptr).mi.bucket_size } as u64;
    let total_sectors = nbuckets * bucket_size;

    let root_inum = subvol_inum(root_inode.bi_inum);
    let name = CString::new("old_migrated_filesystem").unwrap();
    let mut dst = create_or_update_file(
        fs, root_inum, root_inode,
        &name, 0, 0, libc::S_IFREG | 0o400, 0,
    )?;
    dst.bi_size = total_sectors << 9;

    ranges_sort_merge(extents);

    for_each_hole(extents, total_sectors << 9, |hole_start, hole_end| {
        if hole_end <= reserve_start {
            return Ok(());
        }
        let start = std::cmp::max(hole_start, reserve_start);
        link_data(fs, &mut dst, start, start, hole_end - start)
    })?;

    update_inode(fs, &dst)
}

/// Copy a POSIX directory tree into a bcachefs filesystem.
///
/// Entry point for both `bcachefs format --source` and `bcachefs migrate`.
pub fn copy_fs(
    fs: &Fs,
    s: &mut CopyFsState,
    src_fd: BorrowedFd,
    src_path: &CStr,
) -> Result<(), BchError> {
    let raw_fd = src_fd.as_raw_fd();
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(raw_fd, &mut stat) } < 0 || (stat.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        eprintln!("{} is not a directory", src_path.to_string_lossy());
        return Err(BchError::from_raw(-libc::ENOTDIR));
    }

    if s.migrate_type == MigrateType::Migrate {
        rustix::fs::syncfs(src_fd).map_err(rustix_err)?;
    }

    let mut root_inode: c::bch_inode_unpacked = Default::default();
    ret_to_result(unsafe {
        c::bch2_inode_find_by_inum(
            fs.raw,
            root_subvol_inum(),
            &mut root_inode,
        )
    })?;

    if unsafe { libc::fchdir(raw_fd) } < 0 {
        return Err(last_err());
    }

    copy_times(fs, &mut root_inode, &stat);

    let dot = CString::new(".").unwrap();
    copy_xattrs(fs, &mut root_inode, &dot)?;

    let dup_fd = rustix::io::dup(src_fd).map_err(rustix_err)?;
    copy_dir(fs, s, &mut root_inode, dup_fd, src_path)?;

    if s.migrate_type == MigrateType::Migrate {
        reserve_old_fs_space(fs, &mut root_inode, &mut s.extents, s.reserve_start)?;
    }

    update_inode(fs, &root_inode)?;

    // Print summary
    println!("Total files:\t{}", s.total_files);
    print!("Total input:\t");
    print_human_readable(s.total_input);
    println!();

    if s.total_wrote > 0 {
        print!("Wrote:\t");
        print_human_readable(s.total_wrote);
        println!();
    }

    if s.total_linked > 0 {
        print!("Linked:\t");
        print_human_readable(s.total_linked);
        println!();
    }

    Ok(())
}

fn print_human_readable(bytes: u64) {
    if bytes >= 1 << 30 {
        print!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64);
    } else if bytes >= 1 << 20 {
        print!("{:.1} MiB", bytes as f64 / (1u64 << 20) as f64);
    } else if bytes >= 1 << 10 {
        print!("{:.1} KiB", bytes as f64 / (1u64 << 10) as f64);
    } else {
        print!("{} B", bytes);
    }
}
