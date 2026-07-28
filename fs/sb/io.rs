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

    /// The filesystem label, as bytes up to the first NUL.
    pub fn label(&self) -> &[u8] {
        let len = self.label.iter().position(|b| *b == 0).unwrap_or(self.label.len());
        &self.label[..len]
    }

    pub fn number_of_devices(&self) -> u32 {
        unsafe { c::bch2_sb_nr_devices(self) }
    }

    /// Get the nonce used to encrypt the superblock
    pub fn nonce(&self) -> nonce {
        let [a, b, c, d, e, f, g, h, _rest @ ..] = self.uuid.b;
        // nonce.d is __le32, so keep the raw bytes.
        let dword1 = u32::from_ne_bytes([a, b, c, d]);
        let dword2 = u32::from_ne_bytes([e, f, g, h]);
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

    /// Delete a superblock field by type.
    pub fn field_delete(&mut self, ty: c::bch_sb_field_type) {
        unsafe { c::bch2_sb_field_delete(self, ty) }
    }

    /// Reallocate the superblock buffer for at least `u64s` (0 = minimum).
    pub fn sb_realloc(&mut self, u64s: u32) -> Result<(), i32> {
        match unsafe { c::bch2_sb_realloc(self, u64s) } {
            0 => Ok(()),
            e => Err(e),
        }
    }

    /// Copy members_v2 into the members_v1 mirror, for old-kernel compat.
    pub fn members_cpy_v2_v1(&mut self) {
        unsafe { c::bch2_sb_members_cpy_v2_v1(self) };
    }

    /// Find a disk path (label group) by name.
    pub fn disk_path_find(&mut self, name: &core::ffi::CStr) -> Option<u32> {
        let v = unsafe { c::bch2_disk_path_find(self, name.as_ptr()) };
        (v >= 0).then_some(v as u32)
    }

    /// Find or create a disk path; Err(errno) on failure.
    pub fn disk_path_find_or_create(&mut self, name: &core::ffi::CStr) -> Result<u32, i32> {
        let v = unsafe { c::bch2_disk_path_find_or_create(self, name.as_ptr()) };
        if v >= 0 { Ok(v as u32) } else { Err(-v) }
    }

    /// The superblock's full vstruct extent as bytes.
    pub fn sb_bytes(&self) -> &[u8] {
        let bytes = core::mem::size_of::<bch_sb>()
            + u32::from_le(self.sb().u64s) as usize * 8;
        unsafe { core::slice::from_raw_parts(self.sb as *const u8, bytes) }
    }

    /// The superblock's full vstruct extent, mutably.
    pub fn sb_bytes_mut(&mut self) -> &mut [u8] {
        let bytes = core::mem::size_of::<bch_sb>()
            + u32::from_le(self.sb().u64s) as usize * 8;
        unsafe { core::slice::from_raw_parts_mut(self.sb as *mut u8, bytes) }
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

// Safe wrappers over the hand-rolled v2 error entry accessors
// (sb/errors_format.h): they only read through the pointer.
impl c::bch_sb_field_error_entry_v2 {
    pub fn id(&self) -> u16 {
        unsafe { c::BCH_SB_ERROR_ENTRY_V2_ID(self) as u16 }
    }

    pub fn nr(&self) -> u64 {
        unsafe { c::BCH_SB_ERROR_ENTRY_V2_NR(self) }
    }

    pub fn first_error_time(&self) -> u64 {
        unsafe { c::BCH_SB_ERROR_ENTRY_V2_FIRST(self) }
    }

    pub fn last_error_time(&self) -> u64 {
        unsafe { c::BCH_SB_ERROR_ENTRY_V2_LAST(self) }
    }
}

// ---------------------------------------------------------------------------
// Raw-buffer superblock views
//
// For superblocks in plain byte buffers (device scans, recovery) rather
// than a C bch_sb_handle. Construction validates every extent once -
// header within the buffer, vstruct extent within the buffer, each
// field's extent within the superblock - so typed access afterwards is
// safe: the C field lookups walk exactly the extents parse() checked.
// Semantic validity remains bch2_sb_validate()'s job; these views
// guarantee memory safety only. Raw buffers can't be reallocated, so
// mutation is in-place only - field resize/delete stay on bch_sb_handle.
// ---------------------------------------------------------------------------

pub const BCACHE_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x4e, 0x1a, 0x45, 0xca,
    0x82, 0x65, 0xf5, 0x7f, 0x48, 0xba, 0x6d, 0x81,
];
pub const BCHFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

/// Why a buffer failed to parse as a superblock. Precise by design:
/// recovery reports these against scan candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbParseError {
    BufferTooSmall { need: usize, have: usize },
    Misaligned,
    BadMagic,
    ExtentBeyondBuffer { need: usize, have: usize },
    FieldBeyondSb { field_offset: usize },
    FieldBadU64s { field_offset: usize },
}

