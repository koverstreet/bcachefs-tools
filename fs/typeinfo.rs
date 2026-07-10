// SPDX-License-Identifier: GPL-2.0

//! Runtime type information for the C on-disk structures.
//!
//! Static per-type field tables — name, offset, width, endianness — for the
//! bindgen-generated structs, produced by `#[derive(TypeInfo)]`
//! (fs/typeinfo-macros/) which bindgen injects on every `bch_*`-family type
//! (see the `--with-derive-custom-*` flags in codegen.rs). Offsets come from
//! `offset_of!`, so the compiler guarantees them against the real layout;
//! endianness is recovered from the C typedef *name* (`__le32` resolves to
//! `u32`, so the information only exists at the syntax level — the reason
//! this is a derive macro and not a generic reflection library).
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
//! `BITMASK`/`LE*_BITMASK` bit ranges are described too, but through a
//! separate table (`BITMASK_FIELDS`): the macro invocations are freestanding,
//! not part of the struct definition, so codegen.rs scans the headers for
//! them rather than the derive. Bits resolve by bare name when it doesn't
//! collide with a field (`no_keys`) or qualified as `<field>.<bit>`
//! (`flags.subvol` — the `subvol` *field* wins the bare name).

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
}

pub struct StructInfo {
    pub name: &'static str,
    pub size: usize,
    pub shape: Shape,
    pub fields: &'static [FieldInfo],
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
}

pub trait TypeInfo {
    const INFO: &'static StructInfo;
}

/// A named bit range within an integer field, from a `BITMASK`/`LE*_BITMASK`
/// declaration. The instances live in the generated `BITMASK_FIELDS` table.
pub struct BitmaskField {
    pub struct_name: &'static str,
    /// Field path within the struct, as written in the declaration —
    /// resolvable, including an index (`"flags"`, `"flags[0]"`).
    pub field: &'static str,
    pub name: &'static str,
    pub lo: u8,
    pub hi: u8,
}

impl BitmaskField {
    pub fn mask(&self) -> u64 {
        let width = self.hi - self.lo;
        if width >= 64 { !0 } else { (1u64 << width) - 1 }
    }

    /// Does this bit range live in `field` (`idx`: array element)?
    fn matches_field(&self, name: &str, idx: Option<usize>) -> bool {
        match idx {
            None => self.field == name,
            Some(i) => self
                .field
                .strip_prefix(name)
                .and_then(|r| r.strip_prefix('['))
                .and_then(|r| r.strip_suffix(']'))
                .and_then(|n| n.parse::<usize>().ok())
                == Some(i),
        }
    }
}

