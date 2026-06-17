use crate::c;
use crate::fs::Fs;

pub use c::bch_data_type;
pub use c::bch_compression_type;
pub use c::bch_reconcile_accounting_type;
#[allow(non_camel_case_types)]
pub type data_type = c::bch_data_type;
#[allow(non_camel_case_types)]
pub type compression_type = c::bch_compression_type;
#[allow(non_camel_case_types)]
pub type disk_accounting_type = c::disk_accounting_type;
#[allow(non_camel_case_types)]
pub type reconcile_accounting_type = c::bch_reconcile_accounting_type;

pub fn data_type_from_u8(v: u8) -> bch_data_type {
    bch_data_type(v as u32)
}

pub fn compression_type_from_u8(v: u8) -> bch_compression_type {
    bch_compression_type(v as u32)
}

pub fn reconcile_type_from_u8(v: u8) -> bch_reconcile_accounting_type {
    bch_reconcile_accounting_type(v as u32)
}

/// Size of a bpos in bytes — maximum size of any accounting key payload.
const BPOS_SIZE: usize = core::mem::size_of::<c::bpos>();

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
        if t < u32::from(disk_accounting_type::nr) {
            Some(c::disk_accounting_type(t))
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
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DiskAccountingPos {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
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

// Compile-time check: update DiskAccountingKind when new disk_accounting_type values are added.
const _: () = assert!(disk_accounting_type::nr.0 == 11);

impl DiskAccountingKind {
    /// Encode into a DiskAccountingPos (reverse of decode).
    #[allow(dead_code)]
    pub fn encode(&self) -> DiskAccountingPos {
        let mut raw = [0u8; BPOS_SIZE];
        match *self {
            Self::NrInodes => {
                raw[0] = disk_accounting_type::nr_inodes.0 as u8;
            }
            Self::PersistentReserved { nr_replicas } => {
                raw[0] = disk_accounting_type::persistent_reserved.0 as u8;
                raw[1] = nr_replicas;
            }
            Self::Replicas { data_type, nr_devs, nr_required, devs } => {
                raw[0] = disk_accounting_type::replicas.0 as u8;
                raw[1] = data_type.0 as u8;
                raw[2] = nr_devs;
                raw[3] = nr_required;
                let n = (nr_devs as usize).min(BPOS_SIZE - 4);
                raw[4..4 + n].copy_from_slice(&devs[..n]);
            }
            Self::DevDataType { dev, data_type } => {
                raw[0] = disk_accounting_type::dev_data_type.0 as u8;
                raw[1] = dev;
                raw[2] = data_type.0 as u8;
            }
            Self::Compression { compression_type } => {
                raw[0] = disk_accounting_type::compression.0 as u8;
                raw[1] = compression_type.0 as u8;
            }
            Self::Snapshot { id } => {
                raw[0] = disk_accounting_type::snapshot.0 as u8;
                raw[1..5].copy_from_slice(&id.to_ne_bytes());
            }
            Self::Btree { id } => {
                raw[0] = disk_accounting_type::btree.0 as u8;
                raw[1..5].copy_from_slice(&id.to_ne_bytes());
            }
            Self::RebalanceWork => {
                raw[0] = disk_accounting_type::rebalance_work.0 as u8;
            }
            Self::Inum { inum } => {
                raw[0] = disk_accounting_type::inum.0 as u8;
                raw[1..9].copy_from_slice(&inum.to_ne_bytes());
            }
            Self::ReconcileWork { work_type } => {
                raw[0] = disk_accounting_type::reconcile_work.0 as u8;
                raw[1] = work_type.0 as u8;
            }
            Self::DevLeaving { dev } => {
                raw[0] = disk_accounting_type::dev_leaving.0 as u8;
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

pub fn mem_read(fs: &Fs, pos: DiskAccountingPos, counters: &mut [u64]) {
    unsafe {
        c::bch2_accounting_mem_read(
            fs.raw,
            pos.as_bpos(),
            counters.as_mut_ptr(),
            counters.len() as u32,
        );
    }
}

pub fn nr_inodes(fs: &Fs) -> u64 {
    let mut nr_inodes = 0;
    mem_read(
        fs,
        DiskAccountingKind::NrInodes.encode(),
        core::slice::from_mut(&mut nr_inodes),
    );
    nr_inodes
}

/// A single accounting entry (from ioctl or btree iteration). Tools-only —
/// holds its counters in a heap Vec.
#[cfg(feature = "std")]
#[derive(Debug)]
pub struct AccountingEntry {
    pub pos: DiskAccountingPos,
    pub counters: Vec<u64>,
}

#[cfg(feature = "std")]
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
    const NR_INODES:            u32 = disk_accounting_type::nr_inodes.0;
    const PERSISTENT_RESERVED:  u32 = disk_accounting_type::persistent_reserved.0;
    const REPLICAS:             u32 = disk_accounting_type::replicas.0;
    const DEV_DATA_TYPE:        u32 = disk_accounting_type::dev_data_type.0;
    const COMPRESSION:          u32 = disk_accounting_type::compression.0;
    const SNAPSHOT:             u32 = disk_accounting_type::snapshot.0;
    const BTREE:                u32 = disk_accounting_type::btree.0;
    const REBALANCE_WORK:       u32 = disk_accounting_type::rebalance_work.0;
    const INUM:                 u32 = disk_accounting_type::inum.0;
    const RECONCILE_WORK:       u32 = disk_accounting_type::reconcile_work.0;
    const DEV_LEAVING:          u32 = disk_accounting_type::dev_leaving.0;

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
    t == data_type::free
        || t == data_type::need_gc_gens
        || t == data_type::need_discard
}

/// Internal/hidden data types — not user-visible (superblock, journal).
pub fn data_type_is_hidden(t: bch_data_type) -> bool {
    t == data_type::sb || t == data_type::journal
}
