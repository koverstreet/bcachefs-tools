use crate::c;
use crate::c::{bch_member, bch_sb, bch_sb_field_crypt, bch_sb_handle, block_device, nonce};
use crate::sb::members;
use crate::types::ArmOf;
use crate::util::vstructs::{Plain, SelfSized, View, ViewMut};

/// A superblock section's type: an arm of tagged_union! bch_sb_field, which
/// gives its field type - Plain, so a View of a short one reads as zero
/// past its end.
pub trait SbField: ArmOf<c::bch_sb_field, Tag = u32> + Plain + Sized {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type(Self::TAG);
}

impl<F: ArmOf<c::bch_sb_field, Tag = u32> + Plain> SbField for F {}

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

/*
 * bcachefs_format.h's JSET_MAGIC and BSET_MAGIC. C keeps its own: they're
 * __cpu_to_le64()s, XORed with the uuid's bytes as they are, which a
 * c_const! - a plain integer - can't say.
 */
const JSET_MAGIC: u64 = 0x245235c1a3625032;
const BSET_MAGIC: u64 = 0x90135c78b99e07f5;

impl bch_sb {
    /// Its section of type @F: sb_field_get().
    pub fn field<F: SbField>(&self) -> Option<View<'_, F>> {
        sb_field_get(self)
    }

    /// Its crypt section, whole - validate rejects a shorter one.
    pub fn crypt(&self) -> Option<&bch_sb_field_crypt> {
        self.field::<bch_sb_field_crypt>()?.whole()
    }

    pub fn uuid(&self) -> uuid::Uuid {
        uuid::Uuid::from_bytes(self.user_uuid.b)
    }

    /// The magic this filesystem's jsets carry: __jset_magic().
    pub fn jset_magic(&self) -> u64 {
        self.magic() ^ JSET_MAGIC
    }

    /// The magic this filesystem's btree nodes carry: __bset_magic().
    pub fn bset_magic(&self) -> u64 {
        self.magic() ^ BSET_MAGIC
    }

    /// The first 8 bytes of its internal uuid: __bch2_sb_magic().
    fn magic(&self) -> u64 {
        u64::from_le_bytes(self.uuid.b[..8].try_into().unwrap())
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
        // nonce.d is __le32: the raw bytes, as they are.
        use crate::types::Le32;
        let raw = |b| Le32::new(u32::from_le_bytes(b));
        nonce {
            d: [Le32::new(0), Le32::new(0), raw([a, b, c, d]), raw([e, f, g, h])],
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

    /// Its section of type @F: sb_field_get().
    pub fn field<F: SbField>(&self) -> Option<View<'_, F>> {
        sb_field_get(self.sb())
    }

    /// Its section of type @F, to edit: sb_field_get_mut().
    pub fn field_mut<F: SbField>(&mut self) -> Option<ViewMut<'_, F>> {
        sb_field_get_mut(self)
    }

    /// Resize a superblock field to `u64s` 64-bit words: sb_field_resize().
    pub fn field_resize<F: SbField>(&mut self, u64s: u32) -> Option<ViewMut<'_, F>> {
        sb_field_resize(self, u64s)
    }

    /// Get or create a superblock field with at least `min_u64s` size:
    /// sb_field_get_minsize().
    pub fn field_get_minsize<F: SbField>(&mut self, min_u64s: u32) -> Option<ViewMut<'_, F>> {
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
        let v = unsafe { c::bch2_disk_path_find(self, name.as_ptr().cast()) };
        (v >= 0).then_some(v as u32)
    }

    /// Find or create a disk path; Err(errno) on failure.
    pub fn disk_path_find_or_create(&mut self, name: &core::ffi::CStr) -> Result<u32, i32> {
        let v = unsafe { c::bch2_disk_path_find_or_create(self, name.as_ptr().cast()) };
        if v >= 0 { Ok(v as u32) } else { Err(-v) }
    }

    /// The superblock's full vstruct extent as bytes.
    pub fn sb_bytes(&self) -> &[u8] {
        let bytes = self.sb().bytes();
        unsafe { core::slice::from_raw_parts(self.sb as *const u8, bytes) }
    }

    /// The superblock's full vstruct extent, mutably.
    pub fn sb_bytes_mut(&mut self) -> &mut [u8] {
        let bytes = self.sb().bytes();
        unsafe { core::slice::from_raw_parts_mut(self.sb as *mut u8, bytes) }
    }

    /// Member @idx, to edit: None if there's no such member in the section,
    /// or it's shorter than a bch_member - written before it grew - which a
    /// write to would run past, into the next member.
    pub fn member_mut(&mut self, idx: u32) -> Option<&mut bch_member> {
        let nr = self.sb().nr_devices as u32;
        let mi = self.field::<c::bch_sb_field_members_v2>()?;
        let member_bytes = mi.member_bytes().get() as usize;
        let end = core::mem::offset_of!(c::bch_sb_field_members_v2, _members) + (idx as usize + 1) * member_bytes;
        if idx >= nr || member_bytes < size_of::<bch_member>() || end > mi.as_bytes().len() {
            return None;
        }
        // SAFETY: the member's in the section, all of it
        unsafe { Some(&mut *c::bch2_members_v2_get_mut(self.sb, idx as i32)) }
    }

    /// Read-only, bounds-checked access to members_v2.
    pub fn members_v2(&self) -> Option<members::MembersV2<'_>> {
        members::members_v2(self.sb())
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

