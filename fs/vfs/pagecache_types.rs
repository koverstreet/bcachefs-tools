// SPDX-License-Identifier: GPL-2.0

//! The data types of vfs/pagecache_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::{bitfield, c_enum, c_typedef, c_xmacro, CStruct};
use typeinfo_macros::TypeInfo;

#[cfg(not(NO_BCACHEFS_FS))]
#[cfg(not(NO_BCACHEFS_FS))]
c_typedef! {
    #[c("DARRAY(struct folio *) folios")]
    pub type folios = DArray<*mut c::folio>;
}

#[cfg(not(NO_BCACHEFS_FS))]
c_xmacro! {
    BCH_FOLIO_SECTOR_STATE(x) {
        (unallocated),
        (reserved),
        (dirty),
        (dirty_reserved),
        (allocated),
    }
}

#[cfg(not(NO_BCACHEFS_FS))]
macro_rules! __bch_folio_sector_state_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum bch_folio_sector_state: u32 {
                $($acc)*
                $([<SECTOR_ $n>],)*
            }
        }
    } };
}
#[cfg(not(NO_BCACHEFS_FS))]
BCH_FOLIO_SECTOR_STATE!(__bch_folio_sector_state_0 []);

#[cfg(not(NO_BCACHEFS_FS))]
#[bitfield(u8)]
pub struct bch_folio_sector_nr_replicas_bits {
    /* Uncompressed, fully allocated replicas (or on disk reservation): */
    #[bits(4)]
    pub nr_replicas: u8,
    /* Owns PAGE_SECTORS * replicas_reserved sized in memory reservation: */
    #[bits(4)]
    pub replicas_reserved: u8,
}

#[cfg(not(NO_BCACHEFS_FS))]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_folio_sector {
    #[c_bitfield]
    pub nr_replicas_bits: bch_folio_sector_nr_replicas_bits,
    pub state: u8,
}
#[cfg(not(NO_BCACHEFS_FS))]
impl bch_folio_sector {
    pub fn nr_replicas(&self) -> u8 { let b = self.nr_replicas_bits; b.nr_replicas() }
    pub fn set_nr_replicas(&mut self, v: u8) { let mut b = self.nr_replicas_bits; b.set_nr_replicas(v); self.nr_replicas_bits = b; }
    pub fn replicas_reserved(&self) -> u8 { let b = self.nr_replicas_bits; b.replicas_reserved() }
    pub fn set_replicas_reserved(&mut self, v: u8) { let mut b = self.nr_replicas_bits; b.set_replicas_reserved(v); self.nr_replicas_bits = b; }
}

#[cfg(not(NO_BCACHEFS_FS))]
#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_folio {
    pub lock: c::spinlock_t,
    pub write_count: c::atomic_t,
    /* is s[] up to date with the btree? says nothing about the data */
    pub state_uptodate: bool,
    /* the count s[].replicas_reserved is charged at, and released at */
    pub replicas_reserved_at: u8,
    /*
     * A foreground reservation fell back: writeback shouldn't insist on the
     * inode's count - see bch2_get_folio_disk_reservation().
     */
    pub reserved_degraded: bool,
    /*
     * The data: sectors [0, partially_uptodate) are read but the folio
     * isn't uptodate. One offset suffices because reads start at the front
     * of the folio; 0 means nothing partial, so only
     * readpage_bio_drop_unissued() maintains this.
     */
    pub partially_uptodate: u16,
    pub s: [c::bch_folio_sector; 0],
}
#[cfg(not(NO_BCACHEFS_FS))]
c_default!(bch_folio);

#[cfg(not(NO_BCACHEFS_FS))]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct bch2_folio_reservation {
    pub disk: c::disk_reservation,
    pub quota: c::quota_res,
    /* @disk fell back to fewer replicas than the inode asks for */
    pub degraded: bool,
}
