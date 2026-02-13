// SPDX-License-Identifier: GPL-2.0

//! Rust implementations of superblock read/write operations.

use std::os::unix::fs::FileExt;
use std::os::unix::io::FromRawFd;

use bch_bindgen::c;

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
/// # Safety
/// `sb` must point to a valid, fully initialized `bch_sb`.
///
/// Panics on I/O errors (matches C `die()` behavior).
#[no_mangle]
pub extern "C" fn bch2_super_write(fd: i32, sb: *mut c::bch_sb) {
    let file = borrowed_file(fd);

    let bs = unsafe { c::get_blocksize(fd) } as usize;
    let sb_ref = unsafe { &mut *sb };

    let nr_superblocks = sb_ref.layout.nr_superblocks as usize;
    for i in 0..nr_superblocks {
        sb_ref.offset = sb_ref.layout.sb_offset[i];

        let offset_sectors = u64::from_le(sb_ref.offset);

        if offset_sectors == c::BCH_SB_SECTOR as u64 {
            // Write backup layout at byte 4096
            let buflen = bs.max(4096);
            let mut buf = vec![0u8; buflen];

            // Read existing data at 4096 - bs
            file.read_exact_at(&mut buf[..bs], 4096 - bs as u64)
                .unwrap_or_else(|e| panic!("pread failed at offset {}: {}", 4096 - bs, e));

            // Patch the layout into the end of this block
            let layout_bytes = std::mem::size_of::<c::bch_sb_layout>();
            let src = unsafe {
                std::slice::from_raw_parts(
                    &sb_ref.layout as *const _ as *const u8,
                    layout_bytes,
                )
            };
            buf[bs - layout_bytes..bs].copy_from_slice(src);

            pwrite_exact(&file, &buf[..bs], 4096 - bs as u64);
        }

        sb_ref.csum = csum_vstruct_sb(sb);

        let sb_bytes = vstruct_bytes_sb(unsafe { &*sb });
        let write_len = round_up(sb_bytes, bs);
        let sb_slice = unsafe { std::slice::from_raw_parts(sb as *const u8, write_len) };

        pwrite_exact(&file, sb_slice, offset_sectors << 9);
    }

    rustix::fs::fsync(&*file).expect("fsync failed writing superblock");
}

/// Read a superblock from disk at the given sector offset.
///
/// Returns a malloc'd `bch_sb` pointer (caller must free).
///
/// Panics if the magic doesn't match a bcachefs superblock.
#[no_mangle]
pub extern "C" fn __bch2_super_read(fd: i32, sector: u64) -> *mut c::bch_sb {
    let file = borrowed_file(fd);

    // Read the fixed-size header first
    let header_size = std::mem::size_of::<c::bch_sb>();
    let mut header_buf = vec![0u8; header_size];
    file.read_exact_at(&mut header_buf, sector << 9)
        .unwrap_or_else(|e| panic!("pread failed at offset {}: {}", sector << 9, e));

    let sb_header = unsafe { &*(header_buf.as_ptr() as *const c::bch_sb) };

    if sb_header.magic.b != BCACHE_MAGIC && sb_header.magic.b != BCHFS_MAGIC {
        panic!("not a bcachefs superblock");
    }

    let bytes = vstruct_bytes_sb(sb_header);

    // Use malloc so the caller can free() it (C callers expect this)
    let ptr = unsafe { libc::malloc(bytes) as *mut u8 };
    if ptr.is_null() {
        panic!("allocation failed for superblock ({} bytes)", bytes);
    }

    let buf = unsafe { std::slice::from_raw_parts_mut(ptr, bytes) };
    file.read_exact_at(buf, sector << 9)
        .unwrap_or_else(|e| panic!("pread failed at offset {}: {}", sector << 9, e));

    ptr as *mut c::bch_sb
}

fn round_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

/// Write exactly `buf.len()` bytes at `offset`. Panics on error.
fn pwrite_exact(file: &std::fs::File, buf: &[u8], offset: u64) {
    file.write_all_at(buf, offset)
        .unwrap_or_else(|e| panic!("pwrite failed at offset {}: {}", offset, e));
}

pub const BCACHE_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x4e, 0x1a, 0x45, 0xca,
    0x82, 0x65, 0xf5, 0x7f, 0x48, 0xba, 0x6d, 0x81,
];
pub const BCHFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

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
) {
    *l = unsafe { std::mem::zeroed() };

    l.magic.b = BCHFS_MAGIC;
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
        panic!(
            "insufficient space for superblocks: start {} end {} > {} size {}",
            sb_start, sb_pos, sb_end, sb_size
        );
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
}