pub struct CounterInfo {
    pub name: &'static str,
    pub stable_id: u16,
    pub is_sectors: bool,
}

// From BCH_PERSISTENT_COUNTERS(): its entries are (name, stable id, type,
// description).
macro_rules! counter_is_sectors {
    (TYPE_SECTORS) => { true };
    ($other:tt) => { false };
}
macro_rules! counter_table {
    ($(($name:tt, $id:tt, $ty:tt $(, $($rest:tt)*)?)),* $(,)?) => {
        pub const COUNTERS: &[CounterInfo] = &[$(
            CounterInfo { name: stringify!($name), stable_id: $id, is_sectors: counter_is_sectors!($ty) },
        )*];
    };
}
c::BCH_PERSISTENT_COUNTERS!(counter_table);

// ---------------------------------------------------------------------------
// Superblock field access — safe, handle-based API
//
// The key safety property: `sb_field_resize` takes `&mut bch_sb_handle`,
// which invalidates any outstanding Views from `sb_field_get` at compile
// time. This is the capnp-inspired reader/builder split —
// resize is the "build" operation and must be exclusive.
// ---------------------------------------------------------------------------

/// The superblock's section of type @F, if it has one: a View of the bytes
/// its header says are its - one written before F grew reads as zero past
/// them, as C's bch2_sb_field_get_minsize() would have grown it to.
pub fn sb_field_get<F: SbField>(sb: &c::bch_sb) -> Option<View<'_, F>> {
    let f = unsafe { c::bch2_sb_field_get_id(sb as *const _ as *mut _, F::FIELD_TYPE) };
    // SAFETY: C's walk of the sections the superblock's count says it has -
    // validated, so the section and its bytes are there
    (!f.is_null()).then(|| unsafe { View::from_validated(f) })?.arm::<F>()
}

/// sb_field_get(), to edit: a field past the section's end is a set()
/// error, not a write it won't carry - whole_mut() for all of it, if it's
/// all there.
///
/// Taking `&mut bch_sb_handle` ensures exclusive access and prevents
/// dangling references after resize.
pub fn sb_field_get_mut<F: SbField>(disk_sb: &mut c::bch_sb_handle) -> Option<ViewMut<'_, F>> {
    let f = unsafe { c::bch2_sb_field_get_id(disk_sb.sb, F::FIELD_TYPE) };
    // SAFETY: as sb_field_get()'s
    (!f.is_null()).then(|| unsafe { ViewMut::from_validated(f) })?.arm_mut::<F>()
}

/// Resize the section of type @F to @u64s, making it if there's none: the
/// section, at its (possibly new) location - whole_mut() if that's all of F.
/// The `&mut` borrow on the handle ensures no stale references can exist.
pub fn sb_field_resize<F: SbField>(
    disk_sb: &mut c::bch_sb_handle,
    u64s: u32,
) -> Option<ViewMut<'_, F>> {
    let f = unsafe { c::bch2_sb_field_resize_id(disk_sb, F::FIELD_TYPE, u64s) };
    // SAFETY: C's resize, which reallocates the superblock if it has to
    (!f.is_null()).then(|| unsafe { ViewMut::from_validated(f) })?.arm_mut::<F>()
}

