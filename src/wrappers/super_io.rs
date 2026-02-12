// SPDX-License-Identifier: GPL-2.0

//! Rust implementations of superblock read/write operations.

use bch_bindgen::c;

/// Compute the total byte size of a variable-length superblock struct.
fn vstruct_bytes_sb(sb: *const c::bch_sb) -> usize {
    unsafe { c::rust_vstruct_bytes_sb(sb) }
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
    use std::os::unix::io::FromRawFd;

    // Safety: fd is a valid open file descriptor passed from C.
    // We wrap it in ManuallyDrop to avoid closing it when we're done.
    let file = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });

    let bs = unsafe { c::get_blocksize(fd) } as usize;
    let sb_ref = unsafe { &mut *sb };

    let nr_superblocks = sb_ref.layout.nr_superblocks as usize;
    for i in 0..nr_superblocks {
        sb_ref.offset = sb_ref.layout.sb_offset[i];

        let offset_sectors = u64::from_le(sb_ref.offset as u64);

        if offset_sectors == c::BCH_SB_SECTOR as u64 {
            // Write backup layout at byte 4096
            let buflen = bs.max(4096);
            let mut buf = vec![0u8; buflen];

            // Read existing data at 4096 - bs
            pread_exact(&file, &mut buf[..bs], 4096 - bs as u64);

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

        let sb_bytes = vstruct_bytes_sb(sb);
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
    use std::os::unix::io::FromRawFd;

    let file = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });

    // Read the fixed-size header first
    let header_size = std::mem::size_of::<c::bch_sb>();
    let mut header_buf = vec![0u8; header_size];
    pread_exact(&file, &mut header_buf, sector << 9);

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
    pread_exact(&file, buf, sector << 9);

    ptr as *mut c::bch_sb
}

fn round_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

/// Read exactly `buf.len()` bytes at `offset`. Panics on error or short read.
fn pread_exact(file: &std::fs::File, buf: &mut [u8], offset: u64) {
    use std::os::unix::fs::FileExt;

    let n = file.read_at(buf, offset)
        .unwrap_or_else(|e| panic!("pread failed at offset {}: {}", offset, e));
    if n != buf.len() {
        panic!("short pread at offset {}: got {} bytes, expected {}", offset, n, buf.len());
    }
}

/// Write exactly `buf.len()` bytes at `offset`. Panics on error.
fn pwrite_exact(file: &std::fs::File, buf: &[u8], offset: u64) {
    use std::os::unix::fs::FileExt;

    file.write_all_at(buf, offset)
        .unwrap_or_else(|e| panic!("pwrite failed at offset {}: {}", offset, e));
}

const BCACHE_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x4e, 0x1a, 0x45, 0xca,
    0x82, 0x65, 0xf5, 0x7f, 0x48, 0xba, 0x6d, 0x81,
];
const BCHFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];
