use crate::c;
use crate::printbuf::Printbuf;

pub use c::bch_data_type;
pub use c::bch_compression_type;
pub use c::bch_reconcile_accounting_type;

use bch_data_type::*;

/// Safely convert a raw u8 to a bindgen #[repr(u32)] enum.
/// Out-of-range values return the NR sentinel.
macro_rules! enum_from_u8 {
    ($ty:ty, $nr:expr, $v:expr) => {{
        let v = $v as u32;
        if v < $nr as u32 {
            // SAFETY: v is in [0, NR), all valid #[repr(u32)] discriminants
            unsafe { std::mem::transmute::<u32, $ty>(v) }
        } else {
            $nr
        }
    }}
}

pub fn data_type_from_u8(v: u8) -> bch_data_type {
    enum_from_u8!(bch_data_type, BCH_DATA_NR, v)
}

pub fn compression_type_from_u8(v: u8) -> bch_compression_type {
    enum_from_u8!(bch_compression_type, bch_compression_type::BCH_COMPRESSION_TYPE_NR, v)
}

pub fn reconcile_type_from_u8(v: u8) -> bch_reconcile_accounting_type {
    enum_from_u8!(bch_reconcile_accounting_type, bch_reconcile_accounting_type::BCH_RECONCILE_ACCOUNTING_NR, v)
}

/// Size of a bpos in bytes — maximum size of any accounting key payload.
const BPOS_SIZE: usize = std::mem::size_of::<c::bpos>();

/// A bpos encoding a disk accounting key position.
///
/// Same size and ABI as bpos (`#[repr(transparent)]`). The accounting type
/// and variant fields are encoded in the bpos bytes (byte-reversed on LE).
/// Use `decode()` to parse into `DiskAccountingKind` for pattern matching.
#[repr(transparent)]
#[derive(Clone, Copy, Debug)]
pub struct DiskAccountingPos(pub c::bpos);

impl DiskAccountingPos {
    /// Wrap a raw bpos as an accounting position.
    pub fn from_bpos(p: c::bpos) -> Self {
        Self(p)
    }

    /// The underlying bpos, for passing to btree/ioctl APIs.
    #[allow(dead_code)]
    pub fn as_bpos(&self) -> c::bpos {
        self.0
    }

    /// Decode into the typed enum for pattern matching.
    pub fn decode(&self) -> DiskAccountingKind {
        bpos_to_accounting_kind(&self.0)
    }

    /// Extract the accounting type byte without full decode.
    /// On LE, this is the high byte of bpos.inode (equivalent to raw[0]
    /// after the 20-byte memcpy_swab reversal).
    fn type_byte(&self) -> u8 {
        (self.0.inode >> 56) as u8
    }

    /// Get the accounting type discriminant without full decode.
    pub fn accounting_type(&self) -> Option<disk_accounting_type> {
        let t = self.type_byte() as u32;
        if t < BCH_DISK_ACCOUNTING_TYPE_NR as u32 {
            Some(unsafe { std::mem::transmute(t) })
        } else {
            None
        }
    }
}

impl PartialEq for DiskAccountingPos {
    fn eq(&self, other: &Self) -> bool { self.0 == other.0 }
}
impl Eq for DiskAccountingPos {}

