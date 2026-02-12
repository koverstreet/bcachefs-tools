// SPDX-License-Identifier: GPL-2.0

//! QCOW2 sparse image format — writer and reader.
//!
//! Used by `bcachefs dump` to create sparse metadata images and
//! `bcachefs undump` to convert them back to raw device images.

use std::ops::Range;
use std::os::unix::io::RawFd;

use anyhow::{anyhow, Result};

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

pub fn pread_exact(fd: RawFd, buf: &mut [u8], mut offset: u64) -> Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        let r = unsafe {
            libc::pread(
                fd,
                buf[pos..].as_mut_ptr() as *mut _,
                buf.len() - pos,
                offset as libc::off_t,
            )
        };
        if r < 0 {
            return Err(anyhow!("read error: {}", std::io::Error::last_os_error()));
        }
        if r == 0 {
            return Err(anyhow!("read error: unexpected EOF"));
        }
        pos += r as usize;
        offset += r as u64;
    }
    Ok(())
}

fn pwrite_all(fd: RawFd, buf: &[u8], offset: u64) -> Result<()> {
    let r = unsafe {
        libc::pwrite(
            fd,
            buf.as_ptr() as *const _,
            buf.len(),
            offset as libc::off_t,
        )
    };
    if r < 0 || r as usize != buf.len() {
        return Err(anyhow!("write error: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

// ---- qcow2 format constants ----

const QCOW_MAGIC: u32 = (b'Q' as u32) << 24 | (b'F' as u32) << 16 | (b'I' as u32) << 8 | 0xfb;
const QCOW_VERSION: u32 = 2;
const QCOW_OFLAG_COPIED: u64 = 1 << 63;

#[repr(C)]
struct Qcow2Hdr {
    magic:                  u32,
    version:                u32,
    backing_file_offset:    u64,
    backing_file_size:      u32,
    block_bits:             u32,
    size:                   u64,
    crypt_method:           u32,
    l1_size:                u32,
    l1_table_offset:        u64,
    refcount_table_offset:  u64,
    refcount_table_blocks:  u32,
    nb_snapshots:           u32,
    snapshots_offset:       u64,
}

// ---- Qcow2Image ----

pub struct Qcow2Image {
    infd:       RawFd,
    outfd:      RawFd,
    image_size: u64,
    block_size: u32,
    l1_table:   Vec<u64>,
    l1_index:   Option<u32>,
    l2_table:   Vec<u64>,
    offset:     u64,
}

impl Qcow2Image {
    pub fn new(infd: RawFd, outfd: RawFd, block_size: u32) -> Result<Self> {
        assert!(block_size.is_power_of_two());

        let image_size = file_size_fd(infd)?;
        let l2_size = block_size as u64 / 8;
        let l1_size = div_round_up(image_size, block_size as u64 * l2_size) as usize;

        Ok(Qcow2Image {
            infd,
            outfd,
            image_size,
            block_size,
            l1_table:   vec![0u64; l1_size],
            l1_index:   None,
            l2_table:   vec![0u64; l2_size as usize],
            offset:     round_up(std::mem::size_of::<Qcow2Hdr>() as u64, block_size as u64),
        })
    }

    /// Raw fd of the input device, for callers that need to read
    /// directly (e.g. sanitize path).
    pub fn infd(&self) -> RawFd {
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
        let hdr = Qcow2Hdr {
            magic:                  QCOW_MAGIC.to_be(),
            version:                QCOW_VERSION.to_be(),
            backing_file_offset:    0,
            backing_file_size:      0,
            block_bits:             self.block_size.trailing_zeros().to_be(),
            size:                   self.image_size.to_be(),
            crypt_method:           0,
            l1_size:                (self.l1_table.len() as u32).to_be(),
            l1_table_offset:        l1_offset.to_be(),
            refcount_table_offset:  0,
            refcount_table_blocks:  0,
            nb_snapshots:           0,
            snapshots_offset:       0,
        };

        let mut header_buf = vec![0u8; self.block_size as usize];
        let hdr_bytes = unsafe {
            std::slice::from_raw_parts(
                &hdr as *const Qcow2Hdr as *const u8,
                std::mem::size_of::<Qcow2Hdr>(),
            )
        };
        header_buf[..hdr_bytes.len()].copy_from_slice(hdr_bytes);
        pwrite_all(self.outfd, &header_buf, 0)?;

        Ok(())
    }
}

/// Convert a qcow2 image back to a raw device image.
pub fn qcow2_to_raw(infd: RawFd, outfd: RawFd) -> Result<()> {
    let hdr_size = std::mem::size_of::<Qcow2Hdr>();
    let mut hdr_buf = vec![0u8; hdr_size];
    pread_exact(infd, &mut hdr_buf, 0)?;

    let hdr = unsafe { &*(hdr_buf.as_ptr() as *const Qcow2Hdr) };

    if u32::from_be(hdr.magic) != QCOW_MAGIC {
        return Err(anyhow!("not a qcow2 image"));
    }
    if u32::from_be(hdr.version) != QCOW_VERSION {
        return Err(anyhow!("incorrect qcow2 version"));
    }

    let size = u64::from_be(hdr.size);
    let ret = unsafe { libc::ftruncate(outfd, size as libc::off_t) };
    if ret != 0 {
        return Err(anyhow!("ftruncate: {}", std::io::Error::last_os_error()));
    }

    let block_size = 1u32 << u32::from_be(hdr.block_bits);
    let l1_size = u32::from_be(hdr.l1_size) as usize;
    let l2_size = block_size as usize / 8;

    // Read L1 table
    let l1_offset = u64::from_be(hdr.l1_table_offset);
    let mut l1_buf = vec![0u8; l1_size * 8];
    pread_exact(infd, &mut l1_buf, l1_offset)?;

    let mut l2_buf = vec![0u8; block_size as usize];
    let mut data_buf = vec![0u8; block_size as usize];

    for i in 0..l1_size {
        let l1_entry = u64::from_be_bytes(l1_buf[i * 8..(i + 1) * 8].try_into().unwrap());
        if l1_entry == 0 {
            continue;
        }

        pread_exact(infd, &mut l2_buf, l1_entry & !QCOW_OFLAG_COPIED)?;

        for j in 0..l2_size {
            let l2_entry = u64::from_be_bytes(
                l2_buf[j * 8..(j + 1) * 8].try_into().unwrap(),
            );
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
    (v + align - 1) / align * align
}

fn div_round_up(n: u64, d: u64) -> u64 {
    (n + d - 1) / d
}

// _IOR(0x12, 114, u64) — BLKGETSIZE64 on 64-bit Linux
const BLKGETSIZE64: libc::c_ulong = 0x80081272;

fn file_size_fd(fd: RawFd) -> Result<u64> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(anyhow!("fstat: {}", std::io::Error::last_os_error()));
    }
    if (stat.st_mode & libc::S_IFMT) == libc::S_IFBLK {
        let mut size: u64 = 0;
        if unsafe { libc::ioctl(fd, BLKGETSIZE64, &mut size) } != 0 {
            return Err(anyhow!("BLKGETSIZE64: {}", std::io::Error::last_os_error()));
        }
        Ok(size)
    } else {
        Ok(stat.st_size as u64)
    }
}
