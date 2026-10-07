// SPDX-License-Identifier: GPL-2.0

//! rust_types_gen: writes the C of the types defined in Rust that C shares.
//!
//!   rust_types_gen RECORDS SRC OUT_DIR
//!
//! RECORDS is the .discard.bch_cstruct section of the fs crate's target
//! object, as objcopy -O binary extracts it - see lib.rs for what's in it.
//! Each source file's records, in order, are one header in OUT_DIR, named for
//! the file's path under SRC, the crate's source root: foo/types.rs gives
//! foo/types_gen.h, which foo/types.h includes where the types belong. Run
//! from where rustc ran: a record's file is file!(), which is relative to
//! that. A
//! Rust-only field is opaque storage of its size and alignment; after the
//! definitions come _Static_asserts of the Rust layout, so a C declaration
//! that doesn't match its Rust type fails the C build, naming the field -
//! and of the ioctl numbers, which C and Rust each compute.
//!
//! The records are from the target build, with its configuration: the C is
//! that configuration's, #[cfg] already resolved.
//!
//!   rust_types_gen --emit FILE.rs
//!
//! One source file's C, to stdout, from its text: every configuration, #[cfg]
//! as #if, and no layouts - so no Rust-only fields. For checking a conversion
//! round trips.
//!
//! A host program of its own, with no dependencies: built and run before the
//! C, on any build host.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::process::exit;

include!("cstruct.rs");

const HEADER: &str = "/* SPDX-License-Identifier: GPL-2.0 */\n";

const RECORD_MAGIC: &[u8; 4] = b"CSR1";

/// One record: see lib.rs.
struct Record {
    file:   String,
    line:   u32,
    text:   String,
    nums:   Vec<u64>,
}

/// A struct's layout, from its record's numbers.
struct Layout {
    size:   u64,
    align:  u64,
    /// Each field's offset, size and alignment, in order.
    fields: Vec<(u64, u64, u64)>,
}

fn records(mut b: &[u8]) -> Result<Vec<Record>, String> {
    fn u32_at(b: &[u8], at: usize) -> Result<u32, String> {
        b.get(at..at + 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
            .ok_or_else(|| "record truncated".to_string())
    }
    fn str_at(b: &[u8], at: &mut usize) -> Result<String, String> {
        let len = u32_at(b, *at)? as usize;
        let s = b.get(*at + 4..*at + 4 + len).ok_or("record truncated")?;
        *at += 4 + len;
        String::from_utf8(s.to_vec()).map_err(|_| "record has a string that isn't UTF-8".to_string())
    }

    let mut out = Vec::new();
    loop {
        // The compiler may align each record's static: padding is zeroes.
        while b.first() == Some(&0) {
            b = &b[1..];
        }
        if b.is_empty() {
            return Ok(out);
        }
        if !b.starts_with(RECORD_MAGIC) {
            return Err(format!("record {}: bad magic", out.len()));
        }
        let len = u32_at(b, 4)? as usize;
        let r = b.get(..len).ok_or("record truncated")?;
        let line = u32_at(r, 8)?;
        let mut at = 12;
        let file = str_at(r, &mut at)?;
        let text = str_at(r, &mut at)?;
        let n = u32_at(r, at)? as usize;
        at += 4;
        let nums = (0..n)
            .map(|i| r.get(at + 8 * i..at + 8 * i + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap())))
            .collect::<Option<Vec<_>>>()
            .ok_or("record truncated")?;
        if at + 8 * n != len {
            return Err(format!("{file}:{line}: record length doesn't match its contents"));
        }
        out.push(Record { file, line, text, nums });
        b = &b[len..];
    }
}

