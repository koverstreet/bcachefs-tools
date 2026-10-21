use crate::c;
use core::cmp::Ordering;
#[cfg(feature = "std")]
use crate::errcode::{bch_errcode, BchError};
#[cfg(feature = "std")]
use core::str::FromStr;

impl PartialEq for c::bbpos {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for c::bbpos {}

impl PartialOrd for c::bbpos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for c::bbpos {
    fn cmp(&self, other: &Self) -> Ordering {
        self.btree.0.cmp(&{ other.btree.0 })
            .then(self.pos.cmp(&other.pos))
    }
}

impl c::bbpos {
    pub const MIN: Self = c::bbpos { btree: c::btree_id(0), pos: crate::btree::bkey::POS_MIN };
    /// The last of BTREE_ID_NR - 1.
    pub const MAX: Self = c::bbpos {
        btree: c::btree_id(c::BTREE_ID_NR as u32 - 1),
        pos:   crate::btree::bkey::SPOS_MAX,
    };

    /// Past the last btree's last, panics.
    pub fn successor(self) -> Self {
        if self.pos != crate::btree::bkey::SPOS_MAX {
            return c::bbpos { pos: self.pos.successor(), ..self };
        }

        assert!(self.btree.0 != c::BTREE_ID_NR as u32, "no position after the last btree's last");
        c::bbpos { btree: c::btree_id(self.btree.0 + 1), pos: crate::btree::bkey::POS_MIN }
    }
}

/// btree:pos, as bch2_bbpos_to_text().
impl crate::util::ToText for c::bbpos {
    fn to_text(&self, out: &mut crate::util::Printbuf) {
        unsafe { c::bch2_btree_id_to_text(out.as_raw(), self.btree) };
        crate::prt!(out, ":{}", self.pos);
    }
}
crate::display_via_to_text!(c::bbpos);

#[cfg(feature = "std")]
impl FromStr for c::bbpos {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || BchError::from(bch_errcode::BCH_ERR_EINVAL_parse_bbpos);

        let (btree_s, pos_s) = s.split_once(':').ok_or_else(err)?;

        let btree: c::btree_id = btree_s.parse().map_err(|_| err())?;
        let pos: c::bpos = pos_s.parse().map_err(|_| err())?;

        Ok(c::bbpos { btree, pos })
    }
}

/// A range of btree positions (start..=end).
#[cfg(feature = "std")]
#[derive(Clone, Copy)]
pub struct BbposRange {
    pub start: c::bbpos,
    pub end:   c::bbpos,
}

/// Parse a bbpos range string "start-end" or just "pos" (start == end).
#[cfg(feature = "std")]
pub fn bbpos_range_parse(s: &str) -> Result<BbposRange, BchError> {
    let (start_s, end_s) = match s.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };

    let start: c::bbpos = start_s.parse()?;
    let end: c::bbpos = match end_s {
        Some(e) => e.parse()?,
        None => start,
    };

    Ok(BbposRange { start, end })
}
