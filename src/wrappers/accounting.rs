use bch_bindgen::c;

use super::handle::BcachefsHandle;
use super::printbuf::Printbuf;
use super::ioctl::bch_ioc_w;
use super::sysfs::bcachefs_kernel_version;

use c::bcachefs_metadata_version::*;

pub use c::bch_data_type;
pub use c::bch_compression_type;
pub use c::bch_reconcile_accounting_type;

use bch_data_type::*;

/// Safely convert a raw u8 to bch_data_type.
/// Out-of-range values return BCH_DATA_NR.
pub fn data_type_from_u8(v: u8) -> bch_data_type {
    if (v as u32) < BCH_DATA_NR as u32 {
        // SAFETY: v is in [0, BCH_DATA_NR), all valid #[repr(u32)] discriminants
        unsafe { std::mem::transmute::<u32, bch_data_type>(v as u32) }
    } else {
        BCH_DATA_NR
    }
}

/// Safely convert a raw u8 to bch_compression_type.
fn compression_type_from_u8(v: u8) -> bch_compression_type {
    if (v as u32) < bch_compression_type::BCH_COMPRESSION_TYPE_NR as u32 {
        unsafe { std::mem::transmute::<u32, bch_compression_type>(v as u32) }
    } else {
        bch_compression_type::BCH_COMPRESSION_TYPE_NR
    }
}

/// Safely convert a raw u8 to bch_reconcile_accounting_type.
fn reconcile_type_from_u8(v: u8) -> bch_reconcile_accounting_type {
    if (v as u32) < bch_reconcile_accounting_type::BCH_RECONCILE_ACCOUNTING_NR as u32 {
        unsafe { std::mem::transmute::<u32, bch_reconcile_accounting_type>(v as u32) }
    } else {
        bch_reconcile_accounting_type::BCH_RECONCILE_ACCOUNTING_NR
    }
}

/// Decoded accounting key type.
#[derive(Debug)]
#[allow(dead_code)]
pub enum DiskAccountingPos {
    NrInodes,
    PersistentReserved { nr_replicas: u8 },
    Replicas { data_type: bch_data_type, nr_devs: u8, nr_required: u8, devs: Vec<u8> },
    DevDataType { dev: u8, data_type: bch_data_type },
    Compression { compression_type: bch_compression_type },
    Snapshot { id: u32 },
    Btree { id: u32 },
    RebalanceWork,
    Inum { inum: u64 },
    ReconcileWork { work_type: bch_reconcile_accounting_type },
    DevLeaving { dev: u32 },
    Unknown(u8),
}

use c::disk_accounting_type;
use disk_accounting_type::*;

/// A single accounting entry from the ioctl.
#[derive(Debug)]
pub struct AccountingEntry {
    pub pos: DiskAccountingPos,
    pub bpos: c::bpos,
    pub counters: Vec<u64>,
}

impl AccountingEntry {
    pub fn counter(&self, i: usize) -> u64 {
        self.counters.get(i).copied().unwrap_or(0)
    }
}

/// Result of query_accounting ioctl.
pub struct AccountingResult {
    pub capacity: u64,
    pub used: u64,
    pub online_reserved: u64,
    pub entries: Vec<AccountingEntry>,
}