/// The header for source file @file, under @src: foo/bar_types.rs - foo/bar.h's
/// types and what Rust calls of it - is foo/bar_gen.h; foo/types.rs is
/// foo/types_gen.h.
fn gen_header(src: &std::path::Path, file: &str) -> Result<String, String> {
    let path = std::fs::canonicalize(file)
        .map_err(|e| format!("{file}: {e} - rust_types_gen runs from where rustc did"))?;
    let rel = path.strip_prefix(src)
        .map_err(|_| format!("{file} isn't under {}", src.display()))?;
    let rel = rel.to_str().ok_or_else(|| format!("{file}: not UTF-8"))?;
    let stem = rel.trim_end_matches(".rs");
    Ok(stem.strip_suffix("_types").unwrap_or(stem).to_string() + "_gen.h")
}

/// The typedefs @c's DEFINE_DARRAY*()s make - util/darray.h:
/// DEFINE_DARRAY(foo) and the like are darray_foo, DEFINE_DARRAY_NAMED(name,
/// ...) name.
fn darray_typedefs(c: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in c.match_indices("DEFINE_DARRAY") {
        let rest = &c[i + "DEFINE_DARRAY".len()..];
        let Some(open) = rest.find('(') else { continue };
        let variant = &rest[..open];
        let arg = rest[open + 1..].split(|ch: char| ch == ',' || ch == ')').next().unwrap_or("").trim();
        if arg.is_empty() || !arg.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
            continue;
        }
        match variant {
            "_NAMED" | "_NAMED_FREE_ITEM" => out.push(arg.to_string()),
            "" | "_PREALLOCATED" | "_FREE_ITEM" => out.push(format!("darray_{arg}")),
            _ => {}
        }
    }
    out
}

/// The type a record defines, and the types it holds by value - the ones C
/// must see defined first. A pointer needs only a declaration.
fn defines_and_holds(r: &Record) -> (Option<String>, Vec<String>) {
    // `c::foo`, `[foo; N]` -> foo; a pointer or reference -> nothing
    fn held(ty: &str) -> Option<String> {
        let mut t = ty.trim();
        if t.starts_with('*') || t.starts_with('&') {
            return None;
        }
        while let Some(inner) = t.strip_prefix('[').and_then(|s| s.rsplit_once(';')).map(|(e, _)| e.trim()) {
            t = inner;
        }
        let t = t.rsplit("::").next().unwrap_or(t).trim();
        t.chars().all(|ch| ch.is_alphanumeric() || ch == '_').then(|| t.to_string())
    }
    match parse_items(&r.text).ok().and_then(|mut items| items.pop()) {
        Some(CItem::Struct(def)) =>
            (Some(def.name.clone()), def.fields.iter().filter_map(|f| held(&f.ty)).collect()),
        Some(CItem::Typedef(t)) => (Some(t.name.clone()), held(&t.ty).into_iter().collect()),
        Some(CItem::Bitfield(b)) => (Some(b.name.clone()), Vec::new()),
        Some(CItem::TaggedUnion(tu)) => (Some(tu.name.clone()),
            tu.arms.iter().map(|a| a.ty.as_str()).chain(tu.pad.as_deref()).filter_map(held).collect()),
        _ => (None, Vec::new()),
    }
}

/// @rs, in line order, reordered so a type comes after the types of this file
/// it holds by value: a nested definition (nestify, a tagged_union!'s arms)
/// is on a later line than what holds it, or the same one - a macro's
/// expansion, all at its invocation. Among records of one line, the ones
/// nothing else there holds go first, each laying out what it holds in its
/// own order, depth first: arm types in arm order, nested types in field
/// order - not in whatever order rustc emitted them. Records on lines of
/// their own keep line order. A cycle is left as it is, for C to complain.
fn in_dependency_order(rs: Vec<&Record>) -> Vec<&Record> {
    let info: Vec<_> = rs.iter().map(|r| defines_and_holds(r)).collect();
    let by_name: BTreeMap<&str, usize> = info.iter().enumerate()
        .filter_map(|(i, (d, _))| d.as_deref().map(|d| (d, i)))
        .collect();
    let held_on_its_line = |i: usize| info[i].0.as_deref().is_some_and(|d| {
        info.iter().enumerate().any(|(j, (_, holds))| j != i && rs[j].line == rs[i].line &&
                                                      holds.iter().any(|h| h == d))
    });
    let mut order: Vec<usize> = (0..rs.len()).collect();
    order.sort_by_key(|&i| (rs[i].line, held_on_its_line(i)));

    fn visit(i: usize, info: &[(Option<String>, Vec<String>)], by_name: &BTreeMap<&str, usize>,
             visited: &mut [bool], out: &mut Vec<usize>) {
        if std::mem::replace(&mut visited[i], true) {
            return;
        }
        for h in &info[i].1 {
            if let Some(&j) = by_name.get(h.as_str()) {
                visit(j, info, by_name, visited, out);
            }
        }
        out.push(i);
    }
    let mut visited = vec![false; rs.len()];
    let mut out = Vec::with_capacity(rs.len());
    for i in order {
        visit(i, &info, &by_name, &mut visited, &mut out);
    }
    out.into_iter().map(|i| rs[i]).collect()
}

