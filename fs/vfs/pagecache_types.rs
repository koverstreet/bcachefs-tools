// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]
// The VFS's: kernel only, as C's #ifndef NO_BCACHEFS_FS
#![cfg(not(NO_BCACHEFS_FS))]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::{bitfield, c_enum, c_typedef, c_xmacro, CStruct, TypeInfo};
use nestify::nest;

c_typedef! {
    #[c("DARRAY(struct folio *) folios")]
    pub type folios = DArray<*mut c::folio>;
}

c_xmacro! {
    BCH_FOLIO_SECTOR_STATE(x) {
        (unallocated),
        (reserved),
        (dirty),
        (dirty_reserved),
        (allocated),
    }
}

c_enum! {
    #[closed]
    pub enum bch_folio_sector_state: u32 {
        BCH_FOLIO_SECTOR_STATE(n) => SECTOR_ ## n,
    }
}

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct, TypeInfo)]
    pub struct bch_folio_sector {
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct bch_folio_sector_bits {
            /* Uncompressed, fully allocated replicas (or on disk reservation): */
            nr_replicas:       u4,
            /* Owns PAGE_SECTORS * replicas_reserved sized in memory reservation: */
            replicas_reserved: u4,
        },
        pub state: u8,
    }
}

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
c_default!(bch_folio);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct bch2_folio_reservation {
    pub disk: c::disk_reservation,
    pub quota: c::quota_res,
    /* @disk fell back to fewer replicas than the inode asks for */
    pub degraded: bool,
}
