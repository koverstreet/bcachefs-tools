use crate::c;
use crate::c::{bch_member, bch_sb, bch_sb_field_crypt, bch_sb_handle, block_device, nonce};
use crate::sb::members;
use crate::types::ArmOf;
use crate::util::vstructs::{read_past, ArmRef, Head, SelfSized, Validated};

/// A superblock section's type: an arm of tagged_union! bch_sb_field, which
/// gives its field type - handed out as its ArmRef says, a short one, written
/// before its type grew, reading as zero past its end.
pub trait SbField: ArmOf<c::bch_sb_field, Tag = u32> + ArmRef {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type(Self::TAG);
}

impl<F: ArmOf<c::bch_sb_field, Tag = u32> + ArmRef + ?Sized> SbField for F {}

impl PartialEq for c::bch_sb__head {
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

/// What walks its sections - C's walk, which trusts their counts: a
/// validated superblock's.
impl c::bch_sb {
    pub fn field<F: SbField + ?Sized>(&self) -> Option<&F> {
        sb_field_get::<F>(self)
    }

    /// Its crypt section - whole, or none: validate rejects a shorter one.
    pub fn crypt(&self) -> Option<&bch_sb_field_crypt> {
        self.field::<bch_sb_field_crypt>()
    }

    pub fn number_of_devices(&self) -> u32 {
        unsafe { c::bch2_sb_nr_devices(self.as_ffi()) }
    }
}

/// Its header's: every state's, which each derefs to.
impl c::bch_sb__head {
    pub fn uuid(&self) -> uuid::Uuid {
        uuid::Uuid::from_bytes(self.user_uuid.b)
    }

    /// The magic this filesystem's jsets carry.
    pub fn jset_magic(&self) -> u64 {
        self.magic() ^ JSET_MAGIC
    }

    /// The magic this filesystem's btree nodes carry.
    pub fn bset_magic(&self) -> u64 {
        self.magic() ^ BSET_MAGIC
    }

    /// The first 8 bytes of its internal uuid.
    fn magic(&self) -> u64 {
        u64::from_le_bytes(self.uuid.b[..8].try_into().unwrap())
    }

    /// Up to the first NUL.
    pub fn label(&self) -> &[u8] {
        let len = self.label.iter().position(|b| *b == 0).unwrap_or(self.label.len());
        &self.label[..len]
    }

    /// The nonce the superblock is encrypted with.
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
        // SAFETY: C read and validated it, and owns its buffer while we're
        // borrowed - resize and realloc take &mut
        unsafe { bch_sb::<Validated>::from_ffi(self.sb) }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        // SAFETY: as sb()'s
        unsafe { bch_sb::<Validated>::from_ffi_mut(self.sb) }
    }

    pub fn bdev(&self) -> &block_device {
        unsafe { &*self.bdev }
    }

    pub fn field<F: SbField + ?Sized>(&self) -> Option<&F> {
        sb_field_get::<F>(self.sb())
    }

    pub fn field_mut<F: SbField + ?Sized>(&mut self) -> Option<&mut F> {
        sb_field_get_mut::<F>(self)
    }

    pub fn field_resize<F: SbField + ?Sized>(&mut self, u64s: u32) -> Option<&mut F> {
        sb_field_resize::<F>(self, u64s)
    }

    pub fn field_get_minsize<F: SbField + ?Sized>(&mut self, min_u64s: u32) -> Option<&mut F> {
        sb_field_get_minsize::<F>(self, min_u64s)
    }

    pub fn field_delete(&mut self, ty: c::bch_sb_field_type) {
        unsafe { c::bch2_sb_field_delete(self, ty) }
    }

    /// @u64s 0 for the minimum.
    pub fn sb_realloc(&mut self, u64s: u32) -> Result<(), i32> {
        match unsafe { c::bch2_sb_realloc(self, u64s) } {
            0 => Ok(()),
            e => Err(e),
        }
    }

    /// Copy members_v2 into the members_v1 mirror, for old kernels.
    pub fn members_cpy_v2_v1(&mut self) {
        unsafe { c::bch2_sb_members_cpy_v2_v1(self) };
    }

    /// A disk path is a label group.
    pub fn disk_path_find(&mut self, name: &core::ffi::CStr) -> Option<u32> {
        let v = unsafe { c::bch2_disk_path_find(self, name.as_ptr().cast()) };
        (v >= 0).then_some(v as u32)
    }

    pub fn disk_path_find_or_create(&mut self, name: &core::ffi::CStr) -> Result<u32, i32> {
        let v = unsafe { c::bch2_disk_path_find_or_create(self, name.as_ptr().cast()) };
        if v >= 0 { Ok(v as u32) } else { Err(-v) }
    }

