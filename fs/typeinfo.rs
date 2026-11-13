// SPDX-License-Identifier: GPL-2.0

//! Runtime type information for the C on-disk structures.
//!
//! Static per-type field tables — name, offset, width, endianness — for the
//! on-disk structs, produced by `#[derive(TypeInfo)]`
//! (fs/cstruct-macros/src/typeinfo.rs) on their Rust definitions. Offsets come
//! from `offset_of!`, so the compiler guarantees them against the real layout;
//! width, endianness and signedness are read from the C type a field renders
//! as (`Le32` is `__le32`) - the syntax, which is why this is a derive macro
//! and not a generic reflection library.
//!
//! Primary consumer: the btree REPL (`bcachefs kvdb`), which reads and writes
//! btree keys by field name — both for field debugging and for corruption
//! injection in fsck/repair tests. The tables are plain `'static` const data,
//! no_std, usable from the kernel build too.
//!
//! Scope and limits:
//! - Fixed-layout structs are fully described (the snapshots/subvolumes world,
//!   most val types).
//! - Unions and enums degrade to size-only opaque entries; pointers, function
//!   pointers and kernel-internal types degrade to `Opaque` fields. A field's
//!   byte extent is always recoverable from the *next* field's offset, so
//!   opaque fields can still be hexdumped, just not interpreted.
//! - Varint-packed types (`bch_inode`) and entry-stream vals (`bch_extent`)
//!   are only described up to their fixed header; the variable part shows up
//!   as a `VarTail`. Editing those needs the real pack/unpack helpers, not
//!   reflection.
//!
//! A #[bitfield]'s members are part of its type: its TypeInfo describes each
//! (`Shape::Bitfield`, `FieldKind::Bits`), and a field of it resolves as its
//! bit range - `flags0.clean`, or bare, `clean`, when no field has the name.

/// Byte order of an integer field. `Native` is for in-memory types bound as
/// plain `u32`/`u64`; on-disk formats always use explicit `Little`/`Big`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Struct,
    Union,
    Enum,
    /// A #[bitfield]: a word, in that byte order, its fields its members'
    /// bits of it.
    Bitfield(Endian),
}

pub struct StructInfo {
    pub name: &'static str,
    pub size: usize,
    pub shape: Shape,
    pub fields: &'static [FieldInfo],
}

impl StructInfo {
    /// Where its field @name is: a top level one, not a nested struct's.
    pub const fn field_offset(&self, name: &str) -> Option<usize> {
        let mut i = 0;
        while i < self.fields.len() {
            if str_eq(self.fields[i].name, name) {
                return Some(self.fields[i].offset);
            }
            i += 1;
        }
        None
    }
}

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

pub struct FieldInfo {
    pub name: &'static str,
    pub offset: usize,
    pub kind: FieldKind,
}

pub enum FieldKind {
    Int {
        bytes: u8,
        endian: Endian,
        signed: bool,
    },
    Struct(&'static StructInfo),
    Array {
        elem: &'static FieldKind,
        n: usize,
        stride: usize,
    },
    /// C flexible array member: length is whatever remains of the value.
    VarTail {
        elem: &'static FieldKind,
        stride: usize,
    },
    /// Not interpretable (pointer, union contents, kernel-internal type).
    /// Byte extent is derived from the following field's offset.
    Opaque,
    /// A #[bitfield]'s member: bits @lo..@hi of the word at its offset.
    Bits {
        bytes: u8,
        endian: Endian,
        lo: u8,
        hi: u8,
    },
}

pub trait TypeInfo {
    const INFO: &'static StructInfo;
}

// ---------------------------------------------------------------------------
// Path resolution: "children[1]", "btime.hi", ...

/// A resolved field: byte offset from the start of the struct, the field's
/// kind, and its byte extent (0 for a `VarTail`, which runs to the end of the
/// value; derived from the next field's offset for `Opaque`).
pub struct FieldRef {
    pub offset: usize,
    pub kind: &'static FieldKind,
    pub len: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError<'p> {
    NoSuchField { st: &'static str, field: &'p str },
    NotAStruct { field: &'p str },
    NotAnArray { field: &'p str },
    IndexOutOfBounds { field: &'p str, idx: usize, n: usize },
    BadSyntax { at: &'p str },
}

impl core::fmt::Display for ResolveError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ResolveError::NoSuchField { st, field } => {
                write!(f, "{st} has no field '{field}'")
            }
            ResolveError::NotAStruct { field } => {
                write!(f, "'{field}' is not a struct, can't access subfields")
            }
            ResolveError::NotAnArray { field } => {
                write!(f, "'{field}' is not an array, can't index")
            }
            ResolveError::IndexOutOfBounds { field, idx, n } => {
                write!(f, "'{field}[{idx}]' out of bounds, array has {n} elements")
            }
            ResolveError::BadSyntax { at } => write!(f, "bad field path syntax at '{at}'"),
        }
    }
}

fn kind_len(kind: &FieldKind, bound: usize) -> usize {
    match kind {
        FieldKind::Int { bytes, .. } | FieldKind::Bits { bytes, .. } => *bytes as usize,
        FieldKind::Struct(s) => s.size,
        FieldKind::Array { n, stride, .. } => n * stride,
        FieldKind::VarTail { .. } => 0,
        FieldKind::Opaque => bound,
    }
}

/// Resolve a field path like `children[1]` or `btime.hi` against a struct's
/// type info, yielding the field's offset and kind.
pub fn resolve<'p>(
    info: &'static StructInfo,
    path: &'p str,
) -> Result<FieldRef, ResolveError<'p>> {
    let mut st = info;
    let mut base = 0usize;
    let mut segments = path.split('.').peekable();