/// The section of type @F, made or grown to at least @min_u64s: as
/// sb_field_resize().
pub fn sb_field_get_minsize<F: SbField>(
    disk_sb: &mut c::bch_sb_handle,
    min_u64s: u32,
) -> Option<ViewMut<'_, F>> {
    let f = unsafe { c::bch2_sb_field_get_minsize_id(disk_sb, F::FIELD_TYPE, min_u64s) };
    // SAFETY: as sb_field_resize()'s
    (!f.is_null()).then(|| unsafe { ViewMut::from_validated(f) })?.arm_mut::<F>()
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

    let bytes = sb.bytes();
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
        let f_u64s = f.u64s.get() as u64;
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

    pub fn field<F: SbField>(&self) -> Option<View<'a, F>> {
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

    pub fn field<F: SbField>(&self) -> Option<View<'_, F>> {
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

    pub fn field<F: SbField>(&self) -> Option<View<'_, F>> {
        sb_field_get(self.sb())
    }
}

#[cfg(feature = "std")]
impl SbAccess for SbBuf {
    fn sb(&self) -> &bch_sb { SbBuf::sb(self) }
    fn sb_mut(&mut self) -> &mut bch_sb { SbBuf::sb_mut(self) }
    fn sb_bytes(&self) -> &[u8] { SbBuf::bytes(self) }
}

#[cfg(test)]
mod tests {
    use crate::c;
    use crate::types::Le32;
    use crate::util::vstructs::{VStruct, View};

    /// @buf as a superblock field of type @ty, @u64s long - its header written
    /// - and the View of it a superblock's walk would give: as many bytes as
    /// it says.
    fn field(buf: &mut [u64], ty: c::bch_sb_field_type, u64s: usize) -> View<'_, c::bch_sb_field> {
        let f = buf.as_mut_ptr().cast::<c::bch_sb_field>();
        // SAFETY: u64s, which any bytes are, aligned for a bch_sb_field, and
        // as many as u64s says
        unsafe {
            (*f).set_val_u64s(u64s);
            (*f).type_ = Le32::new(ty.0);
            View::from_validated(f)
        }
    }

    /// A superblock field is the section its type says - and one this code
    /// doesn't know is its type, as an error.
    #[test]
    fn sb_field_get() {
        type T = c::bch_sb_field_type;
        let mut buf = [0u64; 4];

        let f = field(&mut buf, T::BCH_SB_FIELD_journal, 3);
        assert!(matches!(f.typed(), Ok(c::BchSbFieldRef::journal(j)) if j.field().u64s().get() == 3));

        let f = field(&mut buf, T::BCH_SB_FIELD_NR, 3);
        assert!(matches!(f.typed(), Err(t) if t == T::BCH_SB_FIELD_NR));
    }

    /// An ext section from before btrees_lost_data, 88 bytes, is an ext
    /// section: the fields it has read as they are, the ones it doesn't as
    /// zero - not as whatever follows it, which here is all ones.
    #[test]
    fn sb_field_get_short_section() {
        let mut buf = [u64::MAX; 16];
        buf[..11].fill(0);
        buf[3] = 0x5a;                          // errors_silent[0]
        let f = field(&mut buf, c::bch_sb_field_type::BCH_SB_FIELD_ext, 11);

        let Ok(c::BchSbFieldRef::ext(e)) = f.typed() else { panic!("an ext section") };
        assert_eq!(e.errors_silent().get()[0], 0x5a);
        assert_eq!(e.btrees_lost_data().get(), 0);
        assert_eq!(e.btrees_clean().get(), 0);
        assert!(e.whole().is_none());
        assert_eq!(e.as_bytes().len(), 88);

        // and as the one arm sb_field_get() asks for
        let e = f.arm::<c::bch_sb_field_ext>().expect("an ext section");
        assert_eq!((e.errors_silent().get()[0], e.btrees_lost_data().get()), (0x5a, 0));
        assert!(f.arm::<c::bch_sb_field_crypt>().is_none());
    }
}
