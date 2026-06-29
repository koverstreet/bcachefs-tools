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
        (self.btree as u32).cmp(&(other.btree as u32))
            .then(self.pos.cmp(&other.pos))
    }
}

#[cfg(feature = "std")]
impl FromStr for c::bbpos {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || BchError::from_errcode(bch_errcode::BCH_ERR_EINVAL_parse_bbpos);

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
