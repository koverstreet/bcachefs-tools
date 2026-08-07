// SPDX-License-Identifier: GPL-2.0

//! Rust implementations of superblock read/write operations.

use std::os::unix::fs::FileExt;
use std::os::unix::io::FromRawFd;

use bch_bindgen::c;

/// Print error to stderr and exit with failure status.
/// Matches the C `die()` function behavior.
pub fn die(msg: &str) -> ! {
    eprintln!("{}", msg);
    std::process::exit(1);
}

/// Wrap a borrowed file descriptor as a `File` without taking ownership.
///
/// The caller must ensure the fd remains valid for the lifetime of the
/// returned `ManuallyDrop<File>`. The fd will NOT be closed on drop.
pub fn borrowed_file(fd: i32) -> std::mem::ManuallyDrop<std::fs::File> {
    std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Compute the total byte size of a variable-length superblock struct.
/// Equivalent to C's `vstruct_bytes(sb)`.
pub fn vstruct_bytes_sb(sb: &c::bch_sb) -> usize {
    std::mem::size_of::<c::bch_sb>() + u32::from_le(sb.u64s) as usize * 8
}

/// Compute the superblock checksum using the csum type stored in the sb.
fn csum_vstruct_sb(sb: *mut c::bch_sb) -> c::bch_csum {
    unsafe { c::rust_csum_vstruct_sb(sb) }
}

/// Write superblock to all layout locations on disk.
///
/// Exits on I/O errors (matches C `die()` behavior).
pub fn bch2_super_write<S: SbAccess>(fd: i32, sb: &mut S) {
    let file = borrowed_file(fd);

    let bs = crate::wrappers::bdev::get_blocksize_physical_hint(fd) as usize;
    let layout_range = {
        let off = std::mem::offset_of!(c::bch_sb, layout);
        off..off + std::mem::size_of::<c::bch_sb_layout>()
    };

    let nr_superblocks = sb.sb().layout.nr_superblocks as usize;
    for i in 0..nr_superblocks {
        let offset_le = sb.sb().layout.sb_offset[i];
        let offset_sectors = u64::from_le(offset_le);

        sb.sb_mut().offset = offset_le;
        sb.sb_mut().csum = csum_vstruct_sb(sb.sb_mut());

        let sb_src = sb.sb_bytes();
        let sb_bytes = sb_src.len();
        let layout_src = &sb_src[layout_range.clone()];

        if offset_sectors == c::BCH_SB_SECTOR as u64 && bs > 4096 {
            // Layout and superblock are in the same aligned block;
            // write them together.
            let layout_offset = (c::BCH_SB_LAYOUT_SECTOR as usize) << 9;
            let sb_offset = (offset_sectors as usize) << 9;
            let write_len = round_up(sb_offset + sb_bytes, bs);
            let mut buf = vec![0u8; write_len];

            buf[layout_offset..layout_offset + layout_src.len()].copy_from_slice(layout_src);
            buf[sb_offset..sb_offset + sb_bytes].copy_from_slice(sb_src);

            pwrite_exact(&file, &buf, 0);
        } else {
            if offset_sectors == c::BCH_SB_SECTOR as u64 {
                // Write backup layout in the block preceding the superblock
                let mut buf = vec![0u8; bs];

                file.read_exact_at(&mut buf, 4096 - bs as u64)
                    .unwrap_or_else(|e| die(&format!("pread failed at offset {}: {}", 4096 - bs, e)));

                buf[bs - layout_src.len()..].copy_from_slice(layout_src);

                pwrite_exact(&file, &buf, 4096 - bs as u64);
            }

            let write_len = round_up(sb_bytes, bs);
            let mut buf = vec![0u8; write_len];
            buf[..sb_bytes].copy_from_slice(sb_src);

            pwrite_exact(&file, &buf, offset_sectors << 9);
        }
    }

    if let Err(e) = rustix::fs::fsync(&*file) {
        die(&format!("fsync failed writing superblock: {}", e));
    }
}

/// Read a superblock from disk at the given sector offset, into an
/// owned, validated buffer.
pub fn super_read(fd: i32, sector: u64) -> anyhow::Result<SbBuf> {
    let file = borrowed_file(fd);

    // Read the fixed-size header to learn the full extent
    let header_size = std::mem::size_of::<c::bch_sb>();
    let mut buf = vec![0u8; header_size];
    file.read_exact_at(&mut buf, sector << 9)
        .map_err(|e| anyhow::anyhow!("pread failed at offset {}: {}", sector << 9, e))?;

    let u64s_off = std::mem::offset_of!(c::bch_sb, u64s);
    let u64s = u32::from_le_bytes(buf[u64s_off..u64s_off + 4].try_into().unwrap());
    let bytes = header_size + u64s as usize * 8;

    buf.resize(bytes, 0);
    file.read_exact_at(&mut buf, sector << 9)
        .map_err(|e| anyhow::anyhow!("pread failed at offset {}: {}", sector << 9, e))?;

    SbBuf::from_bytes(&buf)
        .map_err(|e| anyhow::anyhow!("superblock at sector {}: {}", sector, e))
}

fn round_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

/// Write exactly `buf.len()` bytes at `offset`. Exits on error.
fn pwrite_exact(file: &std::fs::File, buf: &[u8], offset: u64) {
    file.write_all_at(buf, offset)
        .unwrap_or_else(|e| die(&format!("pwrite failed at offset {}: {}", offset, e)));
}

use bcachefs_kernel::sb::io::{SbAccess, SbBuf};

/// Default superblock size in 512-byte sectors
pub const SUPERBLOCK_SIZE_DEFAULT: u32 = 2048;

/// Initialize superblock layout with primary and backup superblock positions.
///
/// `block_size` and `bucket_size` are in bytes.
/// `sb_size`, `sb_start`, and `sb_end` are in 512-byte sectors.
pub fn sb_layout_init(
    l: &mut c::bch_sb_layout,
    block_size: u32,
    bucket_size: u32,
    sb_size: u32,
    sb_start: u64,
    sb_end: u64,
    no_sb_at_end: bool,
) -> anyhow::Result<()> {
    *l = Default::default();

    l.magic.b = bcachefs_kernel::sb::io::BCHFS_MAGIC;
    l.layout_type = 0;
    l.nr_superblocks = 2;
    l.sb_max_size_bits = sb_size.ilog2() as u8;

    // Create two superblocks in the allowed range
    let mut sb_pos = sb_start;
    for i in 0..l.nr_superblocks as usize {
        if sb_pos != c::BCH_SB_SECTOR as u64 {
            let align = (block_size >> 9) as u64;
            sb_pos = sb_pos.div_ceil(align) * align;
        }

        l.sb_offset[i] = sb_pos.to_le();
        sb_pos += sb_size as u64;
    }

    if sb_pos > sb_end {
        return Err(anyhow::anyhow!(
            "insufficient space for superblocks: need {} sectors but only {} available",
            sb_pos - sb_start, sb_end - sb_start
        ));
    }

    // Also create a backup superblock at the end of the disk:
    //
    // If we're not creating a superblock at the default offset, it
    // means we're being run from the migrate tool and we could be
    // overwriting existing data if we write to the end of the disk
    if sb_start == c::BCH_SB_SECTOR as u64 && !no_sb_at_end {
        let sb_max_size = 1u64 << l.sb_max_size_bits;
        let bucket_sectors = (bucket_size >> 9) as u64;
        let backup_sb = (sb_end - sb_max_size) / bucket_sectors * bucket_sectors;
        let idx = l.nr_superblocks as usize;
        l.sb_offset[idx] = backup_sb.to_le();
        l.nr_superblocks += 1;
    }

    Ok(())
}
