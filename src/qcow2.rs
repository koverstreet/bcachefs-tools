// SPDX-License-Identifier: GPL-2.0

//! QCOW2 sparse image format — writer and reader.
//!
//! Used by `bcachefs dump` to create sparse metadata images and
//! `bcachefs undump` to convert them back to raw device images.

use std::ops::Range;
use std::os::fd::BorrowedFd;

use anyhow::{anyhow, Result};
use rustix::io::Errno;

// ---- Ranges helpers ----

pub type Ranges = Vec<Range<u64>>;

pub fn range_add(ranges: &mut Ranges, offset: u64, size: u64) {
    ranges.push(offset..offset + size);
}

fn ranges_roundup(ranges: &mut Ranges, block_size: u64) {
    for r in ranges.iter_mut() {
        r.start = r.start / block_size * block_size;
        r.end = round_up(r.end, block_size);
    }
}

fn ranges_sort_merge(ranges: &mut Ranges) {
    ranges.sort_by_key(|r| r.start);
    let mut merged = Vec::with_capacity(ranges.len());
    for r in ranges.drain(..) {
        if let Some(last) = merged.last_mut() {
            let last: &mut Range<u64> = last;
            if last.end >= r.start {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        merged.push(r);
    }
    *ranges = merged;
}

pub fn ranges_sort(ranges: &mut Ranges) {
    ranges.sort_by_key(|r| r.start);
}

// ---- I/O helpers ----

pub fn pread_exact(fd: BorrowedFd<'_>, buf: &mut [u8], mut offset: u64) -> Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        match rustix::io::pread(fd, &mut buf[pos..], offset) {
            Ok(0) => return Err(anyhow!("read error: unexpected EOF")),
            Ok(n) => {
                pos += n;
                offset += n as u64;
            }
            Err(Errno::INTR) => continue,
            Err(e) => return Err(anyhow!("read error: {e}")),
        }
    }
    Ok(())
}

fn pwrite_all(fd: BorrowedFd<'_>, buf: &[u8], mut offset: u64) -> Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        match rustix::io::pwrite(fd, &buf[pos..], offset) {
            Ok(0) => return Err(anyhow!("write error: no progress")),
            Ok(n) => {
                pos += n;
                offset += n as u64;
            }
            Err(Errno::INTR) => continue,
            Err(e) => return Err(anyhow!("write error: {e}")),
        }
    }
    Ok(())
}

// ---- qcow2 format constants ----

const QCOW_MAGIC: u32 = (b'Q' as u32) << 24 | (b'F' as u32) << 16 | (b'I' as u32) << 8 | 0xfb;
const QCOW_VERSION: u32 = 2;
const QCOW_OFLAG_COPIED: u64 = 1 << 63;

// Header field offsets and sizes (all big-endian on disk):
//   magic:                  u32   @ 0
//   version:                u32   @ 4
//   backing_file_offset:    u64   @ 8
//   backing_file_size:      u32   @ 16
//   block_bits:             u32   @ 20
//   size:                   u64   @ 24
//   crypt_method:           u32   @ 32
//   l1_size:                u32   @ 36
//   l1_table_offset:        u64   @ 40
//   refcount_table_offset:  u64   @ 48
//   refcount_table_blocks:  u32   @ 56
//   nb_snapshots:           u32   @ 60
//   snapshots_offset:       u64   @ 64
const QCOW2_HDR_BYTES: usize = 72;

fn encode_header(buf: &mut [u8], magic: u32, version: u32, block_bits: u32,
                 size: u64, l1_size: u32, l1_table_offset: u64) {
    buf[0..4].copy_from_slice(&magic.to_be_bytes());
    buf[4..8].copy_from_slice(&version.to_be_bytes());
    buf[8..16].copy_from_slice(&0u64.to_be_bytes());       // backing_file_offset
    buf[16..20].copy_from_slice(&0u32.to_be_bytes());       // backing_file_size
    buf[20..24].copy_from_slice(&block_bits.to_be_bytes());
    buf[24..32].copy_from_slice(&size.to_be_bytes());
    buf[32..36].copy_from_slice(&0u32.to_be_bytes());       // crypt_method
    buf[36..40].copy_from_slice(&l1_size.to_be_bytes());
    buf[40..48].copy_from_slice(&l1_table_offset.to_be_bytes());
    buf[48..56].copy_from_slice(&0u64.to_be_bytes());       // refcount_table_offset
    buf[56..60].copy_from_slice(&0u32.to_be_bytes());       // refcount_table_blocks
    buf[60..64].copy_from_slice(&0u32.to_be_bytes());       // nb_snapshots
    buf[64..72].copy_from_slice(&0u64.to_be_bytes());       // snapshots_offset
}