/// Convert a bpos to a DiskAccountingPos by byte-reversing the 20-byte bpos
/// (memcpy_swab on little-endian) and parsing the type-tagged union.
fn bpos_to_disk_accounting_pos(p: &c::bpos) -> DiskAccountingPos {
    // bpos is 20 bytes: on little-endian, the accounting pos is the
    // byte-reversed form. We copy to a 20-byte LE array, then reverse all bytes.
    let mut raw = [0u8; 20];

    // Copy bpos fields into raw bytes in memory order (LE: snapshot, offset, inode)
    let snap_bytes = p.snapshot.to_ne_bytes();
    let off_bytes = p.offset.to_ne_bytes();
    let ino_bytes = p.inode.to_ne_bytes();
    raw[0..4].copy_from_slice(&snap_bytes);
    raw[4..12].copy_from_slice(&off_bytes);
    raw[12..20].copy_from_slice(&ino_bytes);

    // memcpy_swab: reverse all 20 bytes
    raw.reverse();

    // Match on raw discriminant — no transmute, unknown types safely fall to Unknown
    const NR_INODES:            u32 = BCH_DISK_ACCOUNTING_nr_inodes as u32;
    const PERSISTENT_RESERVED:  u32 = BCH_DISK_ACCOUNTING_persistent_reserved as u32;
    const REPLICAS:             u32 = BCH_DISK_ACCOUNTING_replicas as u32;
    const DEV_DATA_TYPE:        u32 = BCH_DISK_ACCOUNTING_dev_data_type as u32;
    const COMPRESSION:          u32 = BCH_DISK_ACCOUNTING_compression as u32;
    const SNAPSHOT:             u32 = BCH_DISK_ACCOUNTING_snapshot as u32;
    const BTREE:                u32 = BCH_DISK_ACCOUNTING_btree as u32;
    const REBALANCE_WORK:       u32 = BCH_DISK_ACCOUNTING_rebalance_work as u32;
    const INUM:                 u32 = BCH_DISK_ACCOUNTING_inum as u32;
    const RECONCILE_WORK:       u32 = BCH_DISK_ACCOUNTING_reconcile_work as u32;
    const DEV_LEAVING:          u32 = BCH_DISK_ACCOUNTING_dev_leaving as u32;

    match raw[0] as u32 {
        NR_INODES => DiskAccountingPos::NrInodes,
        PERSISTENT_RESERVED => DiskAccountingPos::PersistentReserved {
            nr_replicas: raw[1],
        },
        REPLICAS => {
            let nr_devs = raw[2];
            let nr_required = raw[3];
            let devs = raw[4..4 + nr_devs as usize].to_vec();
            DiskAccountingPos::Replicas {
                data_type: data_type_from_u8(raw[1]),
                nr_devs, nr_required, devs,
            }
        }
        DEV_DATA_TYPE => DiskAccountingPos::DevDataType {
            dev: raw[1],
            data_type: data_type_from_u8(raw[2]),
        },
        COMPRESSION => DiskAccountingPos::Compression {
            compression_type: compression_type_from_u8(raw[1]),
        },
        SNAPSHOT => {
            let id = u32::from_ne_bytes([raw[1], raw[2], raw[3], raw[4]]);
            DiskAccountingPos::Snapshot { id }
        }
        BTREE => {
            let id = u32::from_ne_bytes([raw[1], raw[2], raw[3], raw[4]]);
            DiskAccountingPos::Btree { id }
        }
        REBALANCE_WORK => DiskAccountingPos::RebalanceWork,
        INUM => {
            let inum = u64::from_ne_bytes([
                raw[1], raw[2], raw[3], raw[4],
                raw[5], raw[6], raw[7], raw[8],
            ]);
            DiskAccountingPos::Inum { inum }
        }
        RECONCILE_WORK => DiskAccountingPos::ReconcileWork {
            work_type: reconcile_type_from_u8(raw[1]),
        },
        DEV_LEAVING => {
            let dev = u32::from_ne_bytes([raw[1], raw[2], raw[3], raw[4]]);
            DiskAccountingPos::DevLeaving { dev }
        }
        _ => DiskAccountingPos::Unknown(raw[0]),
    }
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

        let pos = bpos_to_disk_accounting_pos(&bpos);

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

        entries.push(AccountingEntry { pos, bpos, counters });
        offset += entry_bytes;
    }

    entries
}

/// Free/empty data types — not counted as "used" space.
pub fn data_type_is_empty(t: bch_data_type) -> bool {
    matches!(t, BCH_DATA_free | BCH_DATA_need_gc_gens | BCH_DATA_need_discard)
}

/// Internal/hidden data types — not user-visible (superblock, journal).
pub fn data_type_is_hidden(t: bch_data_type) -> bool {
    matches!(t, BCH_DATA_sb | BCH_DATA_journal)
}

/// Print a data type directly into a Printbuf via bch2_prt_data_type.
pub fn prt_data_type(out: &mut Printbuf, t: bch_data_type) {
    unsafe { c::bch2_prt_data_type(out.as_raw(), t) }
}

/// Print a compression type directly into a Printbuf via bch2_prt_compression_type.
pub fn prt_compression_type(out: &mut Printbuf, t: bch_compression_type) {
    unsafe { c::bch2_prt_compression_type(out.as_raw(), t) }
}

/// Print a reconcile accounting type directly into a Printbuf.
pub fn prt_reconcile_type(out: &mut Printbuf, t: bch_reconcile_accounting_type) {
    unsafe { c::bch2_prt_reconcile_accounting_type(out.as_raw(), t) }
}

/// Get a btree ID name string.
pub fn btree_id_str(id: u32) -> String {
    if id < c::btree_id::BTREE_ID_NR as u32 {
        // SAFETY: id is in [0, BTREE_ID_NR), a valid discriminant
        let btree_id: c::btree_id = unsafe { std::mem::transmute(id) };
        format!("{}", btree_id)
    } else {
        format!("(unknown btree {})", id)
    }
}

/// Get a member state string.
pub fn member_state_str(state: u8) -> &'static str {
    bch_bindgen::sb::member_state_str(state)
}
