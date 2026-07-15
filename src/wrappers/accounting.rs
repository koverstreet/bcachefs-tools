use bch_bindgen::c;

use super::handle::BcachefsHandle;
use super::ioctl::bch_ioc_w;
use super::sysfs::bcachefs_kernel_version;

use c::bcachefs_metadata_version::*;

// Re-export types and functions from bch_bindgen::accounting for consumers
// that were importing from this module.
pub use bch_bindgen::accounting::*;

/// Result of query_accounting ioctl.
pub struct AccountingResult {
    pub capacity: u64,
    pub used: u64,
    pub online_reserved: u64,
    pub entries: Vec<AccountingEntry>,
}

/// Header of bch_ioctl_query_accounting (fixed part before flex array).
#[repr(C)]
struct QueryAccountingHeader {
    capacity: u64,
    used: u64,
    online_reserved: u64,
    accounting_u64s: u32,
    accounting_types_mask: u32,
}

impl BcachefsHandle {
    /// Query filesystem accounting data via BCH_IOCTL_QUERY_ACCOUNTING.
    /// Returns None on ENOTTY (old kernel without this ioctl).
    pub fn query_accounting(&self, type_mask: u32) -> Result<AccountingResult, errno::Errno> {
        let hdr_size = std::mem::size_of::<QueryAccountingHeader>();
        let mut accounting_u64s: u32 = 128;

        loop {
            let total_bytes = hdr_size + (accounting_u64s as usize) * 8;
            let mut buf = vec![0u8; total_bytes];

            // Fill header
            let hdr = unsafe { &mut *(buf.as_mut_ptr() as *mut QueryAccountingHeader) };
            hdr.accounting_u64s = accounting_u64s;
            hdr.accounting_types_mask = type_mask;

            // BCH_IOCTL_QUERY_ACCOUNTING is _IOW(0xbc, 21, struct bch_ioctl_query_accounting)
            // The struct has a flex array, so the kernel uses the header size for the ioctl nr.
            // We use bch_ioc_w with the header size.
            let request = bch_ioc_w::<QueryAccountingHeader>(21);
            let ret = unsafe { libc::ioctl(self.ioctl_fd_raw(), request, buf.as_mut_ptr()) };

            if ret == 0 {
                let hdr = unsafe { &*(buf.as_ptr() as *const QueryAccountingHeader) };
                let entries = parse_accounting_entries(
                    &buf[hdr_size..hdr_size + (hdr.accounting_u64s as usize) * 8],
                );

                return Ok(AccountingResult {
                    capacity: hdr.capacity,
                    used: hdr.used,
                    online_reserved: hdr.online_reserved,
                    entries,
                });
            }

            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno == libc::ENOTTY {
                return Err(errno::Errno(libc::ENOTTY));
            }
            if errno == libc::ERANGE {
                accounting_u64s *= 2;
                continue;
            }
            return Err(errno::Errno(errno));
        }
    }
}

/// Parse the raw u64 buffer of bkey_i_accounting entries.
///
/// Each entry starts with a `struct bkey` header (5 u64s = 40 bytes),
/// followed by counters. The `bkey.u64s` field gives the total size
/// of key + value in u64s.
fn parse_accounting_entries(data: &[u8]) -> Vec<AccountingEntry> {
    let mut entries = Vec::new();
    let kernel_version = bcachefs_kernel_version();
    let need_swab = kernel_version > 0
        && kernel_version < bcachefs_metadata_version_disk_accounting_big_endian as u64;

    let mut offset = 0;
    while offset < data.len() {
        let key_u64s = data[offset] as usize;
        if key_u64s == 0 {
            break;
        }

        let entry_bytes = key_u64s * 8;
        if offset + entry_bytes > data.len() {
            break;
        }

        let entry_data = &data[offset..offset + entry_bytes];

        // bkey header is 5 u64s (40 bytes). The bpos is at the end of the bkey.
        // On little-endian: bkey layout is [u64s(1B), format:nw(1B), type(1B), pad(1B),
        //                                   bversion(12B), size(4B), bpos(20B)]
        // bpos starts at byte 20 (offset 20..40)
        const BKEY_U64S: usize = 5;
        const BPOS_OFFSET: usize = 20;

        if entry_bytes < BKEY_U64S * 8 {
            break;
        }

        // Extract bpos
        let mut bpos = c::bpos {
            snapshot: u32::from_ne_bytes(entry_data[BPOS_OFFSET..BPOS_OFFSET+4].try_into().unwrap()),
            offset: u64::from_ne_bytes(entry_data[BPOS_OFFSET+4..BPOS_OFFSET+12].try_into().unwrap()),
            inode: u64::from_ne_bytes(entry_data[BPOS_OFFSET+12..BPOS_OFFSET+20].try_into().unwrap()),
        };

        if need_swab {
            unsafe { c::bch2_bpos_swab(&mut bpos) };
        }

        let pos = DiskAccountingPos::from_bpos(bpos);

        // Counters start after the bkey header (bch_accounting.d[])
        // bch_accounting has just a bch_val (0 bytes), then d[]
        // So counters start at u64 offset BKEY_U64S
        let nr_counters = key_u64s - BKEY_U64S;
        let counters: Vec<u64> = (0..nr_counters)
            .map(|i| {
                let off = (BKEY_U64S + i) * 8;
                u64::from_ne_bytes(entry_data[off..off + 8].try_into().unwrap())
            })
            .collect();

        entries.push(AccountingEntry { pos, counters });
        offset += entry_bytes;
    }

    entries
}