impl core::fmt::Display for SbParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SbParseError::BufferTooSmall { need, have } =>
                write!(f, "buffer too small for superblock header: need {need}, have {have}"),
            SbParseError::Misaligned =>
                write!(f, "buffer not sufficiently aligned for a superblock"),
            SbParseError::BadMagic =>
                write!(f, "not a bcachefs superblock (bad magic)"),
            SbParseError::ExtentBeyondBuffer { need, have } =>
                write!(f, "superblock u64s extends beyond buffer: need {need}, have {have}"),
            SbParseError::FieldBeyondSb { field_offset } =>
                write!(f, "superblock field at offset {field_offset} extends beyond superblock"),
            SbParseError::FieldBadU64s { field_offset } =>
                write!(f, "superblock field at offset {field_offset} with u64s 0"),
        }
    }
}

/// Validate the extents; returns the vstruct size in bytes.
fn sb_parse_checks(buf: &[u8]) -> Result<usize, SbParseError> {
    let hdr = core::mem::size_of::<bch_sb>();
    if buf.len() < hdr {
        return Err(SbParseError::BufferTooSmall { need: hdr, have: buf.len() });
    }
    if buf.as_ptr() as usize % core::mem::align_of::<bch_sb>() != 0 {
        return Err(SbParseError::Misaligned);
    }
    /* header extent and alignment just validated: */
    let sb = unsafe { &*(buf.as_ptr() as *const bch_sb) };

    if sb.magic.b != BCACHE_MAGIC && sb.magic.b != BCHFS_MAGIC {
        return Err(SbParseError::BadMagic);
    }

    let bytes = hdr + u32::from_le(sb.u64s) as usize * 8;
    if bytes > buf.len() {
        return Err(SbParseError::ExtentBeyondBuffer { need: bytes, have: buf.len() });
    }

    let mut off = hdr;
    while off < bytes {
        if bytes - off < core::mem::size_of::<c::bch_sb_field>() {
            return Err(SbParseError::FieldBeyondSb { field_offset: off });
        }
        /* field header extent just validated: */
        let f = unsafe { &*(buf.as_ptr().add(off) as *const c::bch_sb_field) };
        let f_u64s = u32::from_le(f.u64s) as u64;
        if f_u64s == 0 {
            return Err(SbParseError::FieldBadU64s { field_offset: off });
        }
        let next = off as u64 + f_u64s * 8;
        if next > bytes as u64 {
            return Err(SbParseError::FieldBeyondSb { field_offset: off });
        }
        off = next as usize;
    }

    Ok(bytes)
}

/// A validated read-only superblock in a byte buffer.
pub struct SbRef<'a> {
    buf:   &'a [u8],
    bytes: usize,
}