/// Include guard for @header: foo/types_gen.h -> _BCACHEFS_FOO_TYPES_GEN_H.
fn guard(header: &str) -> String {
    let mut g = String::from("_BCACHEFS_");
    g.extend(header.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_uppercase() } else { '_' }));
    g
}

/// The layout of struct @def from @nums, checked against its fields.
fn layout(def: &CStructDef, nums: &[u64]) -> Result<Layout, String> {
    match nums {
        [size, align, fields @ ..] if fields.len() == 3 * def.fields.len() => Ok(Layout {
            size:   *size,
            align:  *align,
            fields: fields.chunks(3).map(|f| (f[0], f[1], f[2])).collect(),
        }),
        _ => Err(format!("{}: record has {} numbers for {} fields", def.name, nums.len(), def.fields.len())),
    }
}

/// Check that C places the members of bitfield type @bf where Rust does: the
/// offset asserts can't see inside the storage word. Rust has the word at
/// byte @offset, members packed into it one after another, least significant
/// first. C starts from the end of the field before, byte @prev_end, and - in
/// a struct that isn't @packed - aligns a plain member to its type, and starts
/// a new unit of a bitfield member's type rather than let it straddle one.
/// Little-endian bitfields only: big-endian C declares the members reversed,
/// to put each at the same bits of the same word.
fn check_bitfield(bf: &CBitfieldDef, offset: u64, prev_end: u64, packed: bool) -> Result<(), String> {
    let storage = int_bits(&bf.storage)
        .ok_or_else(|| format!("{}: storage {} isn't an integer", bf.name, bf.storage))? as u64;
    let align_up = |pos: u64, unit: u64| pos.div_ceil(unit) * unit;

    let mut rust = offset * 8;
    let mut c = prev_end * 8;
    for (name, ty, width) in &bf.fields {
        let width = *width as u64;
        // padding: Rust's alone - C skips by itself, which the next
        // member's position checks
        if is_bitfield_pad(name) {
            rust += width;
            continue;
        }
        let unit = unit_bits(ty).ok_or_else(|| format!("{}.{name}: {ty} isn't an integer or bool", bf.name))? as u64;
        if !packed && (width == unit || c / unit != (c + width - 1) / unit) {
            c = align_up(c, unit);
        }
        if c != rust {
            return Err(format!("{}.{name}: C puts it at bit {c}, Rust at bit {rust} - the word must be \
                                where C's member types put it", bf.name));
        }
        c += width;
        rust += width;
    }
    if rust != (offset * 8) + storage {
        return Err(format!("{}: members are {} bits, storage {}", bf.name, rust - offset * 8, bf.storage));
    }
    Ok(())
}