/// All bit ranges declared on a struct.
pub fn bitmask_fields(
    struct_name: &str,
) -> impl Iterator<Item = &'static BitmaskField> + '_ {
    BITMASK_FIELDS.iter().filter(move |b| b.struct_name == struct_name)
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
        FieldKind::Int { bytes, .. } => *bytes as usize,
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

/// Resolve a path that may name either a field (`parent`, `btime.hi`) or a
/// declared bit range (`no_keys`, `flags.subvol`). Fields always win the
/// bare name; a bit whose name collides with a field stays reachable in the
/// qualified `<field>.<bit>` form.
pub fn resolve_with_bits<'p>(
    info: &'static StructInfo,
    path: &'p str,
) -> Result<(FieldRef, Option<&'static BitmaskField>), ResolveError<'p>> {
    let err = match resolve(info, path) {
        Ok(r) => return Ok((r, None)),
        Err(e) => e,
    };

    // bare bit name: "no_keys"
    if !path.contains('.') {
        if let Some(bm) = bitmask_fields(info.name).find(|b| b.name == path) {
            return Ok((resolve(info, bm.field)?, Some(bm)));
        }
    }

    // qualified: "<field>.<bit>", field as written in the declaration
    // (so "flags[0].clean" for a bit of an array element)
    if let Some((field, bit)) = path.rsplit_once('.') {
        if let Some(bm) = bitmask_fields(info.name)
            .find(|b| b.field == field && b.name == bit)
        {
            return Ok((resolve(info, bm.field)?, Some(bm)));
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

/// Read a scalar field's raw bits (not sign-extended; see [`sign_extend`]).
pub fn read_scalar(buf: &[u8], r: &FieldRef) -> Result<u64, AccessError> {
    let FieldKind::Int { bytes, endian, .. } = r.kind else {
        return Err(AccessError::NotScalar);
    };
    let b = *bytes as usize;
    let end = r.offset + b;
    if end > buf.len() {
        return Err(AccessError::OutOfBounds { need: end, have: buf.len() });
    }

    let mut tmp = [0u8; 8];
    if is_le(*endian) {
        tmp[..b].copy_from_slice(&buf[r.offset..end]);
        Ok(u64::from_le_bytes(tmp))
    } else {
        tmp[8 - b..].copy_from_slice(&buf[r.offset..end]);
        Ok(u64::from_be_bytes(tmp))
    }
}

pub fn sign_extend(v: u64, bytes: u8) -> i64 {
    let shift = 64 - 8 * bytes as u32;
    ((v << shift) as i64) >> shift
}

/// Write a scalar field. `v` holds the raw bits: for a signed field, pass the
/// two's-complement value (`-5i64 as u64`).
pub fn write_scalar(buf: &mut [u8], r: &FieldRef, v: u64) -> Result<(), AccessError> {
    let FieldKind::Int { bytes, endian, signed } = r.kind else {
        return Err(AccessError::NotScalar);
    };
    let b = *bytes as usize;
    let end = r.offset + b;
    if end > buf.len() {
        return Err(AccessError::OutOfBounds { need: end, have: buf.len() });
    }

    let fits = if *signed {
        sign_extend(v, *bytes) as u64 == v
    } else {
        b == 8 || v >> (8 * b) == 0
    };
    if !fits {
        return Err(AccessError::Overflow { bytes: *bytes, val: v });
    }

    if is_le(*endian) {
        buf[r.offset..end].copy_from_slice(&v.to_le_bytes()[..b]);
    } else {
        buf[r.offset..end].copy_from_slice(&v.to_be_bytes()[8 - b..]);
    }
    Ok(())
}

/// Read a declared bit range of a scalar field.
pub fn read_bits(buf: &[u8], r: &FieldRef, bm: &BitmaskField) -> Result<u64, AccessError> {
    Ok((read_scalar(buf, r)? >> bm.lo) & bm.mask())
}

/// Write a declared bit range, read-modify-write on the containing field.
/// Unlike the C `SET_*` macros (which silently mask), an oversized value is
/// an error — an injection tool must not corrupt differently than asked.
pub fn write_bits(
    buf: &mut [u8],
    r: &FieldRef,
    bm: &BitmaskField,
    v: u64,
) -> Result<(), AccessError> {
    if v & !bm.mask() != 0 {
        return Err(AccessError::BitsOverflow { bits: bm.hi - bm.lo, val: v });
    }
    let old = read_scalar(buf, r)?;
    write_scalar(buf, r, old & !(bm.mask() << bm.lo) | (v << bm.lo))
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

/// Append a field's declared bits, decoded: ` (subvol|no_keys)`,
/// ` (state=1|durability=2)`. Prints nothing when no declared bit is set.
fn bits_to_text(
    out: &mut dyn core::fmt::Write,
    info: &'static StructInfo,
    name: &str,
    idx: Option<usize>,
    buf: &[u8],
    offset: usize,
    kind: &'static FieldKind,
) -> core::fmt::Result {
    let r = FieldRef { offset, kind, len: 0 };
    let Ok(v) = read_scalar(buf, &r) else { return Ok(()) };

    let mut first = true;
    for bm in bitmask_fields(info.name) {
        if !bm.matches_field(name, idx) {
            continue;
        }
        let bits = (v >> bm.lo) & bm.mask();
        if bits == 0 {
            continue;
        }
        write!(out, "{}", if first { " (" } else { "|" })?;
        first = false;
        if bm.hi - bm.lo == 1 {
            write!(out, "{}", bm.name)?;
        } else {
            write!(out, "{}={bits}", bm.name)?;
        }
    }
    if !first {
        write!(out, ")")?;
    }
    Ok(())
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
                bits_to_text(out, info, f.name, None, buf, offset, &f.kind)?;
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
                        bits_to_text(out, info, f.name, Some(i), buf,
                                     offset + i * stride, elem)?;
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

// Generated from BCH_BKEY_TYPES(): BKEY_TYPE_INFO table mapping each key type
// to its val struct's StructInfo, plus lookup helpers.
include!(concat!(env!("OUT_DIR"), "/typeinfo_gen.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::c;

    fn snapshot_info() -> &'static StructInfo {
        <c::bch_snapshot as TypeInfo>::INFO
    }

    #[test]
    fn snapshot_table() {
        let info = snapshot_info();
        assert_eq!(info.name, "bch_snapshot");
        assert_eq!(info.size, core::mem::size_of::<c::bch_snapshot>());
        // v (zero-size), flags, parent, children, subvol, tree, depth, skip, btime
        assert_eq!(info.fields.len(), 9);
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

    #[test]
    fn scalar_roundtrip() {
        let info = snapshot_info();
        let mut buf = [0u8; core::mem::size_of::<c::bch_snapshot>()];

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
        let mut buf = [0u8; core::mem::size_of::<c::bch_snapshot>()];
        let r = resolve(info, "parent").unwrap();
        write_scalar(&mut buf, &r, 249).unwrap();

        let mut s = String::new();
        struct_to_text(&mut s, info, &buf).unwrap();
        assert!(s.contains("parent: 249"), "{s}");
        assert!(s.contains("children: [0, 0]"), "{s}");
    }

    #[test]
    fn bitmask_resolution() {
        let info = snapshot_info();

        // bare bit name resolves to the containing field + bit range
        let (r, bm) = resolve_with_bits(info, "no_keys").unwrap();
        let bm = bm.expect("no_keys is a declared bit");
        assert_eq!((bm.lo, bm.hi), (3, 4));
        assert_eq!(r.offset, 0); // bch_snapshot.flags

        // a field wins the bare name...
        let (_, bm) = resolve_with_bits(info, "subvol").unwrap();
        assert!(bm.is_none());
        // ...the like-named bit stays reachable qualified
        let (_, bm) = resolve_with_bits(info, "flags.subvol").unwrap();
        assert_eq!(bm.expect("flags.subvol is a bit").lo, 1);

        assert!(resolve_with_bits(info, "flags.nonexistent").is_err());
    }

    #[test]
    fn bits_roundtrip() {
        let info = snapshot_info();
        let mut buf = [0u8; core::mem::size_of::<c::bch_snapshot>()];

        let (r, bm) = resolve_with_bits(info, "no_keys").unwrap();
        let bm = bm.unwrap();
        write_bits(&mut buf, &r, bm, 1).unwrap();
        assert_eq!(read_bits(&buf, &r, bm).unwrap(), 1);
        assert_eq!(buf[0], 8); // NO_KEYS is bit 3

        // read-modify-write leaves neighbouring bits alone
        let (r2, bm2) = resolve_with_bits(info, "flags.subvol").unwrap();
        assert_eq!(read_bits(&buf, &r2, bm2.unwrap()).unwrap(), 0);

        assert!(matches!(
            write_bits(&mut buf, &r, bm, 2),
            Err(AccessError::BitsOverflow { bits: 1, val: 2 })
        ));
    }

    #[test]
    fn bits_decode_display() {
        let info = snapshot_info();
        let mut buf = [0u8; core::mem::size_of::<c::bch_snapshot>()];
        buf[0] = 2 | 8; // SUBVOL | NO_KEYS

        let mut s = String::new();
        struct_to_text(&mut s, info, &buf).unwrap();
        assert!(s.contains("flags: 10 (subvol|no_keys)"), "{s}");
    }

    #[test]
    fn typed_accessors() {
        let mut s = c::bch_snapshot::default();
        s.set_no_keys(true);
        assert!(s.no_keys());
        assert_eq!(u32::from_le(s.flags), 8);
        s.set_no_keys(false);
        assert!(!s.no_keys());
        assert_eq!(u32::from_le(s.flags), 0);
    }

    #[test]
    fn bkey_type_dispatch() {
        let snapshot_type = c::bch_bkey_type::KEY_TYPE_snapshot.0;
        let info = bkey_val_info(snapshot_type).unwrap();
        assert_eq!(info.name, "bch_snapshot");

        let ti = bkey_type_info_by_name("snapshot").unwrap();
        assert_eq!(ti.type_, snapshot_type);
        assert!(bkey_type_info_by_name("no_such_type").is_none());
    }
}
