use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::util::vstructs::FlexArray;

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

/// The disk_accounting_pos an accounting key at @p holds, as bytes: the
/// position as one big-endian number - inode, offset, snapshot - so the first
/// byte, the inode's top one, is the accounting type. As
/// bpos_to_disk_accounting_pos(): memcpy_swab() on little endian, a copy on
/// big - the same bytes, as a bpos's fields are in the opposite order there.
fn bpos_to_acc_bytes(p: c::bpos) -> [u8; BPOS_SIZE] {
    let mut b = [0u8; BPOS_SIZE];
    b[0..8].copy_from_slice(&p.inode.to_be_bytes());
    b[8..16].copy_from_slice(&p.offset.to_be_bytes());
    b[16..20].copy_from_slice(&p.snapshot.to_be_bytes());
    b
}

/// The bpos of the accounting key holding disk_accounting_pos bytes @b:
/// bpos_to_acc_bytes()'s inverse.
fn acc_bytes_to_bpos(b: &[u8; BPOS_SIZE]) -> c::bpos {
    c::bpos {
        inode:    u64::from_be_bytes(b[0..8].try_into().unwrap()),
        offset:   u64::from_be_bytes(b[8..16].try_into().unwrap()),
        snapshot: u32::from_be_bytes(b[16..20].try_into().unwrap()),
    }
}

/// An accounting key's position is a disk_accounting_pos - see
/// accounting_format.rs: get() for the arm it holds, from_arm() to make one.
impl c::disk_accounting_pos {
    /// The disk_accounting_pos an accounting key at @p holds:
    /// bpos_to_disk_accounting_pos().
    pub fn from_bpos(p: c::bpos) -> Self {
        Self::from_bytes(&bpos_to_acc_bytes(p))
    }

    /// The position of the accounting key holding it:
    /// disk_accounting_pos_to_bpos().
    pub fn to_bpos(&self) -> c::bpos {
        acc_bytes_to_bpos(self.as_bytes())
    }
}

/// replicas' devices: devs[], nr_devs long.
impl FlexArray for c::bch_replicas_entry_v1 {
    type Elem = u8;
    const TAIL: usize = core::mem::offset_of!(c::bch_replicas_entry_v1, devs);

    fn nr(&self) -> usize {
        self.nr_devs as usize
    }
}

pub fn mem_read(fs: &Fs, pos: &c::disk_accounting_pos, counters: &mut [u64]) {
    unsafe {
        c::bch2_accounting_mem_read(
            fs.raw,
            pos.to_bpos(),
            counters.as_mut_ptr(),
            counters.len() as u32,
        );
    }
}

/// Add @d to accounting key @pos's counters, in @trans - the gc copy's, with
/// @gc: as bch2_disk_accounting_mod(). For triggers.
pub fn add(trans: &BtreeTrans<'_>, pos: &c::disk_accounting_pos, d: &[i64], gc: bool)
    -> Result<(), BchError>
{
    // C's signature isn't const: a copy of @pos, and @d only read.
    let mut acc = c::disk_accounting_pos::from_bytes(pos.as_bytes());
    ret_to_result(unsafe {
        c::bch2_disk_accounting_mod(trans.raw(), &mut acc, d.as_ptr() as *mut i64,
                                    d.len() as u32, gc)
    })
}

pub fn nr_inodes(fs: &Fs) -> u64 {
    let mut nr_inodes = 0;
    mem_read(
        fs,
        &c::disk_accounting_pos::from_arm(c::bch_acct_nr_inodes {}),
        core::slice::from_mut(&mut nr_inodes),
    );
    nr_inodes
}

/// A single accounting entry (from ioctl or btree iteration). Tools-only —
/// holds its counters in a heap Vec.
#[cfg(feature = "std")]
pub struct AccountingEntry {
    pub pos: c::disk_accounting_pos,
    pub counters: Vec<u64>,
}

#[cfg(feature = "std")]
impl AccountingEntry {
    pub fn counter(&self, i: usize) -> u64 {
        self.counters.get(i).copied().unwrap_or(0)
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