/// The asserts of @def's fields, as members of C type @ty at offset @base in
/// it, named @prefix + name: a type written in its field's place has no name
/// of its own, so its fields are asserted through @ty - an anonymous member's
/// as @ty's own, a #[c_inline] or #[c_struct_group] member's as member.field,
/// the first element's of an array of them, member[0].field.
fn field_asserts(out: &mut String, ty: &str, def: &CStructDef, base: u64, prefix: &str,
                 items: &[CItem], layouts: &BTreeMap<String, Layout>) {
    for (f, (offset, size, _)) in def.fields.iter().zip(&layouts[&def.name].fields) {
        let offset = base + offset;
        let name = format!("{prefix}{}", f.name);
        if let Some(inner) = in_place_def(f, items) {
            if f.inline || f.group {
                member_asserts(out, ty, &name, offset, *size);
                let first = "[0]".repeat(array_of(&f.ty).map_or(0, |(_, dims)| dims.matches('[').count()));
                field_asserts(out, ty, inner, offset, &format!("{name}{first}."), items, layouts);
            } else {
                field_asserts(out, ty, inner, offset, prefix, items, layouts);
            }
            continue;
        }
        if f.anon || f.bitfield {
            continue;
        }
        member_asserts(out, ty, &name, offset, *size);
    }
}

/// The asserts of member @name of C type @ty: its offset, and its size.
fn member_asserts(out: &mut String, ty: &str, name: &str, offset: u64, size: u64) {
    let _ = writeln!(out, "_Static_assert(__builtin_offsetof({ty}, {name}) == {offset}, \
                           \"{ty}.{name}: offset differs from Rust's\");");
    // Zero-sized: a flexible array, which C can't take the size of.
    if size != 0 {
        let _ = writeln!(out, "_Static_assert(sizeof((({ty} *) 0)->{name}) == {size}, \
                               \"{ty}.{name}: size differs from Rust's\");");
    }
}