fn read_be_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(buf[off..off + 4].try_into().unwrap())
}

fn read_be_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_be_bytes(buf[off..off + 8].try_into().unwrap())
}

// ---- Qcow2Image ----

pub struct Qcow2Image<'fd> {
    infd:       BorrowedFd<'fd>,
    outfd:      BorrowedFd<'fd>,
    image_size: u64,
    block_size: u32,
    l1_table:   Vec<u64>,
    l1_index:   Option<u32>,
    l2_table:   Vec<u64>,
    offset:     u64,
}

impl<'fd> Qcow2Image<'fd> {
    pub fn new(infd: BorrowedFd<'fd>, outfd: BorrowedFd<'fd>, block_size: u32) -> Result<Self> {
        assert!(block_size.is_power_of_two());

        let image_size = file_size_fd(infd)?;
        let l2_size = block_size as u64 / 8;
        let l1_size = image_size.div_ceil(block_size as u64 * l2_size) as usize;

        Ok(Qcow2Image {
            infd,
            outfd,
            image_size,
            block_size,
            l1_table:   vec![0u64; l1_size],
            l1_index:   None,
            l2_table:   vec![0u64; l2_size as usize],
            offset:     round_up(QCOW2_HDR_BYTES as u64, block_size as u64),
        })
    }

    /// Borrowed fd of the input device, for callers that need to read
    /// directly (e.g. sanitize path).
    pub fn infd(&self) -> BorrowedFd<'_> {
        self.infd
    }

    fn write_raw(&mut self, buf: &[u8]) -> Result<()> {
        assert!(buf.len() as u64 % self.block_size as u64 == 0);
        pwrite_all(self.outfd, buf, self.offset)?;
        self.offset += buf.len() as u64;
        Ok(())
    }

    fn flush_l2(&mut self) -> Result<()> {
        if let Some(idx) = self.l1_index {
            self.l1_table[idx as usize] = self.offset | QCOW_OFLAG_COPIED;

            let mut buf = vec![0u8; self.block_size as usize];
            for (i, &entry) in self.l2_table.iter().enumerate() {
                buf[i * 8..(i + 1) * 8].copy_from_slice(&entry.to_be_bytes());
            }
            self.write_raw(&buf)?;

            self.l2_table.fill(0);
            self.l1_index = None;
        }
        Ok(())
    }

    fn add_l2(&mut self, src_blk: u64, dst_offset: u64) -> Result<()> {
        let l2_size = self.block_size as u64 / 8;
        let l1_index = (src_blk / l2_size) as u32;
        let l2_index = (src_blk % l2_size) as usize;

        if self.l1_index != Some(l1_index) {
            self.flush_l2()?;
            self.l1_index = Some(l1_index);
        }

        self.l2_table[l2_index] = dst_offset | QCOW_OFLAG_COPIED;
        Ok(())
    }

    /// Write a buffer to the image, mapping src_offset blocks to the
    /// output position. buf.len() must be a multiple of block_size.
    pub fn write_buf(&mut self, buf: &[u8], src_offset: u64) -> Result<()> {
        let dst_offset = self.offset;
        self.write_raw(buf)?;

        let bs = self.block_size as u64;
        let nblocks = buf.len() as u64 / bs;
        for i in 0..nblocks {
            self.add_l2((src_offset + i * bs) / bs, dst_offset + i * bs)?;
        }
        Ok(())
    }

    /// Write ranges read from the input device to the image.
    /// Rounds up and merges the ranges in place.
    pub fn write_ranges(&mut self, ranges: &mut Ranges) -> Result<()> {
        ranges_roundup(ranges, self.block_size as u64);
        ranges_sort_merge(ranges);

        let bs = self.block_size as usize;
        let mut buf = vec![0u8; bs];

        for r in ranges.iter() {
            let mut src_offset = r.start;
            while src_offset < r.end {
                pread_exact(self.infd, &mut buf, src_offset)?;
                self.write_buf(&buf, src_offset)?;
                src_offset += bs as u64;
            }
        }
        Ok(())
    }

    /// Finalize the image: flush pending L2 entries, write L1 table
    /// and header. Consumes self.
    pub fn finish(mut self) -> Result<()> {
        self.flush_l2()?;

        // Write L1 table (big-endian)
        let l1_offset = self.offset;
        let l1_bytes: Vec<u8> = self.l1_table.iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        self.offset += round_up(l1_bytes.len() as u64, self.block_size as u64);
        pwrite_all(self.outfd, &l1_bytes, l1_offset)?;

        // Write header
        let mut header_buf = vec![0u8; self.block_size as usize];
        encode_header(
            &mut header_buf,
            QCOW_MAGIC,
            QCOW_VERSION,
            self.block_size.trailing_zeros(),
            self.image_size,
            self.l1_table.len() as u32,
            l1_offset,
        );
        pwrite_all(self.outfd, &header_buf, 0)?;

        Ok(())
    }
}