    /// The superblock's full vstruct extent as bytes.
    pub fn sb_bytes(&self) -> &[u8] {
        self.sb().as_bytes()
    }

    /// Its header - C's struct bch_sb, every field at its offset - edited as
    /// bytes by @f: kvdb's sb set. Not its count, u64s, which changes only
    /// with what it counts: an error if @f changed it, the header left as it
    /// was.
    pub fn edit_header<R>(&mut self, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, &'static str> {
        const N: usize = core::mem::size_of::<Head<bch_sb>>();
        let mut h: [u8; N] = self.sb_bytes()[..N].try_into().unwrap();
        let r = f(&mut h);
        if read_past::<Head<bch_sb>>(&h, 0).u64s() != self.sb().u64s() {
            return Err("u64s is the superblock's count: it changes with the fields it counts");
        }
        crate::util::vstructs::bytes_of_mut(self.sb_mut())[..N].copy_from_slice(&h);
        Ok(r)
    }

    /// Member @idx, to edit: None if there's no such member in the section,
    /// or it's shorter than a bch_member - written before it grew - which a
    /// write to would run past, into the next member.
    pub fn member_mut(&mut self, idx: u32) -> Option<&mut bch_member> {
        let nr = self.sb().nr_devices as u32;
        let mi = self.field::<c::bch_sb_field_members_v2>()?;
        let member_bytes = mi.member_bytes.get() as usize;
        let end = core::mem::offset_of!(Head<c::bch_sb_field_members_v2>, _members) + (idx as usize + 1) * member_bytes;
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

// ── Sections, through a bch_sb_handle ────────────────────────────────────
//
// Resizing takes &mut bch_sb_handle - it can reallocate the superblock - so
// no section borrowed from the handle outlives it.

/// The superblock's section of type @F, if it has one, over the bytes its
/// header says are its - one written before F grew reads as zero past them.
pub fn sb_field_get<F: SbField + ?Sized>(sb: &c::bch_sb) -> Option<&F> {
    let f = unsafe { c::bch2_sb_field_get_id(sb.as_ffi() as *mut _, F::FIELD_TYPE) };
    // SAFETY: C's walk of the sections the superblock's count says it has -
    // validated, so the section and its bytes are there
    (!f.is_null()).then(|| unsafe { c::bch_sb_field::<Validated>::from_ffi(f) })?.arm::<F>()
}

/// To edit: a field past the section's end is no write it will carry -
/// whole_mut() for all of it, if it's all there.
pub fn sb_field_get_mut<F: SbField + ?Sized>(disk_sb: &mut c::bch_sb_handle) -> Option<&mut F> {
    let f = unsafe { c::bch2_sb_field_get_id(disk_sb.sb, F::FIELD_TYPE) };
    // SAFETY: as sb_field_get()'s
    (!f.is_null()).then(|| unsafe { c::bch_sb_field::<Validated>::from_ffi_mut(f) })?.arm_mut::<F>()
}

/// Resize the section of type @F to @u64s, making it if there's none: the
/// section, at its (possibly new) location - whole_mut() if that's all of F.
pub fn sb_field_resize<F: SbField + ?Sized>(
    disk_sb: &mut c::bch_sb_handle,
    u64s: u32,
) -> Option<&mut F> {
    let f = unsafe { c::bch2_sb_field_resize_id(disk_sb, F::FIELD_TYPE, u64s) };
    // SAFETY: C's resize, which reallocates the superblock if it has to
    (!f.is_null()).then(|| unsafe { c::bch_sb_field::<Validated>::from_ffi_mut(f) })?.arm_mut::<F>()
}

/// The section of type @F, made or grown to at least @min_u64s.
pub fn sb_field_get_minsize<F: SbField + ?Sized>(
    disk_sb: &mut c::bch_sb_handle,
    min_u64s: u32,
) -> Option<&mut F> {
    let f = unsafe { c::bch2_sb_field_get_minsize_id(disk_sb, F::FIELD_TYPE, min_u64s) };
    // SAFETY: as sb_field_resize()'s
    (!f.is_null()).then(|| unsafe { c::bch_sb_field::<Validated>::from_ffi_mut(f) })?.arm_mut::<F>()
}

// ── Superblocks in byte buffers ──────────────────────────────────────────
//
// For device scans and recovery, without a bch_sb_handle. Construction checks
// every extent once - the header and vstruct within the buffer, each section
// within the superblock - so the C lookups after walk only what parse()
// checked. That's memory safety only: whether the contents make sense is
// bch2_sb_validate()'s. A buffer can't be reallocated, so resizing and
// deleting sections stay bch_sb_handle's.

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
    let hdr = core::mem::size_of::<Head<bch_sb>>();
    if buf.len() < hdr {
        return Err(SbParseError::BufferTooSmall { need: hdr, have: buf.len() });
    }
    if buf.as_ptr() as usize % core::mem::align_of::<Head<bch_sb>>() != 0 {
        return Err(SbParseError::Misaligned);
    }
    /* header extent and alignment just validated: */
    let sb = unsafe { &*(buf.as_ptr() as *const Head<bch_sb>) };

    if sb.magic.b != BCACHE_MAGIC && sb.magic.b != BCHFS_MAGIC {
        return Err(SbParseError::BadMagic);
    }

    let bytes = sb.bytes();
    if bytes > buf.len() {
        return Err(SbParseError::ExtentBeyondBuffer { need: bytes, have: buf.len() });
    }

    let mut off = hdr;
    while off < bytes {
        if bytes - off < core::mem::size_of::<Head<c::bch_sb_field>>() {
            return Err(SbParseError::FieldBeyondSb { field_offset: off });
        }
        /* field header extent just validated: */
        let f = unsafe { &*(buf.as_ptr().add(off) as *const Head<c::bch_sb_field>) };
        let f_u64s = f.u64s() as u64;
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
        // SAFETY: parse() checked every count it has
        unsafe { bch_sb::<Validated>::from_ffi(self.buf.as_ptr().cast()) }
    }

    pub fn bytes(&self) -> &'a [u8] {
        &self.buf[..self.bytes]
    }