    loop {
        let Some(seg) = segments.next() else {
            return Err(ResolveError::BadSyntax { at: path });
        };

        // Split the segment into a field name and any [idx] suffixes.
        let (name, mut idx_part) = match seg.find('[') {
            Some(i) => (&seg[..i], &seg[i..]),
            None => (seg, ""),
        };
        if name.is_empty() {
            return Err(ResolveError::BadSyntax { at: seg });
        }

        let field = st
            .fields
            .iter()
            .position(|f| f.name == name)
            .map(|i| &st.fields[i])
            .ok_or(ResolveError::NoSuchField { st: st.name, field: name })?;

        let mut offset = base + field.offset;
        let mut kind = &field.kind;
        // Opaque extent: up to the next field in this struct (or its end).
        let mut len = kind_len(
            kind,
            st.fields
                .iter()
                .map(|f| f.offset)
                .filter(|&o| o > field.offset)
                .min()
                .unwrap_or(st.size)
                - field.offset,
        );

        while !idx_part.is_empty() {
            let Some(rest) = idx_part.strip_prefix('[') else {
                return Err(ResolveError::BadSyntax { at: seg });
            };
            let Some(close) = rest.find(']') else {
                return Err(ResolveError::BadSyntax { at: seg });
            };
            let idx: usize = rest[..close]
                .parse()
                .map_err(|_| ResolveError::BadSyntax { at: seg })?;
            idx_part = &rest[close + 1..];

            let (elem, stride) = match kind {
                FieldKind::Array { elem, n, stride } => {
                    if idx >= *n {
                        return Err(ResolveError::IndexOutOfBounds {
                            field: name,
                            idx,
                            n: *n,
                        });
                    }
                    (*elem, *stride)
                }
                // VarTail length isn't known statically; bounds-checked
                // against the value's size at access time instead.
                FieldKind::VarTail { elem, stride } => (*elem, *stride),
                _ => return Err(ResolveError::NotAnArray { field: name }),
            };
            offset += idx * stride;
            kind = elem;
            len = kind_len(kind, stride);
        }

        if segments.peek().is_none() {
            return Ok(FieldRef { offset, kind, len });
        }

        match kind {
            FieldKind::Struct(inner) => {
                st = inner;
                base = offset;
            }
            _ => return Err(ResolveError::NotAStruct { field: name }),
        }
    }
}

/// Resolve a path that names a field (`parent`, `btime.hi`, `flags0.clean`) -
/// or, bare, a #[bitfield] field's member (`clean`), as a member's accessors
/// are the struct's. A field wins the bare name; a member whose name collides
/// with one stays reachable qualified.
pub fn resolve_with_bits<'p>(
    info: &'static StructInfo,
    path: &'p str,
) -> Result<FieldRef, ResolveError<'p>> {
    let err = match resolve(info, path) {
        Ok(r) => return Ok(r),
        Err(e) => e,
    };

    if !path.contains('.') {
        let member = info.fields.iter().find_map(|f| match &f.kind {
            FieldKind::Struct(w) if matches!(w.shape, Shape::Bitfield(_)) =>
                w.fields.iter().find(|m| m.name == path).map(|m| (f.offset, m)),
            _ => None,
        });
        if let Some((offset, m)) = member {
            return Ok(FieldRef { offset, kind: &m.kind, len: kind_len(&m.kind, 0) });
        }
    }

    Err(err)
}

