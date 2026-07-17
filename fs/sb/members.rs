use crate::c;
use crate::bitmask_accessors;
use super::io::*;

// ---------------------------------------------------------------------------
// Members — bounds-checked reader and writer for bch_sb_field_members_v2
//
// Members are variable-size: each entry is `member_bytes` wide, which may
// be smaller than `sizeof(bch_member)` for on-disk backward compatibility.
// The reader copies into a zeroed struct (like the C `bch2_members_v2_get`);
// the writer returns an in-place `&mut bch_member` for field-level mutation.
// ---------------------------------------------------------------------------

// Member state name table — generated from BCH_MEMBER_STATES() x-macro
include!(concat!(env!("OUT_DIR"), "/member_states_gen.rs"));

pub fn member_state_str(state: u8) -> &'static str {
    MEMBER_STATE_NAMES.get(state as usize).copied().unwrap_or("unknown")
}

const BCH_MEMBER_V1_BYTES: usize = 56;

/// Read-only view of members_v2 with bounds-checked access.
pub struct MembersV2<'a> {
    field: &'a c::bch_sb_field_members_v2,
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
        unsafe {
            let base = self.field._members.as_ptr() as *const u8;
            let src = base.add(idx as usize * self.member_bytes);
            let mut ret: c::bch_member = core::mem::zeroed();
            let copy_len = self.member_bytes.min(core::mem::size_of::<c::bch_member>());
            core::ptr::copy_nonoverlapping(src, &mut ret as *mut _ as *mut u8, copy_len);
            Some(ret)
        }
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

/// Mutable view of members_v2 with bounds-checked access.
pub struct MembersV2Mut<'a> {
    field: &'a mut c::bch_sb_field_members_v2,
    member_bytes: usize,
    nr_devices: u32,
}

impl<'a> MembersV2Mut<'a> {
    /// Get a copy (read path) — same as MembersV2::get.
    pub fn get(&self, idx: u32) -> Option<c::bch_member> {
        if idx >= self.nr_devices {
            return None;
        }
        unsafe {
            let base = self.field._members.as_ptr() as *const u8;
            let src = base.add(idx as usize * self.member_bytes);
            let mut ret: c::bch_member = core::mem::zeroed();
            let copy_len = self.member_bytes.min(core::mem::size_of::<c::bch_member>());
            core::ptr::copy_nonoverlapping(src, &mut ret as *mut _ as *mut u8, copy_len);
            Some(ret)
        }
    }

    /// Get a mutable reference to the member at `idx` for in-place field mutation.
    ///
    /// Callers should only write to fields that fit within `member_bytes`.
    pub fn get_mut(&mut self, idx: u32) -> Option<&mut c::bch_member> {
        if idx >= self.nr_devices {
            return None;
        }
        unsafe {
            let base = self.field._members.as_ptr() as *mut u8;
            let ptr = base.add(idx as usize * self.member_bytes);
            Some(&mut *(ptr as *mut c::bch_member))
        }
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
    field: &'a c::bch_sb_field_members_v1,
    nr_devices: u32,
}

impl<'a> MembersV1<'a> {
    pub fn get(&self, idx: u32) -> Option<c::bch_member> {
        if idx >= self.nr_devices {
            return None;
        }
        unsafe {
            let base = self.field._members.as_ptr() as *const u8;
            let src = base.add(idx as usize * BCH_MEMBER_V1_BYTES);
            let mut ret: c::bch_member = core::mem::zeroed();
            let copy_len = BCH_MEMBER_V1_BYTES.min(core::mem::size_of::<c::bch_member>());
            core::ptr::copy_nonoverlapping(src, &mut ret as *mut _ as *mut u8, copy_len);
            Some(ret)
        }
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
    let field: &c::bch_sb_field_members_v2 = sb_field_get(sb)?;
    Some(MembersV2 {
        member_bytes: u16::from_le(field.member_bytes) as usize,
        nr_devices: sb.nr_devices as u32,
        field,
    })
}

/// Construct a MembersV2Mut writer from a handle.
pub fn members_v2_mut(disk_sb: &mut c::bch_sb_handle) -> Option<MembersV2Mut<'_>> {
    let nr_devices = unsafe { (*disk_sb.sb).nr_devices as u32 };
    let field: &mut c::bch_sb_field_members_v2 = sb_field_get_mut(disk_sb)?;
    let member_bytes = u16::from_le(field.member_bytes) as usize;
    Some(MembersV2Mut {
        field,
        member_bytes,
        nr_devices,
    })
}

/// Construct a MembersV1 reader from a superblock.
pub fn members_v1(sb: &c::bch_sb) -> Option<MembersV1<'_>> {
    let field: &c::bch_sb_field_members_v1 = sb_field_get(sb)?;
    Some(MembersV1 {
        nr_devices: sb.nr_devices as u32,
        field,
    })
}

bitmask_accessors! {
    bch_member, flags,
        BCH_MEMBER_STATE          => (member_state, set_member_state),
        BCH_MEMBER_GROUP          => (member_group, set_member_group),
        BCH_MEMBER_DATA_ALLOWED   => (member_data_allowed, set_member_data_allowed),
        BCH_MEMBER_RESIZE_ON_MOUNT => (member_resize_on_mount, set_member_resize_on_mount),
        BCH_MEMBER_ROTATIONAL_SET => (member_rotational_set, set_member_rotational_set),
        BCH_MEMBER_FREESPACE_INITIALIZED => (member_freespace_initialized, set_member_freespace_initialized);
}