    pub fn field<F: SbField + ?Sized>(&self) -> Option<&'a F> {
        sb_field_get::<F>(self.sb())
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
        // SAFETY: parse() checked every count it has
        unsafe { bch_sb::<Validated>::from_ffi(self.buf.as_ptr().cast()) }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        // SAFETY: as sb()'s
        unsafe { bch_sb::<Validated>::from_ffi_mut(self.buf.as_mut_ptr().cast()) }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.bytes]
    }

    pub fn field<F: SbField + ?Sized>(&self) -> Option<&F> {
        sb_field_get::<F>(self.sb())
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

/// An owned superblock: aligned storage, extents validated at construction -
/// for recovery, which carries superblocks around as values.
#[cfg(feature = "std")]
pub struct SbBuf {
    buf:   Vec<u64>,
    bytes: usize,
}

#[cfg(feature = "std")]
impl SbBuf {
    /// @src needn't be aligned.
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
        // SAFETY: parse() checked every count it has
        unsafe { bch_sb::<Validated>::from_ffi(self.buf.as_ptr().cast()) }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        // SAFETY: as sb()'s
        unsafe { bch_sb::<Validated>::from_ffi_mut(self.buf.as_mut_ptr().cast()) }
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr() as *const u8, self.bytes) }
    }

    pub fn field<F: SbField + ?Sized>(&self) -> Option<&F> {
        sb_field_get::<F>(self.sb())
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
    use crate::util::vstructs::{Head, Unvalidated, Validated, VStruct, VStructError};

    /// @buf as a superblock field of type @ty, @u64s long - its header written
    /// - as a superblock's walk would give it: as many bytes as it says.
    fn field(buf: &mut [u64], ty: c::bch_sb_field_type, u64s: usize) -> &c::bch_sb_field {
        let f = buf.as_mut_ptr().cast::<Head<c::bch_sb_field>>();
        // SAFETY: u64s, which any bytes are, aligned for a bch_sb_field, and
        // as many as u64s says
        unsafe {
            (*f).set_val_u64s(u64s);
            (*f).type_ = Le32::new(ty.0);
            c::bch_sb_field::<Validated>::from_ffi(f)
        }
    }

    /// A superblock field is the section its type says - and one this code
    /// doesn't know is its type, as an error.
    #[test]
    fn sb_field_get() {
        type T = c::bch_sb_field_type;
        let mut buf = [0u64; 4];

        let f = field(&mut buf, T::BCH_SB_FIELD_journal, 3);
        assert_eq!(f.as_bytes().len(), 24);
        assert!(matches!(f.typed(), Ok(c::BchSbFieldRef::journal(j)) if j.field.u64s() == 3 && j.buckets.len() == 2));

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
        assert_eq!(e.errors_silent[0].get(), 0x5a);
        assert_eq!(e.btrees_lost_data().get(), 0);
        assert_eq!(e.btrees_clean().get(), 0);
        assert!(e.whole().is_none());
        assert_eq!((e.as_bytes().len(), size_of_val(e)), (88, 88));

        // and as the one arm sb_field_get() asks for
        let e = f.arm::<c::bch_sb_field_ext<Validated>>().expect("an ext section");
        assert_eq!((e.errors_silent[0].get(), e.btrees_lost_data().get()), (0x5a, 0));
        assert!(f.arm::<c::bch_sb_field_crypt>().is_none());
    }

    /// A clean section is its header, then journal entries to its end - each
    /// as long as it says.
    #[test]
    fn sb_field_clean_entries() {
        let mut buf = [0u64; 6];
        buf[2] = 42;                            // journal_seq
        buf[3] = 1 | 7 << 32;                   // an entry: u64s 1, type 7
        let f = field(&mut buf, c::bch_sb_field_type::BCH_SB_FIELD_clean, 6);

        let Ok(c::BchSbFieldRef::clean(clean)) = f.typed() else { panic!("a clean section") };
        assert_eq!((clean.journal_seq.get(), clean.as_bytes().len()), (42, 48));
        let es: Vec<_> = clean.entries().map(|e| (e.u64s(), e.type_)).collect();
        assert_eq!(es, [(1, 7), (0, 0)]);
    }

    /// A section is in its superblock field's state, and so are its members:
    /// the same clean section, Unvalidated, checks each entry and says which
    /// overruns it; Validated, trusting its counts as C does, just ends there.
    #[test]
    fn sb_field_state_reaches_its_members() {
        let mut buf = [0u64; 6];
        buf[2] = 42;                            // journal_seq
        buf[3] = 1;                             // an entry of 1 u64
        buf[5] = 5;                             // one of 5, past the section's end
        let f = field(&mut buf, c::bch_sb_field_type::BCH_SB_FIELD_clean, 6);
        let Ok(c::BchSbFieldRef::clean(clean)) = f.typed() else { panic!("a clean section") };
        assert_eq!(clean.entries().count(), 1);

        // SAFETY: u64s, which any bytes are
        let b = unsafe { core::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len() * 8) };
        let f = c::bch_sb_field::<Unvalidated>::from_bytes(b).unwrap();
        let Ok(c::BchSbFieldRef::clean(clean)) = f.typed() else { panic!("a clean section") };
        let es: Vec<_> = clean.entries().collect();
        assert!(matches!(es[..], [Ok(_), Err(VStructError::Overrun { want: 48, have: 8, .. })]));
        assert_eq!(clean.journal_seq.get(), 42);

        // one arm, named in the field's state - c::bch_sb_field_clean alone,
        // the Validated one, isn't an arm of an Unvalidated field
        assert!(f.arm::<c::bch_sb_field_clean<Unvalidated>>().is_some());
    }

    /// A whole ext section is all of it, every field a field; one shorter
    /// than every valid one isn't one at all.
    #[test]
    fn sb_field_ext_whole_and_too_short() {
        let mut buf = [0u64; 16];
        buf[12] = 7;                            // flags0
        let f = field(&mut buf, c::bch_sb_field_type::BCH_SB_FIELD_ext, 15);
        let e = f.arm::<c::bch_sb_field_ext<Validated>>().expect("an ext section");
        let w = e.whole().expect("all of it");
        assert_eq!((w.flags0.get(), e.flags0().get(), size_of_val(e)), (7, 7, 120));

        let mut buf = [0u64; 16];
        let f = field(&mut buf, c::bch_sb_field_type::BCH_SB_FIELD_ext, 10);
        assert!(f.arm::<c::bch_sb_field_ext<Validated>>().is_none());
        assert!(f.typed().is_err());
    }

    #[test]
    fn zstd_early_abort_and_write_degraded_flags_are_independent() {
        use crate::util::vstructs::Head;

        // Bit 63 and the low word stand in for neighbouring fields: neither
        // setter may disturb anything outside its own [start, end).
        let untouched = (1u64 << 63) | 0xdead_beef;
        for action in 0..=3u64 {
            for enabled in [false, true] {
                let mut sb = Head::<c::bch_sb>::default();
                sb.flags[6].set(untouched);
                sb.set_write_degraded_action(action);
                sb.set_zstd_compression_early_abort(enabled);
                assert_eq!(
                    sb.flags[6].get(),
                    untouched | (action << 60) | ((enabled as u64) << 62)
                );
                assert_eq!(sb.write_degraded_action(), action);
                sb.set_write_degraded_action(3 - action);
                assert_eq!(sb.zstd_compression_early_abort(), enabled);
                assert_eq!(
                    sb.flags[6].get(),
                    untouched | ((3 - action) << 60) | ((enabled as u64) << 62)
                );
            }
        }
    }
}