// ---------------------------------------------------------------------------
// Scalar access on raw value bytes

#[derive(Debug, PartialEq, Eq)]
pub enum AccessError {
    /// Field lies beyond the end of the value. Not necessarily corruption:
    /// values are allowed to be shorter than the current struct definition
    /// (older format versions); extend the value to write such a field.
    OutOfBounds { need: usize, have: usize },
    NotScalar,
    Overflow { bytes: u8, val: u64 },
    BitsOverflow { bits: u8, val: u64 },
}

impl core::fmt::Display for AccessError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AccessError::OutOfBounds { need, have } => {
                write!(f, "field needs {need} bytes but value has {have}")
            }
            AccessError::NotScalar => write!(f, "not a scalar field"),
            AccessError::Overflow { bytes, val } => {
                write!(f, "value {val} doesn't fit in {bytes} bytes")
            }
            AccessError::BitsOverflow { bits, val } => {
                write!(f, "value {val} doesn't fit in {bits} bits")
            }
        }
    }
}

fn is_le(endian: Endian) -> bool {
    match endian {
        Endian::Little => true,
        Endian::Big => false,
        Endian::Native => cfg!(target_endian = "little"),
    }
}

/// A #[bitfield]'s word, as a field: its width and byte order - read and
/// written whole as the integer it is, as its members are bits of it.
fn bitfield_word(kind: &FieldKind) -> Option<(u8, Endian)> {
    match kind {
        FieldKind::Struct(StructInfo { shape: Shape::Bitfield(endian), size, .. }) if *size <= 8 =>
            Some((*size as u8, *endian)),
        _ => None,
    }
}

/// Read a scalar field's raw bits (not sign-extended; see [`sign_extend`]).
pub fn read_scalar(buf: &[u8], r: &FieldRef) -> Result<u64, AccessError> {
    match *r.kind {
        FieldKind::Int { bytes, endian, .. } => read_word(buf, r.offset, bytes, endian),
        FieldKind::Bits { bytes, endian, lo, hi } =>
            Ok((read_word(buf, r.offset, bytes, endian)? >> lo) & bits_mask(lo, hi)),
        ref k => match bitfield_word(k) {
            Some((bytes, endian)) => read_word(buf, r.offset, bytes, endian),
            None => Err(AccessError::NotScalar),
        },
    }
}

/// The mask of a bit range @lo..@hi's width.
fn bits_mask(lo: u8, hi: u8) -> u64 {
    let width = hi - lo;
    if width >= 64 { !0 } else { (1u64 << width) - 1 }
}

/// The @bytes wide integer at @offset, in byte order @endian.
fn read_word(buf: &[u8], offset: usize, bytes: u8, endian: Endian) -> Result<u64, AccessError> {
    let b = bytes as usize;
    let end = offset + b;
    if end > buf.len() {
        return Err(AccessError::OutOfBounds { need: end, have: buf.len() });
    }

    let mut tmp = [0u8; 8];
    if is_le(endian) {
        tmp[..b].copy_from_slice(&buf[offset..end]);
        Ok(u64::from_le_bytes(tmp))
    } else {
        tmp[8 - b..].copy_from_slice(&buf[offset..end]);
        Ok(u64::from_be_bytes(tmp))
    }
}

/// read_word(), written: @v fits in @bytes.
fn write_word(buf: &mut [u8], offset: usize, bytes: u8, endian: Endian, v: u64) -> Result<(), AccessError> {
    let b = bytes as usize;
    let end = offset + b;
    if end > buf.len() {
        return Err(AccessError::OutOfBounds { need: end, have: buf.len() });
    }

    if is_le(endian) {
        buf[offset..end].copy_from_slice(&v.to_le_bytes()[..b]);
    } else {
        buf[offset..end].copy_from_slice(&v.to_be_bytes()[8 - b..]);
    }
    Ok(())
}

pub fn sign_extend(v: u64, bytes: u8) -> i64 {
    let shift = 64 - 8 * bytes as u32;
    ((v << shift) as i64) >> shift
}