impl<'a> SbRef<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Self, SbParseError> {
        let bytes = sb_parse_checks(buf)?;
        Ok(SbRef { buf, bytes })
    }

    pub fn sb(&self) -> &'a bch_sb {
        unsafe { &*(self.buf.as_ptr() as *const bch_sb) }
    }

    /// The superblock's full vstruct extent.
    pub fn bytes(&self) -> &'a [u8] {
        &self.buf[..self.bytes]
    }

    pub fn field<F: SbField>(&self) -> Option<&'a F> {
        sb_field_get(self.sb())
    }
}

/// A validated superblock in a byte buffer, for in-place mutation.
pub struct SbMut<'a> {
    buf:   &'a mut [u8],
    bytes: usize,
}

impl<'a> SbMut<'a> {
    pub fn parse(buf: &'a mut [u8]) -> Result<Self, SbParseError> {
        let bytes = sb_parse_checks(buf)?;
        Ok(SbMut { buf, bytes })
    }

    pub fn sb(&self) -> &bch_sb {
        unsafe { &*(self.buf.as_ptr() as *const bch_sb) }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        unsafe { &mut *(self.buf.as_mut_ptr() as *mut bch_sb) }
    }

    /// The superblock's full vstruct extent.
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.bytes]
    }

    pub fn field<F: SbField>(&self) -> Option<&F> {
        sb_field_get(self.sb())
    }
}

/// Anything holding a superblock that can be viewed as bytes and
/// mutated in place: SbMut/SbBuf over raw buffers, bch_sb_handle for
/// C-owned superblocks. Superblock IO takes any of them.
pub trait SbAccess {
    fn sb(&self) -> &bch_sb;
    fn sb_mut(&mut self) -> &mut bch_sb;
    /// The superblock's full vstruct extent.
    fn sb_bytes(&self) -> &[u8];
}

impl SbAccess for bch_sb_handle {
    fn sb(&self) -> &bch_sb { bch_sb_handle::sb(self) }
    fn sb_mut(&mut self) -> &mut bch_sb { bch_sb_handle::sb_mut(self) }
    fn sb_bytes(&self) -> &[u8] { bch_sb_handle::sb_bytes(self) }
}

impl SbAccess for SbMut<'_> {
    fn sb(&self) -> &bch_sb { SbMut::sb(self) }
    fn sb_mut(&mut self) -> &mut bch_sb { SbMut::sb_mut(self) }
    fn sb_bytes(&self) -> &[u8] { SbMut::bytes(self) }
}

/// An owned superblock: aligned storage, extents validated at
/// construction. The owned counterpart of SbRef/SbMut, for recovery
/// flows that carry superblocks around as values.
#[cfg(feature = "std")]
pub struct SbBuf {
    buf:   Vec<u64>,
    bytes: usize,
}

#[cfg(feature = "std")]
impl SbBuf {
    /// Copy a superblock out of `src` (no alignment requirement) and
    /// validate it.
    pub fn from_bytes(src: &[u8]) -> Result<Self, SbParseError> {
        let mut buf = vec![0u64; src.len().div_ceil(8)];
        let dst = unsafe {
            core::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, src.len())
        };
        dst.copy_from_slice(src);

        let bytes = sb_parse_checks(&dst[..])?;
        Ok(SbBuf { buf, bytes })
    }

    pub fn sb(&self) -> &bch_sb {
        unsafe { &*(self.buf.as_ptr() as *const bch_sb) }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        unsafe { &mut *(self.buf.as_mut_ptr() as *mut bch_sb) }
    }

    /// The superblock's full vstruct extent.
    pub fn bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr() as *const u8, self.bytes) }
    }

    pub fn field<F: SbField>(&self) -> Option<&F> {
        sb_field_get(self.sb())
    }
}

#[cfg(feature = "std")]
impl SbAccess for SbBuf {
    fn sb(&self) -> &bch_sb { SbBuf::sb(self) }
    fn sb_mut(&mut self) -> &mut bch_sb { SbBuf::sb_mut(self) }
    fn sb_bytes(&self) -> &[u8] { SbBuf::bytes(self) }
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