/// The header for one source file's records, in order. @structs: every
/// struct, union and typedef recorded, any file's - Rust name to C type - for
/// the fields whose type is one.
fn file_h(header: &str, records: &[&Record], structs: &BTreeMap<String, String>)
    -> Result<String, String>
{
    let mut items = Vec::new();
    let mut layouts = BTreeMap::new();
    let mut enum_sizes = Vec::new();
    let mut sames = Vec::new();
    let mut opcodes = Vec::new();
    for r in records {
        let at = |e: String| format!("{}:{}: {e}", r.file, r.line);
        let mut parsed = parse_items(&r.text).map_err(at)?;
        let mut item = match (parsed.pop(), parsed.is_empty()) {
            (Some(item), true) => item,
            _ => return Err(at("a record is one item".into())),
        };
        match &mut item {
            CItem::Struct(def) => {
                layouts.insert(def.name.clone(), layout(def, &r.nums).map_err(at)?);
            }
            // The values, as rustc evaluated them, for the expressions.
            CItem::Enum(e) => {
                let [size, align, values @ ..] = r.nums.as_slice() else {
                    return Err(at("an enum's record has its size, alignment and values".into()));
                };
                if values.len() != e.variants.len() {
                    return Err(at(format!("{} values for {} variants", values.len(), e.variants.len())));
                }
                let signed = e.repr.starts_with('i');
                for ((_, value), &v) in e.variants.iter_mut().zip(values) {
                    *value = Some(match (e.kind, signed) {
                        (CEnumKind::Flags, _) => format!("{v:#x}"),
                        (_, true) => format!("{}", v as i64),
                        (_, false) => format!("{v}"),
                    });
                }
                if let Some(name) = &e.name {
                    enum_sizes.push((name.clone(), *size, *align));
                }
            }
            CItem::Const(c) => {
                let [v] = r.nums.as_slice() else {
                    return Err(at("a constant's record has its value".into()));
                };
                c.value = if rust_int_signed(&c.ty) { format!("{}", *v as i64) } else { format!("{v}") };
            }
            // Each opcode as Rust made it - none in a build that has no
            // Rust ones (see fs/util/ioctl.rs).
            CItem::Ioctl(io) => match r.nums.len() {
                0 => {}
                n if n == io.entries.len() =>
                    opcodes.extend(io.entries.iter().map(|e| e.name.clone()).zip(r.nums.iter().copied())),
                n => return Err(at(format!("{n} opcodes for {} ioctls", io.entries.len()))),
            },
            CItem::Same(s) => {
                let [size, align, offsets @ ..] = r.nums.as_slice() else {
                    return Err(at("a c_same!'s record has the type's size and alignment".into()));
                };
                if offsets.len() != s.fields.len() {
                    return Err(at(format!("{} offsets for {} fields", offsets.len(), s.fields.len())));
                }
                sames.push((s.c_type.clone(), *size, *align,
                            s.fields.iter().cloned().zip(offsets.iter().copied()).collect::<Vec<_>>()));
            }
            // The Rust layout C's must have: a struct's tag at 0, every arm at
            // the payload offset (a union's at 0) - as for a c_same!.
            CItem::TaggedUnion(tu) => {
                let [size, align, payload] = r.nums.as_slice() else {
                    return Err(at("a tagged_union!'s record has its size, alignment and payload offset".into()));
                };
                let mut offsets = Vec::new();
                if !tu.union {
                    offsets.push((c_ident(&tu.tag_field), 0));
                }
                offsets.extend(tu.arms.iter().map(|a| (c_ident(&a.name), *payload)));
                sames.push((tu.c_type(), *size, *align, offsets));
            }
            _ => {}
        }
        items.push(item);
    }

    // Every misplaced bitfield, not just the first.
    let mut misplaced = Vec::new();
    for item in &items {
        let CItem::Struct(def) = item else { continue };
        let fields = &layouts[&def.name].fields;
        for (i, (f, (offset, _, _))) in def.fields.iter().zip(fields).enumerate() {
            if !f.bitfield {
                continue;
            }
            let prev_end = i.checked_sub(1).map_or(0, |p| fields[p].0 + fields[p].1);
            let bf = items.iter().find_map(|i| match i {
                CItem::Bitfield(b) if b.name == f.ty.trim() => Some(b),
                _ => None,
            }).ok_or_else(|| format!("{}.{}: #[c_bitfield], but {} isn't a #[bitfield] type in {header}",
                                     def.name, f.name, f.ty))?;
            if let Err(e) = check_bitfield(bf, *offset, prev_end, def.repr.packed.is_some()) {
                misplaced.push(format!("{}.{}: {e}", def.name, f.name));
            }
        }
    }
    if !misplaced.is_empty() {
        return Err(misplaced.join("\n"));
    }

    // A field whose type is a recorded struct is that struct; any other
    // without a C rendering is Rust's alone: opaque storage, aligned as Rust
    // aligns it - up to the struct's packing.
    let opaque = |def: &CStructDef, f: &CFieldDef| -> Result<String, String> {
        if let Some(c_type) = structs.get(f.ty.trim()) {
            return Ok(format!("{c_type}\t{}", f.name));
        }
        let i = def.fields.iter().position(|x| x.name == f.name).unwrap();
        let (_, size, align) = layouts[&def.name].fields[i];
        let align = match def.repr.packed.as_deref() {
            Some(p) => align.min(p.parse().map_err(|_| format!("packed({p}) isn't a number"))?),
            None => align,
        };
        Ok(if align > 1 {
            format!("unsigned char\t{}[{size}] __aligned({align})", f.name)
        } else {
            format!("unsigned char\t{}[{size}]", f.name)
        })
    };
    // util/darray.h's own darray_foos are typedefs too (types/lib.rs's
    // darrays has them for Rust)
    let kind = |name: &str| structs.get(name).cloned()
        .or_else(|| name.starts_with("darray_").then(|| name.to_string()));
    let body = emit_items(&items, &opaque, &kind)?;

    // The Rust layout, which C must have: every field C names, and each
    // enum's size - C picks an enum's type from its values.
    let mut asserts = String::new();
    for (name, size, align) in &enum_sizes {
        let _ = write!(asserts, "\n_Static_assert(sizeof(enum {name}) == {size}, \"enum {name}: size differs from Rust's\");\n\
                                 _Static_assert(_Alignof(enum {name}) == {align}, \"enum {name}: alignment differs from Rust's\");\n");
    }
    // C types defined by C that are Rust types: C's must be the Rust type's.
    for (ty, size, align, fields) in &sames {
        let _ = write!(asserts, "\n_Static_assert(sizeof({ty}) == {size}, \"{ty}: size differs from Rust's\");\n\
                                 _Static_assert(_Alignof({ty}) == {align}, \"{ty}: alignment differs from Rust's\");\n");
        for (f, offset) in fields {
            let _ = writeln!(asserts, "_Static_assert(__builtin_offsetof({ty}, {f}) == {offset}, \
                                       \"{ty}.{f}: offset differs from Rust's\");");
        }
    }
    // A type written in its field's place has no name in C: its fields are
    // asserted through the struct it's in.
    let in_place = in_place_types(&items);
    for item in &items {
        let CItem::Struct(def) = item else { continue };
        if in_place.contains(&def.name.as_str()) {
            continue;
        }
        let l = &layouts[&def.name];
        let ty = def.c_type();
        let _ = write!(asserts, "\n_Static_assert(sizeof({ty}) == {}, \"{ty}: size differs from Rust's\");\n\
                                 _Static_assert(_Alignof({ty}) == {}, \"{ty}: alignment differs from Rust's\");\n",
                       l.size, l.align);
        field_asserts(&mut asserts, &ty, def, 0, "", &items, &layouts);
    }
    // The ioctl numbers: C's _IO*() and Rust's, each from the target's own
    // constants, must make the same one.
    if !opcodes.is_empty() {
        asserts.push('\n');
    }
    for (name, op) in &opcodes {
        let _ = writeln!(asserts, "_Static_assert({name} == {op:#x}U, \"{name}: differs from Rust's opcode\");");
    }

    let g = guard(header);
    Ok(format!("{HEADER}/* Generated by rust_types_gen from the fs crate's records: do not edit. */\n\
                #ifndef {g}\n#define {g}\n{body}{asserts}\n#endif /* {g} */\n"))
}