/// Write a scalar field. `v` holds the raw bits: for a signed field, pass the
/// two's-complement value (`-5i64 as u64`).
pub fn write_scalar(buf: &mut [u8], r: &FieldRef, v: u64) -> Result<(), AccessError> {
    let (bytes, endian, signed) = match *r.kind {
        FieldKind::Int { bytes, endian, signed } => (bytes, endian, signed),
        // read-modify-write of the word; an oversized value is an error, not
        // masked as C's SET_*() did - an injection tool mustn't corrupt
        // differently than asked
        FieldKind::Bits { bytes, endian, lo, hi } => {
            let mask = bits_mask(lo, hi);
            if v & !mask != 0 {
                return Err(AccessError::BitsOverflow { bits: hi - lo, val: v });
            }
            let old = read_word(buf, r.offset, bytes, endian)?;
            return write_word(buf, r.offset, bytes, endian, old & !(mask << lo) | (v << lo));
        }
        ref k => match bitfield_word(k) {
            Some((bytes, endian)) => (bytes, endian, false),
            None => return Err(AccessError::NotScalar),
        },
    };
    let fits = if signed {
        sign_extend(v, bytes) as u64 == v
    } else {
        bytes == 8 || v >> (8 * bytes) == 0
    };
    if !fits {
        return Err(AccessError::Overflow { bytes, val: v });
    }
    write_word(buf, r.offset, bytes, endian, v)
}

// ---------------------------------------------------------------------------
// Text output: dump every field of a value

fn int_to_text(
    out: &mut dyn core::fmt::Write,
    buf: &[u8],
    offset: usize,
    kind: &'static FieldKind,
) -> core::fmt::Result {
    let r = FieldRef { offset, kind, len: 0 };
    match read_scalar(buf, &r) {
        Ok(v) => {
            let FieldKind::Int { bytes, signed, .. } = kind else { unreachable!() };
            if *signed {
                write!(out, "{}", sign_extend(v, *bytes))
            } else {
                write!(out, "{v}")
            }
        }
        Err(_) => write!(out, "(beyond end of value)"),
    }
}

/// The members of @w set in word @v: ` (subvol|state=1)`.
fn set_bits_to_text(
    out: &mut dyn core::fmt::Write,
    v: u64,
    w: &'static StructInfo,
) -> core::fmt::Result {
    let bits = w.fields.iter().filter_map(|m| match m.kind {
        FieldKind::Bits { lo, hi, .. } => Some((m.name, lo, hi)),
        _ => None,
    });
    let mut first = true;
    for (name, lo, hi) in bits {
        let bits = (v >> lo) & bits_mask(lo, hi);
        if bits == 0 {
            continue;
        }
        write!(out, "{}", if first { " (" } else { "|" })?;
        first = false;
        if hi - lo == 1 {
            write!(out, "{name}")?;
        } else {
            write!(out, "{name}={bits}")?;
        }
    }
    if !first {
        write!(out, ")")?;
    }
    Ok(())
}

/// A #[bitfield] field: its word, and its members that are set - or, a word
/// kvdb can't read, a u128's, its bytes.
fn bitfield_to_text(
    out: &mut dyn core::fmt::Write,
    w: &'static StructInfo,
    buf: &[u8],
    offset: usize,
) -> core::fmt::Result {
    let Some((bytes, endian)) = bitfield_word(&FieldKind::Struct(w)) else {
        for b in buf.get(offset..(offset + w.size).min(buf.len())).unwrap_or_default() {
            write!(out, "{b:02x}")?;
        }
        return Ok(());
    };
    let Ok(v) = read_word(buf, offset, bytes, endian) else {
        return write!(out, "(beyond end of value)");
    };
    write!(out, "{v}")?;
    set_bits_to_text(out, v, w)
}

