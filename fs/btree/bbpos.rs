#![allow(non_camel_case_types)]

use core::cmp::Ordering;
use cstruct_macros::{MaybeZeroable, Plain, bitfield, BitfieldMembers};
use nestify::nest;
use crate::format::{Bpos, Btree};

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain)]
pub struct Bbpos {
    pub btree: Btree,
    pub pos: Bpos,
}

// Torn, a mix of two positions - not necessarily between them: harmless in
// what reads one racily, a walk's progress, shown
impl crate::util::projections::TearSafe for Bbpos {}

/*
 * Layout is padding-free (bpos is __packed __aligned(4)), so a Blbpos can be
 * used directly as a memcmp/rhashtable key.
 */
nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, MaybeZeroable, BitfieldMembers)]
    pub struct Blbpos {
        #[members]
        #>[bitfield(u16)]
        pub bits: pub struct BlbposBits {
            #[bits(16)] pub btree: Btree,
        },
        pub level: u16,
        pub pos: Bpos,
    }
}

#[cfg(feature = "std")]
use crate::errcode::{Errcode, BchError};
#[cfg(feature = "std")]
use core::str::FromStr;

impl PartialEq for Bbpos {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Bbpos {}

impl PartialOrd for Bbpos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bbpos {
    fn cmp(&self, other: &Self) -> Ordering {
        self.btree.0.cmp(&{ other.btree.0 })
            .then(self.pos.cmp(&other.pos))
    }
}

impl Bbpos {
    pub const MIN: Self = Bbpos { btree: Btree(0), pos: crate::btree::bkey::POS_MIN };
    /// The last of Btree::NR - 1.
    pub const MAX: Self = Bbpos {
        btree: Btree(Btree::NR as u32 - 1),
        pos:   crate::btree::bkey::SPOS_MAX,
    };

    /// Past the last btree's last, panics.
    pub fn successor(self) -> Self {
        if self.pos != crate::btree::bkey::SPOS_MAX {
            return Bbpos { pos: self.pos.successor(), ..self };
        }

        assert!(self.btree.0 != Btree::NR as u32, "no position after the last btree's last");
        Bbpos { btree: Btree(self.btree.0 + 1), pos: crate::btree::bkey::POS_MIN }
    }
}

/// btree:pos, as bch2_bbpos_to_text().
impl crate::util::ToText for Bbpos {
    fn to_text(&self, out: &mut crate::util::Printbuf) {
        crate::prt!(out, "{}:{}", self.btree, self.pos);
    }
}
crate::display_via_to_text!(Bbpos);

#[cfg(feature = "std")]
impl FromStr for Bbpos {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || BchError::from(Errcode::EINVAL_parse_bbpos);

        let (btree_s, pos_s) = s.split_once(':').ok_or_else(err)?;

        let btree: Btree = btree_s.parse().map_err(|_| err())?;
        let pos: Bpos = pos_s.parse().map_err(|_| err())?;

        Ok(Bbpos { btree, pos })
    }
}

/// A range of btree positions (start..=end).
#[cfg(feature = "std")]
#[derive(Clone, Copy)]
pub struct BbposRange {
    pub start: Bbpos,
    pub end:   Bbpos,
}

/// Parse a bbpos range string "start-end" or just "pos" (start == end).
#[cfg(feature = "std")]
pub fn bbpos_range_parse(s: &str) -> Result<BbposRange, BchError> {
    let (start_s, end_s) = match s.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };

    let start: Bbpos = start_s.parse()?;
    let end: Bbpos = match end_s {
        Some(e) => e.parse()?,
        None => start,
    };

    Ok(BbposRange { start, end })
}
