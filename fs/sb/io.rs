use crate::c;
use crate::c::{bch_member, bch_sb, bch_sb_field_crypt, bch_sb_handle, block_device, nonce};
use crate::sb::members;
use crate::bitmask_accessors;

// SbField trait + impls — generated from BCH_SB_FIELDS() x-macro
include!(concat!(env!("OUT_DIR"), "/sb_field_types_gen.rs"));

impl PartialEq for bch_sb {
    fn eq(&self, other: &Self) -> bool {
        self.magic.b == other.magic.b
            && self.user_uuid.b == other.user_uuid.b
            && self.block_size == other.block_size
            && self.version == other.version
            && self.uuid.b == other.uuid.b
            && self.seq == other.seq
    }
}

impl bch_sb {
    pub fn field<F: SbField>(&self) -> Option<&F> {
        sb_field_get(self)
    }

    pub fn crypt(&self) -> Option<&bch_sb_field_crypt> {
        self.field()
    }

    pub fn uuid(&self) -> uuid::Uuid {
        uuid::Uuid::from_bytes(self.user_uuid.b)
    }

    pub fn label(&self) -> &[u8] {
        let len = self
            .label
            .iter()
            .position(|b| *b == b'\0')
            .unwrap_or(self.label.len());
        &self.label[..len]
    }

    pub fn number_of_devices(&self) -> u32 {
        unsafe { c::bch2_sb_nr_devices(self) }
    }

    /// Get the nonce used to encrypt the superblock
    pub fn nonce(&self) -> nonce {
        let [a, b, c, d, e, f, g, h, _rest @ ..] = self.uuid.b;
        let dword1 = u32::from_le_bytes([a, b, c, d]);
        let dword2 = u32::from_le_bytes([e, f, g, h]);
        nonce {
            d: [0, 0, dword1, dword2],
        }
    }
}

impl bch_sb_handle {
    pub fn sb(&self) -> &bch_sb {
        unsafe { &*self.sb }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        unsafe { &mut *self.sb }
    }

    pub fn bdev(&self) -> &block_device {
        unsafe { &*self.bdev }
    }

    /// Get a typed reference to a superblock field, or None if absent.
    pub fn field<F: SbField>(&self) -> Option<&F> {
        sb_field_get(self.sb())
    }

    /// Get a typed mutable reference to a superblock field, or None if absent.
    pub fn field_mut<F: SbField>(&mut self) -> Option<&mut F> {
        sb_field_get_mut(self)
    }

    /// Resize a superblock field to `u64s` 64-bit words.
    pub fn field_resize<F: SbField>(&mut self, u64s: u32) -> Option<&mut F> {
        sb_field_resize(self, u64s)
    }

    /// Get or create a superblock field with at least `min_u64s` size.
    pub fn field_get_minsize<F: SbField>(&mut self, min_u64s: u32) -> Option<&mut F> {
        sb_field_get_minsize(self, min_u64s)
    }

    /// Get a mutable reference to a single member entry by device index.
    ///
    /// This is the simple accessor for one-shot field mutation. For
    /// iteration, use `members_v2_mut()`.
    pub fn member_mut(&mut self, idx: u32) -> Option<&mut bch_member> {
        let nr = self.sb().nr_devices as u32;
        if idx >= nr { return None; }
        unsafe { Some(&mut *c::bch2_members_v2_get_mut(self.sb, idx as i32)) }
    }

    /// Read-only, bounds-checked access to members_v2.
    pub fn members_v2(&self) -> Option<members::MembersV2<'_>> {
        members::members_v2(self.sb())
    }

    /// Mutable, bounds-checked access to members_v2.
    pub fn members_v2_mut(&mut self) -> Option<members::MembersV2Mut<'_>> {
        members::members_v2_mut(self)
    }

    /// Read-only, bounds-checked access to members_v1.
    pub fn members_v1(&self) -> Option<members::MembersV1<'_>> {
        members::members_v1(self.sb())
    }
}

impl Drop for bch_sb_handle {
    fn drop(&mut self) {
        unsafe { c::bch2_free_super(&mut *self); }
    }
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
}