impl PartialOrd for DiskAccountingPos {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for DiskAccountingPos {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

/// Decoded accounting key — the typed form for pattern matching.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum DiskAccountingKind {
    NrInodes,
    PersistentReserved { nr_replicas: u8 },
    Replicas { data_type: bch_data_type, nr_devs: u8, nr_required: u8, devs: [u8; BPOS_SIZE] },
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

// Compile-time check: update DiskAccountingKind when new disk_accounting_type values are added.
const _: () = assert!(BCH_DISK_ACCOUNTING_TYPE_NR as u32 == 11);

impl DiskAccountingKind {
    /// Encode into a DiskAccountingPos (reverse of decode).
    #[allow(dead_code)]
    pub fn encode(&self) -> DiskAccountingPos {
        let mut raw = [0u8; BPOS_SIZE];
        match *self {
            Self::NrInodes => {
                raw[0] = BCH_DISK_ACCOUNTING_nr_inodes as u8;
            }
            Self::PersistentReserved { nr_replicas } => {
                raw[0] = BCH_DISK_ACCOUNTING_persistent_reserved as u8;
                raw[1] = nr_replicas;
            }
            Self::Replicas { data_type, nr_devs, nr_required, devs } => {
                raw[0] = BCH_DISK_ACCOUNTING_replicas as u8;
                raw[1] = data_type as u8;
                raw[2] = nr_devs;
                raw[3] = nr_required;
                let n = (nr_devs as usize).min(BPOS_SIZE - 4);
                raw[4..4 + n].copy_from_slice(&devs[..n]);
            }
            Self::DevDataType { dev, data_type } => {
                raw[0] = BCH_DISK_ACCOUNTING_dev_data_type as u8;
                raw[1] = dev;
                raw[2] = data_type as u8;
            }
            Self::Compression { compression_type } => {
                raw[0] = BCH_DISK_ACCOUNTING_compression as u8;
                raw[1] = compression_type as u8;
            }
            Self::Snapshot { id } => {
                raw[0] = BCH_DISK_ACCOUNTING_snapshot as u8;
                raw[1..5].copy_from_slice(&id.to_ne_bytes());
            }
            Self::Btree { id } => {
                raw[0] = BCH_DISK_ACCOUNTING_btree as u8;
                raw[1..5].copy_from_slice(&id.to_ne_bytes());
            }
            Self::RebalanceWork => {
                raw[0] = BCH_DISK_ACCOUNTING_rebalance_work as u8;
            }
            Self::Inum { inum } => {
                raw[0] = BCH_DISK_ACCOUNTING_inum as u8;
                raw[1..9].copy_from_slice(&inum.to_ne_bytes());
            }
            Self::ReconcileWork { work_type } => {
                raw[0] = BCH_DISK_ACCOUNTING_reconcile_work as u8;
                raw[1] = work_type as u8;
            }
            Self::DevLeaving { dev } => {
                raw[0] = BCH_DISK_ACCOUNTING_dev_leaving as u8;
                raw[1..5].copy_from_slice(&dev.to_ne_bytes());
            }
            Self::Unknown(t) => {
                raw[0] = t;
            }
        }

        // Reverse memcpy_swab: reverse bytes back into bpos layout
        raw.reverse();

        DiskAccountingPos(c::bpos {
            snapshot: u32::from_ne_bytes(raw[0..4].try_into().unwrap()),
            offset:   u64::from_ne_bytes(raw[4..12].try_into().unwrap()),
            inode:    u64::from_ne_bytes(raw[12..20].try_into().unwrap()),
        })
    }
}

/// A single accounting entry (from ioctl or btree iteration).
#[derive(Debug)]
pub struct AccountingEntry {
    pub pos: DiskAccountingPos,
    pub counters: Vec<u64>,
}

impl AccountingEntry {
    pub fn counter(&self, i: usize) -> u64 {
        self.counters.get(i).copied().unwrap_or(0)
    }
}

/// Decode a bpos into a DiskAccountingKind by byte-reversing the 20-byte bpos
/// (memcpy_swab on little-endian) and parsing the type-tagged union.
fn bpos_to_accounting_kind(p: &c::bpos) -> DiskAccountingKind {
    // bpos is 20 bytes: on little-endian, the accounting pos is the
    // byte-reversed form. We copy to a 20-byte LE array, then reverse all bytes.
    let mut raw = [0u8; BPOS_SIZE];

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
        NR_INODES => DiskAccountingKind::NrInodes,
        PERSISTENT_RESERVED => DiskAccountingKind::PersistentReserved {
            nr_replicas: raw[1],
        },
        REPLICAS => {
            let nr_devs = raw[2];
            let nr_required = raw[3];
            let mut devs = [0u8; BPOS_SIZE];
            let n = (nr_devs as usize).min(BPOS_SIZE - 4);
            devs[..n].copy_from_slice(&raw[4..4 + n]);
            DiskAccountingKind::Replicas {
                data_type: data_type_from_u8(raw[1]),
                nr_devs, nr_required, devs,
            }
        }
        DEV_DATA_TYPE => DiskAccountingKind::DevDataType {
            dev: raw[1],
            data_type: data_type_from_u8(raw[2]),
        },
        COMPRESSION => DiskAccountingKind::Compression {
            compression_type: compression_type_from_u8(raw[1]),
        },
        SNAPSHOT => {
            let id = u32::from_ne_bytes([raw[1], raw[2], raw[3], raw[4]]);
            DiskAccountingKind::Snapshot { id }
        }
        BTREE => {
            let id = u32::from_ne_bytes([raw[1], raw[2], raw[3], raw[4]]);
            DiskAccountingKind::Btree { id }
        }
        REBALANCE_WORK => DiskAccountingKind::RebalanceWork,
        INUM => {
            let inum = u64::from_ne_bytes([
                raw[1], raw[2], raw[3], raw[4],
                raw[5], raw[6], raw[7], raw[8],
            ]);
            DiskAccountingKind::Inum { inum }
        }
        RECONCILE_WORK => DiskAccountingKind::ReconcileWork {
            work_type: reconcile_type_from_u8(raw[1]),
        },
        DEV_LEAVING => {
            let dev = u32::from_ne_bytes([raw[1], raw[2], raw[3], raw[4]]);
            DiskAccountingKind::DevLeaving { dev }
        }
        _ => DiskAccountingKind::Unknown(raw[0]),
    }
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
    crate::sb::member_state_str(state)
}
