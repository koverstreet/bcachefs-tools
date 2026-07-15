pub mod io;

use crate::c;
use crate::bitmask_accessors;

// SbField trait + impls — generated from BCH_SB_FIELDS() x-macro
include!(concat!(env!("OUT_DIR"), "/sb_field_types_gen.rs"));

// Member state name table — generated from BCH_MEMBER_STATES() x-macro
include!(concat!(env!("OUT_DIR"), "/member_states_gen.rs"));

pub fn member_state_str(state: u8) -> &'static str {
    MEMBER_STATE_NAMES.get(state as usize).copied().unwrap_or("unknown")
}

// Counter info table — generated from BCH_PERSISTENT_COUNTERS() x-macro
include!(concat!(env!("OUT_DIR"), "/counters_gen.rs"));

// ---------------------------------------------------------------------------
// Superblock field access — safe, handle-based API
//
// The key safety property: `sb_field_resize` takes `&mut bch_sb_handle`,
// which invalidates any outstanding `&F` references from `sb_field_get`
// at compile time. This is the capnp-inspired reader/builder split —
// resize is the "build" operation and must be exclusive.
// ---------------------------------------------------------------------------

/// Get a typed reference to a superblock field, or None if absent.
pub fn sb_field_get<F: SbField>(sb: &c::bch_sb) -> Option<&F> {
    unsafe {
        let ptr = c::bch2_sb_field_get_id(sb as *const _ as *mut _, F::FIELD_TYPE);
        if ptr.is_null() { None } else { Some(&*(ptr as *const F)) }
    }
}

/// Get a typed mutable reference to a superblock field via handle.
///
/// Taking `&mut bch_sb_handle` ensures exclusive access and prevents
/// dangling references after resize.
pub fn sb_field_get_mut<F: SbField>(disk_sb: &mut c::bch_sb_handle) -> Option<&mut F> {
    unsafe {
        let ptr = c::bch2_sb_field_get_id(disk_sb.sb, F::FIELD_TYPE);
        if ptr.is_null() { None } else { Some(&mut *(ptr as *mut F)) }
    }
}

/// Resize a typed superblock field.
///
/// Returns the field at its (possibly new) location. The `&mut` borrow on
/// the handle ensures no stale references can exist.
pub fn sb_field_resize<F: SbField>(
    disk_sb: &mut c::bch_sb_handle,
    u64s: u32,
) -> Option<&mut F> {
    unsafe {
        let ptr = c::bch2_sb_field_resize_id(disk_sb, F::FIELD_TYPE, u64s);
        if ptr.is_null() { None } else { Some(&mut *(ptr as *mut F)) }
    }
}

/// Get a typed field, creating or growing it to at least `min_u64s`.
pub fn sb_field_get_minsize<F: SbField>(
    disk_sb: &mut c::bch_sb_handle,
    min_u64s: u32,
) -> Option<&mut F> {
    unsafe {
        let ptr = c::bch2_sb_field_get_minsize_id(disk_sb, F::FIELD_TYPE, min_u64s);
        if ptr.is_null() { None } else { Some(&mut *(ptr as *mut F)) }
    }
}

// ---------------------------------------------------------------------------
// Members — bounds-checked reader and writer for bch_sb_field_members_v2
//
// Members are variable-size: each entry is `member_bytes` wide, which may
// be smaller than `sizeof(bch_member)` for on-disk backward compatibility.
// The reader copies into a zeroed struct (like the C `bch2_members_v2_get`);
// the writer returns an in-place `&mut bch_member` for field-level mutation.
// ---------------------------------------------------------------------------

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
            let mut ret: c::bch_member = std::mem::zeroed();
            let copy_len = self.member_bytes.min(std::mem::size_of::<c::bch_member>());
            std::ptr::copy_nonoverlapping(src, &mut ret as *mut _ as *mut u8, copy_len);
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
            let mut ret: c::bch_member = std::mem::zeroed();
            let copy_len = self.member_bytes.min(std::mem::size_of::<c::bch_member>());
            std::ptr::copy_nonoverlapping(src, &mut ret as *mut _ as *mut u8, copy_len);
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
            let mut ret: c::bch_member = std::mem::zeroed();
            let copy_len = BCH_MEMBER_V1_BYTES.min(std::mem::size_of::<c::bch_member>());
            std::ptr::copy_nonoverlapping(src, &mut ret as *mut _ as *mut u8, copy_len);
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

// LE64_BITMASK accessors — pure Rust replacements for C shims in rust_shims.c.
// Each field is defined by: struct type, flags field + index, C constant prefix.

bitmask_accessors! {
    bch_sb, flags[0],
        BCH_SB_INITIALIZED        => (sb_initialized, set_sb_initialized),
        BCH_SB_CLEAN              => (sb_clean, set_sb_clean),
        BCH_SB_CSUM_TYPE          => (sb_csum_type, set_sb_csum_type),
        BCH_SB_BTREE_NODE_SIZE    => (sb_btree_node_size, set_sb_btree_node_size);

    bch_sb, flags[1],
        BCH_SB_ENCRYPTION_TYPE    => (sb_encryption_type, set_sb_encryption_type),
        BCH_SB_META_REPLICAS_REQ  => (sb_meta_replicas_req, set_sb_meta_replicas_req),
        BCH_SB_DATA_REPLICAS_REQ  => (sb_data_replicas_req, set_sb_data_replicas_req),
        BCH_SB_PROMOTE_TARGET     => (sb_promote_target, set_sb_promote_target),
        BCH_SB_FOREGROUND_TARGET  => (sb_foreground_target, set_sb_foreground_target),
        BCH_SB_BACKGROUND_TARGET  => (sb_background_target, set_sb_background_target);

    bch_sb, flags[3],
        BCH_SB_METADATA_TARGET    => (sb_metadata_target, set_sb_metadata_target),
        BCH_SB_MULTI_DEVICE       => (sb_multi_device, set_sb_multi_device);

    bch_sb, flags[5],
        BCH_SB_VERSION_INCOMPAT_ALLOWED => (sb_version_incompat_allowed, set_sb_version_incompat_allowed);

    bch_sb, flags[6],
        BCH_SB_EXTENT_BP_SHIFT    => (sb_extent_bp_shift, set_sb_extent_bp_shift);

    bch_member, flags,
        BCH_MEMBER_STATE          => (member_state, set_member_state),
        BCH_MEMBER_GROUP          => (member_group, set_member_group),
        BCH_MEMBER_DATA_ALLOWED   => (member_data_allowed, set_member_data_allowed),
        BCH_MEMBER_RESIZE_ON_MOUNT => (member_resize_on_mount, set_member_resize_on_mount),
        BCH_MEMBER_ROTATIONAL_SET => (member_rotational_set, set_member_rotational_set),
        BCH_MEMBER_FREESPACE_INITIALIZED => (member_freespace_initialized, set_member_freespace_initialized);
}
