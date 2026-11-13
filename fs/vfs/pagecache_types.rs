// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]
// The VFS's: kernel only, as C's #ifndef NO_BCACHEFS_FS
#![cfg(not(NO_BCACHEFS_FS))]

use crate::cstructs::c;
use crate::locking::Locked;
use crate::types::c_default;
use cstruct_macros::{bitfield, c_enum, CStruct, TypeInfo};
use nestify::nest;
use core::sync::atomic::AtomicI32;

c_enum! {
    #[closed]
    #[prefix(SECTOR_)]
    pub enum bch_folio_sector_state: u32 {
        unallocated,
        reserved,
        dirty,
        dirty_reserved,
        allocated,
    }
}

impl bch_folio_sector_state {
    pub fn dirty(self) -> Self {
        match self {
            Self::unallocated => Self::dirty,
            Self::reserved    => Self::dirty_reserved,
            s                 => s,
        }
    }

    pub fn undirty(self) -> Self {
        match self {
            Self::dirty          => Self::unallocated,
            Self::dirty_reserved => Self::reserved,
            s                    => s,
        }
    }

    pub fn reserve(self) -> Self {
        match self {
            Self::unallocated => Self::reserved,
            Self::dirty       => Self::dirty_reserved,
            s                 => s,
        }
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
        /*
         * FIXME: a bch_folio_sector_state, once buffered.c is Rust: C can't
         * have it as one - an enum bitfield takes the enum's alignment, and
         * would make this 4 bytes, not 2.
         */
        pub state: u8,
    }
}

/*
 * Worst-case bytes for a per-folio bch_folio_sector snapshot: a folio can be up
 * to MAX_PAGECACHE_ORDER pages, each contributing PAGE_SECTORS sector entries.
 * Used to size the writepage_buf_pool reserve.
 */
pub const BCH_WRITEPAGE_BUF_BYTES: usize =
    ((crate::util::kernel::PAGE_SIZE >> 9) << crate::c::MAX_PAGECACHE_ORDER) * core::mem::size_of::<c::bch_folio_sector>();

/// A folio's sector state, under its bch_folio's lock: written by writeback
/// completion with the folio unlocked, so the folio lock doesn't cover it.
/// s[] has a sector of the folio each.
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct FolioState {
    /* is s[] up to date with the btree? says nothing about the data */
    pub state_uptodate: bool,
    /* the count s[].replicas_reserved is charged at, and released at */
    pub replicas_reserved_at: u8,
    /*
     * A foreground reservation fell back: writeback shouldn't insist on the
     * inode's count - see Folio::reserve_writeback().
     */
    pub reserved_degraded: bool,
    pub s: [c::bch_folio_sector; 0],
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_folio {
    pub write_count: AtomicI32,
    /*
     * The data: sectors [0, partially_uptodate) are read but the folio
     * isn't uptodate. One offset suffices because reads start at the front
     * of the folio; 0 means nothing partial, so only
     * readpage_bio_drop_unissued() maintains this.
     */
    pub partially_uptodate: u16,
    #[c_lock("spinlock_t lock")]
    pub state: Locked<c::spinlock_t, FolioState>,
}
c_default!(bch_folio);