/// Convert a qcow2 image back to a raw device image.
pub fn qcow2_to_raw(infd: BorrowedFd<'_>, outfd: BorrowedFd<'_>) -> Result<()> {
    let mut hdr_buf = [0u8; QCOW2_HDR_BYTES];
    pread_exact(infd, &mut hdr_buf, 0)?;

    let magic = read_be_u32(&hdr_buf, 0);
    let version = read_be_u32(&hdr_buf, 4);
    if magic != QCOW_MAGIC {
        return Err(anyhow!("not a qcow2 image"));
    }
    if version != QCOW_VERSION {
        return Err(anyhow!("incorrect qcow2 version"));
    }

    let size = read_be_u64(&hdr_buf, 24);
    rustix::fs::ftruncate(outfd, size)?;

    let block_size = 1u32 << read_be_u32(&hdr_buf, 20);
    let l1_size = read_be_u32(&hdr_buf, 36) as usize;
    let l2_size = block_size as usize / 8;

    // Read L1 table
    let l1_offset = read_be_u64(&hdr_buf, 40);
    let mut l1_buf = vec![0u8; l1_size * 8];
    pread_exact(infd, &mut l1_buf, l1_offset)?;

    let mut l2_buf = vec![0u8; block_size as usize];
    let mut data_buf = vec![0u8; block_size as usize];

    for i in 0..l1_size {
        let l1_entry = read_be_u64(&l1_buf, i * 8);
        if l1_entry == 0 {
            continue;
        }

        pread_exact(infd, &mut l2_buf, l1_entry & !QCOW_OFLAG_COPIED)?;

        for j in 0..l2_size {
            let l2_entry = read_be_u64(&l2_buf, j * 8);
            let src_offset = l2_entry & !QCOW_OFLAG_COPIED;
            if src_offset == 0 {
                continue;
            }

            let dst_offset = (i as u64 * l2_size as u64 + j as u64) * block_size as u64;
            pread_exact(infd, &mut data_buf, src_offset)?;
            pwrite_all(outfd, &data_buf, dst_offset)?;
        }
    }

    Ok(())
}

fn round_up(v: u64, align: u64) -> u64 {
    v.div_ceil(align) * align
}

/// Get the size of a file or block device given a borrowed fd.
fn file_size_fd(fd: BorrowedFd<'_>) -> Result<u64> {
    use rustix::fs::{fstat, FileType};
    use rustix::ioctl::{Getter, ReadOpcode};

    let stat = fstat(fd)?;
    if FileType::from_raw_mode(stat.st_mode) == FileType::BlockDevice {
        type BlkGetSize64 = ReadOpcode<0x12, 114, u64>;
        Ok(unsafe { rustix::ioctl::ioctl(fd, Getter::<BlkGetSize64, u64>::new()) }?)
    } else {
        Ok(stat.st_size as u64)
    }
}
