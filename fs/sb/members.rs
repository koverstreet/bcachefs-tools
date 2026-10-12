use crate::c;
use crate::util::vstructs::View;
use core::mem::offset_of;
use super::io::*;

// ---------------------------------------------------------------------------
// Members — bounds-checked readers for bch_sb_field_members_v2 and v1
//
// Members are variable-size: each entry is `member_bytes` wide, which may
// be smaller than `sizeof(bch_member)` for on-disk backward compatibility.
// The reader copies into a zeroed struct (like the C `bch2_members_v2_get`);
// to edit one, bch_sb_handle::member_mut(), for a member that's all there.
// ---------------------------------------------------------------------------

// Member state names, by value: from BCH_MEMBER_STATES()
macro_rules! member_state_names {
    ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => {
        pub const MEMBER_STATE_NAMES: &[&str] = &[$(stringify!($name),)*];
    };
}
c::BCH_MEMBER_STATES!(member_state_names);

pub fn member_state_str(state: u8) -> &'static str {
    MEMBER_STATE_NAMES.get(state as usize).copied().unwrap_or("unknown")
}

const BCH_MEMBER_V1_BYTES: usize = 56;

/// Member @idx of a members section, its bytes @b: one of @member_bytes from
/// @start, copied - zero past the end of one shorter than bch_member. None
/// if it's past the section's end.
fn member_at(b: &[u8], start: usize, member_bytes: usize, idx: u32) -> Option<c::bch_member> {
    let at = start + idx as usize * member_bytes;
    Some(View::<c::bch_member>::new(b.get(at..at + member_bytes)?).get())
}

/// Read-only view of members_v2 with bounds-checked access.
pub struct MembersV2<'a> {
    /// The section's
    bytes: &'a [u8],
    member_bytes: usize,
    nr_devices: u32,
}

impl<'a> MembersV2<'a> {
    /// Get a copy of the member at `idx`, zero-extending if member_bytes
    /// is smaller than sizeof(bch_member).
    pub fn get(&self, idx: u32) -> Option<c::bch_member> {
        if idx >= self.nr_devices {
            return None;
        }
        member_at(self.bytes, offset_of!(c::bch_sb_field_members_v2, _members), self.member_bytes, idx)
    }

    pub fn member_bytes(&self) -> usize {
        self.member_bytes
    }

    pub fn nr_devices(&self) -> u32 {
        self.nr_devices
    }

    pub fn iter(&self) -> impl Iterator<Item = c::bch_member> + '_ {
        (0..self.nr_devices).filter_map(|i| self.get(i))
    }
}

/// Read-only view of members_v1 with bounds-checked access.
pub struct MembersV1<'a> {
    /// The section's
    bytes: &'a [u8],
    nr_devices: u32,
}

impl<'a> MembersV1<'a> {
    pub fn get(&self, idx: u32) -> Option<c::bch_member> {
        if idx >= self.nr_devices {
            return None;
        }
        member_at(self.bytes, offset_of!(c::bch_sb_field_members_v1, _members), BCH_MEMBER_V1_BYTES, idx)
    }

    pub fn nr_devices(&self) -> u32 {
        self.nr_devices
    }

    pub fn iter(&self) -> impl Iterator<Item = c::bch_member> + '_ {
        (0..self.nr_devices).filter_map(|i| self.get(i))
    }
}

/// Construct a MembersV2 reader from a superblock.
pub fn members_v2(sb: &c::bch_sb) -> Option<MembersV2<'_>> {
    let field = sb_field_get::<c::bch_sb_field_members_v2>(sb)?;
    Some(MembersV2 {
        member_bytes: field.member_bytes().get() as usize,
        nr_devices: sb.nr_devices as u32,
        bytes: field.as_bytes(),
    })
}

/// Construct a MembersV1 reader from a superblock.
pub fn members_v1(sb: &c::bch_sb) -> Option<MembersV1<'_>> {
    let field = sb_field_get::<c::bch_sb_field_members_v1>(sb)?;
    Some(MembersV1 {
        nr_devices: sb.nr_devices as u32,
        bytes: field.as_bytes(),
    })
}