fn fields_to_text(
    out: &mut dyn core::fmt::Write,
    info: &'static StructInfo,
    buf: &[u8],
    base: usize,
    indent: usize,
) -> core::fmt::Result {
    for f in info.fields {
        // bch_val and friends: zero-size markers, nothing to show (their
        // flexible-array member would misreport the rest of the value).
        if matches!(&f.kind, FieldKind::Struct(inner) if inner.size == 0) {
            continue;
        }
        let offset = base + f.offset;
        write!(out, "{:indent$}{}: ", "", f.name)?;
        match &f.kind {
            FieldKind::Int { .. } => {
                int_to_text(out, buf, offset, &f.kind)?;
                writeln!(out)?;
            }
            FieldKind::Struct(w) if matches!(w.shape, Shape::Bitfield(_)) => {
                bitfield_to_text(out, w, buf, offset)?;
                writeln!(out)?;
            }
            FieldKind::Struct(inner) => {
                writeln!(out)?;
                fields_to_text(out, inner, buf, offset, indent + 2)?;
            }
            FieldKind::Array { elem, n, stride } => match elem {
                FieldKind::Int { .. } => {
                    write!(out, "[")?;
                    for i in 0..*n {
                        if i != 0 {
                            write!(out, ", ")?;
                        }
                        int_to_text(out, buf, offset + i * stride, elem)?;
                    }
                    writeln!(out, "]")?;
                }
                FieldKind::Struct(inner) => {
                    writeln!(out)?;
                    for i in 0..*n {
                        writeln!(out, "{:indent$}[{i}]:", "", indent = indent + 2)?;
                        fields_to_text(out, inner, buf, offset + i * stride, indent + 4)?;
                    }
                }
                _ => writeln!(out, "(array of {n} opaque elements)")?,
            },
            FieldKind::VarTail { stride, .. } => {
                let n = buf.len().saturating_sub(offset) / stride.max(&1);
                writeln!(out, "({n} trailing elements)")?;
            }
            FieldKind::Bits { .. } => match read_scalar(buf, &FieldRef { offset, kind: &f.kind, len: 0 }) {
                Ok(v) => writeln!(out, "{v}")?,
                Err(_) => writeln!(out, "(beyond end of value)")?,
            },
            FieldKind::Opaque => {
                let end = info
                    .fields
                    .iter()
                    .map(|g| g.offset)
                    .filter(|&o| o > f.offset)
                    .min()
                    .unwrap_or(info.size)
                    + base;
                let end = end.min(buf.len());
                if offset >= end {
                    writeln!(out, "(opaque, beyond end of value)")?;
                } else {
                    for b in &buf[offset..end] {
                        write!(out, "{b:02x}")?;
                    }
                    writeln!(out)?;
                }
            }
        }
    }
    Ok(())
}

