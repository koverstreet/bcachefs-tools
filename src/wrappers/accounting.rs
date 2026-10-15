use anyhow::anyhow;
use bch_bindgen::c;
use bcachefs_kernel::metadata_version;
use bcachefs_kernel::util::vstructs::UnvalidatedRefEntries;

use super::handle::BcachefsHandle;
use super::ioctl::{grow_on_erange, ioctl_ptr, IoctlBuf, BCH_IOCTL_QUERY_ACCOUNTING, BCH_IOCTL_QUERY_ACCOUNTING_v2};
use super::sysfs::bcachefs_kernel_version;

// Re-export types and functions from bcachefs_kernel::accounting for consumers
// that were importing from this module.
pub use bcachefs_kernel::accounting::*;

/// Result of query_accounting ioctl.
pub struct AccountingResult {
    pub capacity: u64,
    pub used: u64,
    pub online_reserved: u64,
    /// Free space by replica count: free[n - 1] is what could be granted at n
    /// replicas, cumulative and non-increasing. Empty on a kernel too old for
    /// the v2 ioctl - free space is a vector we simply can't see there, which
    /// is different from it being zero, so callers print nothing rather than
    /// guess.
    pub free: Vec<u64>,
    /// The same vector counting only what's writable without waiting on
    /// copygc. Where it's short of `free`, that difference is the allocator's
    /// backlog, not space we don't have.
    pub free_now: Vec<u64>,
    pub entries: Vec<AccountingEntry>,
}

impl BcachefsHandle {
    /// Query filesystem accounting data.
    ///
    /// Tries the v2 ioctl, which also reports free space by replica count, and
    /// falls back to v1 on a kernel that doesn't have it. ENOTTY is the only
    /// error that means "too old" - anything else is a real failure and is
    /// returned, rather than silently downgraded.
    pub fn query_accounting(&self, type_mask: u32) -> anyhow::Result<AccountingResult> {
        match self.query_accounting_v2(type_mask) {
            Err(e) if e.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error())
                == Some(libc::ENOTTY) => self.query_accounting_v1(type_mask),
            r => r,
        }
    }

    fn query_accounting_v2(&self, type_mask: u32) -> anyhow::Result<AccountingResult> {
        let buf = grow_on_erange(1 << 10, MAX_REPLY, |bytes| {
            let mut buf = IoctlBuf::<c::bch_ioctl_query_accounting_v2>::with_trailing_bytes(bytes);
            buf.hdr_mut().accounting_u64s = (bytes / 8) as u32;
            buf.hdr_mut().accounting_types_mask = type_mask;
            unsafe { ioctl_ptr::<BCH_IOCTL_QUERY_ACCOUNTING_v2>(&self.ioctl_fd(), buf.as_mut_ptr())? };
            Ok(buf)
        })?;
        let hdr = buf.hdr();

        Ok(AccountingResult {
            capacity: hdr.capacity,
            used: hdr.used,
            online_reserved: hdr.online_reserved,
            free: hdr.free.to_vec(),
            free_now: hdr.free_now.to_vec(),
            entries: parse_accounting_entries(buf.trailing_bytes(hdr.accounting_u64s as usize * 8))?,
        })
    }

    fn query_accounting_v1(&self, type_mask: u32) -> anyhow::Result<AccountingResult> {
        let buf = grow_on_erange(1 << 10, MAX_REPLY, |bytes| {
            let mut buf = IoctlBuf::<c::bch_ioctl_query_accounting>::with_trailing_bytes(bytes);
            buf.hdr_mut().accounting_u64s = (bytes / 8) as u32;
            buf.hdr_mut().accounting_types_mask = type_mask;
            unsafe { ioctl_ptr::<BCH_IOCTL_QUERY_ACCOUNTING>(&self.ioctl_fd(), buf.as_mut_ptr())? };
            Ok(buf)
        })?;
        let hdr = buf.hdr();

        Ok(AccountingResult {
            capacity: hdr.capacity,
            used: hdr.used,
            online_reserved: hdr.online_reserved,
            free: Vec::new(),
            free_now: Vec::new(),
            entries: parse_accounting_entries(buf.trailing_bytes(hdr.accounting_u64s as usize * 8))?,
        })
    }
}

/// A reply bigger than this is a kernel that won't stop saying ERANGE:
/// per-inode accounting on a large filesystem is the most there is.
const MAX_REPLY: usize = 1 << 30;

/// The accounting keys of a QUERY_ACCOUNTING reply, one after another: each
/// checked against the bytes there are, an error if it doesn't fit or isn't
/// an accounting key - the kernel and us disagreeing about the reply.
fn parse_accounting_entries(data: &[u8]) -> anyhow::Result<Vec<AccountingEntry>> {
    // Kernels before disk_accounting_big_endian wrote the pos byte swapped
    let kernel_version = bcachefs_kernel_version();
    let need_swab = kernel_version > 0
        && kernel_version < u32::from(metadata_version::disk_accounting_big_endian) as u64;

    UnvalidatedRefEntries::<c::bkey_i>::new(data)
        .map(|k| {
            let k = k.map_err(|e| anyhow!("QUERY_ACCOUNTING reply: {e}"))?.key();
            let a = k.arm::<c::bch_accounting>().ok_or_else(|| {
                anyhow!("QUERY_ACCOUNTING reply: a key of type {} at {}", k.k.type_, k.k.p)
            })?;

            let mut p = k.k.p;
            if need_swab {
                unsafe { c::bch2_bpos_swab(&mut p) };
            }
            Ok(AccountingEntry { pos: c::disk_accounting_pos::from_bpos(p), counters: a.v().d.to_vec() })
        })
        .collect()
}