/// Write @contents to @path unless it's already there: a rerun doesn't
/// rebuild every C object.
fn write_if_changed(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == contents) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, contents)
}

fn die(msg: String) -> ! {
    eprintln!("rust_types_gen: {msg}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    match args.as_slice() {
        [_, flag, src] if flag == "--emit" => {
            let text = std::fs::read_to_string(src).unwrap_or_else(|e| die(format!("reading {src}: {e}")));
            let mut items = parse_items(&text).unwrap_or_else(|e| die(format!("{src}: {e}")));
            // A tagged_union!'s arms from a list this file defines: as the
            // macro expands them. (One from another file's list has none here.)
            let lists: BTreeMap<String, Vec<String>> = items.iter()
                .filter_map(|i| match i {
                    CItem::XMacro(x) => Some((x.name.clone(), x.entries.iter()
                        .filter_map(|e| match e { CXEntry::Args(a) => Some(a.clone()), _ => None })
                        .collect())),
                    _ => None,
                })
                .collect();
            for item in &mut items {
                if let CItem::TaggedUnion(tu) = item {
                    if let Some(entries) = tu.from.as_ref().and_then(|(l, _, _)| lists.get(l)) {
                        tu.arms = tagged_union_arms(tu, entries).unwrap_or_else(|e| die(format!("{src}: {e}")));
                    }
                }
            }
            // The arm types a tagged_union! defines in its block: structs of
            // their own, before it - with its `#[...]*`, as nestify gives them.
            // One that nests a type of its own, nestify's syntax, isn't
            // something this parser reads: an error naming it.
            let items: Vec<CItem> = items.into_iter()
                .flat_map(|item| {
                    let mut out = Vec::new();
                    if let CItem::TaggedUnion(tu) = &item {
                        let attrs: Vec<&str> = tu.arm_attrs.iter()
                            .map(|a| a.trim().trim_end_matches('*'))
                            .collect();
                        for d in &tu.defs {
                            let text = format!("{}\n{}", attrs.join("\n"), d.text);
                            match parse_items(&text).map(|mut v| (v.pop(), v.is_empty())) {
                                Ok((Some(def @ CItem::Struct(_)), true)) => out.push(def),
                                _ => die(format!("{src}: tagged_union! {}: can't read {}'s definition - \
                                                  --emit takes a plain struct", tu.name, d.name)),
                            }
                        }
                    }
                    out.push(item);
                    out
                })
                .collect();
            // How C names the file's own types: a typedef by its name.
            let mut kinds = BTreeMap::new();
            for item in &items {
                match item {
                    CItem::Struct(def) => { kinds.insert(def.name.clone(), def.c_type()); }
                    CItem::Typedef(t) => { kinds.insert(t.name.clone(), t.name.clone()); }
                    CItem::Enum(CEnumDef { name: Some(n), .. }) => { kinds.insert(n.clone(), format!("enum {n}")); }
                    CItem::TaggedUnion(tu) => { kinds.insert(tu.name.clone(), tu.c_type()); }
                    _ => {}
                }
            }
            let kind = |name: &str| kinds.get(name).cloned();
            let no_opaque = |def: &CStructDef, f: &CFieldDef| -> Result<String, String> {
                Err(format!("{}.{}: {} has no C rendering - from source alone, a field needs one",
                            def.name, f.name, f.ty))
            };
            let body = emit_items(&items, &no_opaque, &kind).unwrap_or_else(|e| die(format!("{src}: {e}")));
            print!("{HEADER}/* Generated by rust_types_gen from {src}: do not edit. */\n{body}");
        }
        [_, records_file, src, out_dir] => {
            let b = std::fs::read(records_file).unwrap_or_else(|e| die(format!("reading {records_file}: {e}")));
            let recs = records(&b).unwrap_or_else(|e| die(format!("{records_file}: {e}")));
            let src = std::fs::canonicalize(src).unwrap_or_else(|e| die(format!("{src}: {e}")));

            let mut files: BTreeMap<&str, Vec<&Record>> = BTreeMap::new();
            let mut structs = BTreeMap::new();
            for r in &recs {
                files.entry(&r.file).or_default().push(r);
                match parse_items(&r.text).ok().and_then(|mut items| items.pop()) {
                    Some(CItem::Struct(def)) => { structs.insert(def.name.clone(), def.c_type()); }
                    Some(CItem::TaggedUnion(tu)) => {
                        structs.insert(tu.name.clone(), tu.c_type());
                    }
                    // a C typedef: by its own name - u##_bits in an x-macro
                    // struct body is u96 as well as u64
                    Some(CItem::Typedef(t)) => { structs.insert(t.name.clone(), t.name.clone()); }
                    // how a prototype names it
                    Some(CItem::Enum(CEnumDef { name: Some(n), .. })) => {
                        structs.insert(n.clone(), format!("enum {n}"));
                    }
                    // a darray type is C's typedef, which its DEFINE_DARRAY*()
                    // makes: Rust's is a DArray alias, with no record
                    Some(CItem::Verbatim(v)) => {
                        for n in darray_typedefs(&v.text) {
                            structs.insert(n.clone(), n);
                        }
                    }
                    _ => {}
                }
            }
            // Every file's errors, not just the first's.
            let mut errors = Vec::new();
            let mut written: BTreeMap<String, &str> = BTreeMap::new();
            for (file, mut rs) in files {
                rs.sort_by_key(|r| r.line);
                let rs = in_dependency_order(rs);
                let header = gen_header(&src, file).unwrap_or_else(|e| die(e));
                if let Some(other) = written.insert(header.clone(), file) {
                    errors.push(format!("{file} and {other} would both be {header}"));
                    continue;
                }
                let h = match file_h(&header, &rs, &structs) {
                    Ok(h) => h,
                    Err(e) => { errors.push(e); continue; }
                };
                let path = std::path::Path::new(out_dir).join(&header);
                write_if_changed(&path, &h).unwrap_or_else(|e| die(format!("writing {}: {e}", path.display())));
            }
            if !errors.is_empty() {
                die(errors.join("\n"));
            }
        }
        _ => die("usage: rust_types_gen RECORDS SRC OUT_DIR | --emit FILE.rs".into()),
    }
}