/// Print every field of a value, one per line, nested fields indented.
pub fn struct_to_text(
    out: &mut dyn core::fmt::Write,
    info: &'static StructInfo,
    buf: &[u8],
) -> core::fmt::Result {
    fields_to_text(out, info, buf, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::c;

    fn snapshot_info() -> &'static StructInfo {
        <c::Snapshot as TypeInfo>::INFO
    }

    #[test]
    fn snapshot_table() {
        let info = snapshot_info();
        assert_eq!(info.name, "Snapshot");
        assert_eq!(info.size, core::mem::size_of::<c::Snapshot>());
    }

    #[test]
    fn resolve_paths() {
        let info = snapshot_info();

        let r = resolve(info, "parent").unwrap();
        assert_eq!(r.offset, 4);
        assert!(matches!(
            r.kind,
            FieldKind::Int { bytes: 4, endian: Endian::Little, signed: false }
        ));

        let r = resolve(info, "children[1]").unwrap();
        assert_eq!(r.offset, 12);

        // nested struct access
        let r = resolve(info, "btime.hi").unwrap();
        assert_eq!(r.offset, 48);

        assert!(matches!(
            resolve(info, "children[2]"),
            Err(ResolveError::IndexOutOfBounds { idx: 2, n: 2, .. })
        ));
        assert!(matches!(
            resolve(info, "nonexistent"),
            Err(ResolveError::NoSuchField { .. })
        ));
        assert!(matches!(
            resolve(info, "parent.x"),
            Err(ResolveError::NotAStruct { .. })
        ));
    }

    /// A trailing `[T; 0]` is C's flexible array member, indexable past
    /// zero; anywhere else it's a zero-length array.
    #[test]
    fn flex_array_tail() {
        let info = <crate::util::vstructs::Head<c::bch_ioctl_fsck_offline> as TypeInfo>::INFO;
        let devs = info.fields.iter().find(|f| f.name == "devs").unwrap();
        assert!(matches!(devs.kind, FieldKind::VarTail { stride: 8, .. }));

        let r = resolve(info, "devs[1]").unwrap();
        assert_eq!(r.offset, 32);
        assert!(matches!(
            r.kind,
            FieldKind::Int { bytes: 8, endian: Endian::Native, signed: false }
        ));
    }

    #[test]
    fn scalar_roundtrip() {
        let info = snapshot_info();
        let mut buf = [0u8; core::mem::size_of::<c::Snapshot>()];

        let r = resolve(info, "parent").unwrap();
        write_scalar(&mut buf, &r, 249).unwrap();
        assert_eq!(read_scalar(&buf, &r).unwrap(), 249);
        assert_eq!(&buf[4..8], &249u32.to_le_bytes());

        assert!(matches!(
            write_scalar(&mut buf, &r, 1 << 33),
            Err(AccessError::Overflow { .. })
        ));

        // short value: fields beyond the end must not read or write
        let r = resolve(info, "subvol").unwrap();
        assert!(matches!(
            read_scalar(&buf[..8], &r),
            Err(AccessError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn to_text() {
        let info = snapshot_info();
        let mut buf = [0u8; core::mem::size_of::<c::Snapshot>()];
        let r = resolve(info, "parent").unwrap();
        write_scalar(&mut buf, &r, 249).unwrap();

        let mut s = String::new();
        struct_to_text(&mut s, info, &buf).unwrap();
        assert!(s.contains("parent: 249"), "{s}");
        assert!(s.contains("children: [0, 0]"), "{s}");
    }

    #[test]
    fn member_resolution() {
        let info = snapshot_info();

        // a bare member name resolves to its bits of the word
        let r = resolve_with_bits(info, "no_keys_obsolete").unwrap();
        assert!(matches!(r.kind, FieldKind::Bits { lo: 3, hi: 4, .. }));
        assert_eq!(r.offset, 0); // Snapshot.flags

        // qualified, through the word
        let r = resolve_with_bits(info, "flags.subvol_obsolete").unwrap();
        assert!(matches!(r.kind, FieldKind::Bits { lo: 1, hi: 2, .. }));

        assert!(resolve_with_bits(info, "flags.nonexistent").is_err());
    }

    #[test]
    fn member_roundtrip() {
        let info = snapshot_info();
        let mut buf = [0u8; core::mem::size_of::<c::Snapshot>()];

        let r = resolve_with_bits(info, "no_keys_obsolete").unwrap();
        write_scalar(&mut buf, &r, 1).unwrap();
        assert_eq!(read_scalar(&buf, &r).unwrap(), 1);
        assert_eq!(buf[0], 8); // NO_KEYS is bit 3

        // read-modify-write leaves neighbouring bits alone
        let r2 = resolve_with_bits(info, "flags.subvol_obsolete").unwrap();
        assert_eq!(read_scalar(&buf, &r2).unwrap(), 0);

        assert!(matches!(
            write_scalar(&mut buf, &r, 2),
            Err(AccessError::BitsOverflow { bits: 1, val: 2 })
        ));
    }

    #[test]
    fn bits_decode_display() {
        let info = snapshot_info();
        let mut buf = [0u8; core::mem::size_of::<c::Snapshot>()];
        buf[0] = 2 | 8; // SUBVOL_OBSOLETE | NO_KEYS_OBSOLETE

        let mut s = String::new();
        struct_to_text(&mut s, info, &buf).unwrap();
        assert!(s.contains("flags: 10 (subvol_obsolete|no_keys_obsolete)"), "{s}");
    }

    /// A #[bitfield]'s word reads and writes whole, as the integer it is, its
    /// members bits of it - as tests set an inode's bi_flags, a number.
    #[test]
    fn bitfield_word() {
        let info = <c::bch_sb__head as TypeInfo>::INFO;
        let mut buf = [0u8; core::mem::size_of::<c::bch_sb__head>()];
        let field = |path| resolve_with_bits(info, path).unwrap();
        let (word, clean, csum_type) = (field("flags0"), field("clean"), field("csum_type"));

        write_scalar(&mut buf, &word, 1 << 1 | 3 << 2).unwrap();
        assert_eq!(read_scalar(&buf, &clean).unwrap(), 1);
        assert_eq!(read_scalar(&buf, &csum_type).unwrap(), 3);

        write_scalar(&mut buf, &csum_type, 2).unwrap();
        assert_eq!(read_scalar(&buf, &word).unwrap(), 1 << 1 | 2 << 2);
        assert_eq!(&buf[word.offset..word.offset + 8], &(1u64 << 1 | 2 << 2).to_le_bytes());
    }

    #[test]
    fn typed_accessors() {
        let mut s: c::Snapshot = Default::default();
        s.set_no_keys_obsolete(true);
        assert!(s.no_keys_obsolete());
        assert_eq!(s.flags.into_bits().get(), 8);
        s.set_no_keys_obsolete(false);
        assert!(!s.no_keys_obsolete());
        assert_eq!(s.flags.into_bits().get(), 0);
    }
}
