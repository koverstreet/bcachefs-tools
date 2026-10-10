// SPDX-License-Identifier: GPL-2.0
//
// Parsing the C-shared items of Rust source, and writing their C - shared by
// the two halves of CStruct (see lib.rs), which must agree on every one:
//   - the macros (fs/cstruct-macros), which parse their input when the fs
//     crate compiles, and record it;
//   - rust_types_gen (main.rs), which parses the recorded text again and
//     writes the C.
//
// Both include!() this file, so it's plain `//` comments, and it depends on
// nothing: the kernel build compiles the macros with a bare rustc.
//
// The macros get their input as a TokenStream and record its to_string(), so
// an item is parsed by the same code from the same text both times. --emit
// parses a whole source file, whose tokens differ from a TokenStream's text
// only in spacing and comments.
//
// The items, each with an optional #[cfg(...)] - which C gets as #if:
//
//   #[derive(CStruct)] struct / union, named fields - #[repr(C)], optionally
//   packed and align(N). A field's #[c("...")] is its C declaration, name
//   included, as C's declarator syntax puts the name inside the type (arrays,
//   function pointers); #[c_anon("...")] a C declaration that doesn't name the
//   field (an anonymous union, a run of bitfields), whose layout only the
//   fields around it check; a field with neither is Rust's alone, and C gets
//   opaque storage for it. Fields can have #[cfg] too.
//
//   A bare #[c_anon] is C's anonymous struct or union: the field's type, a
//   CStruct of the same file - nest! defines it in place - is written where
//   the field is, without a name, and has no definition of its own. Its
//   fields are asserted through the struct it's in. #[c_inline] is the same
//   for a member C names, of a type it doesn't - struct { ... } name - whose
//   fields are asserted as name.field; #[c_struct_group] for the kernel's
//   struct_group(name, ...), its members both name's and the struct's own.
//
//   c_verbatim!(r#"..."#): C, carried as is - what hasn't been converted.
//
//   c_xmacro! { NAME(x) { (args), (args), ... } }: an x-macro list. C gets
//   #define NAME() x(args) x(args)... with each entry's text as written;
//   Rust a macro, NAME!(cb), that hands the whole list to cb!.
//
//   c_bitmask! { LE64_BITMASK(struct foo, field), strip PREFIX_ {
//       NAME(start, end), ... } }: bitfields in a flags word. C gets
//   LE64_BITMASK(NAME, struct foo, field, start, end) for each; Rust,
//   accessors on foo named for NAME without PREFIX_, lowercased.
//
//   c_ioctl! { NAME = _IOW(type, nr, ARG), ... }: ioctl numbers. C gets
//   #define NAME _IOW(type, nr, struct arg) for each; Rust, a marker type
//   binding the opcode to ARG, the opcode computed from the target's own
//   _IOC constants (fs/util/ioctl.rs) - and the record carries it, for the
//   generated header to assert C's _IOW() makes the same number.

/// A cfg() predicate - for C, the #if condition it is.
#[derive(Debug, Clone)]
pub enum Cfg {
    Name(String),
    KeyValue(String, String),
    Not(Box<Cfg>),
    All(Vec<Cfg>),
    Any(Vec<Cfg>),
}

impl Cfg {
    /// The C preprocessor condition: a name is defined(NAME); target_endian
    /// is __BYTE_ORDER__, target_pointer_width __BITS_PER_LONG.
    #[allow(dead_code)]
    pub fn to_c(&self) -> Result<String, String> {
        Ok(match self {
            Cfg::Name(n) => format!("defined({n})"),
            Cfg::KeyValue(k, v) if k == "target_endian" && (v == "little" || v == "big") =>
                format!("__BYTE_ORDER__ == __ORDER_{}_ENDIAN__", v.to_uppercase()),
            Cfg::KeyValue(k, v) if k == "target_pointer_width" && (v == "32" || v == "64") =>
                format!("__BITS_PER_LONG == {v}"),
            Cfg::KeyValue(k, v) => return Err(format!("cfg({k} = \"{v}\") has no C equivalent")),
            Cfg::Not(c) => format!("!({})", c.to_c()?),
            Cfg::All(cs) => cs.iter().map(|c| c.to_c().map(|s| format!("({s})")))
                .collect::<Result<Vec<_>, _>>()?.join(" && "),
            Cfg::Any(cs) => cs.iter().map(|c| c.to_c().map(|s| format!("({s})")))
                .collect::<Result<Vec<_>, _>>()?.join(" || "),
        })
    }

    /// As Rust source, for a #[cfg(...)].
    #[allow(dead_code)]
    pub fn to_rust(&self) -> String {
        match self {
            Cfg::Name(n) => n.clone(),
            Cfg::KeyValue(k, v) => format!("{k} = \"{v}\""),
            Cfg::Not(c) => format!("not({})", c.to_rust()),
            Cfg::All(cs) => format!("all({})", cs.iter().map(Cfg::to_rust).collect::<Vec<_>>().join(", ")),
            Cfg::Any(cs) => format!("any({})", cs.iter().map(Cfg::to_rust).collect::<Vec<_>>().join(", ")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CKind {
    Struct,
    Union,
}

impl CKind {
    #[allow(dead_code)]
    pub fn keyword(self) -> &'static str {
        match self {
            CKind::Struct => "struct",
            CKind::Union => "union",
        }
    }
}

/// A struct's repr: C's packing and alignment attributes. packed is "1" for
/// plain packed.
///
/// c_packed, #[c_packed]: C's is __packed though Rust's isn't. Every field is
/// byte aligned in Rust - zerocopy's endian integers are, C's __le64 isn't -
/// so packing is no-op there, but not in C. For __packed __aligned(N), which
/// Rust's repr can't say: align(N) and #[c_packed].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CRepr {
    pub repr_c:   bool,
    pub packed:   Option<String>,
    pub align:    Option<String>,
    pub c_packed: bool,
}

impl CRepr {
    /// As C attributes, after a struct's closing brace: repr(packed(N)) is
    /// __packed __aligned(N) - Rust can't say packed and align at once.
    #[allow(dead_code)]
    pub fn to_c(&self) -> String {
        let mut out = String::new();
        match self.packed.as_deref() {
            Some("1") => out.push_str(" __packed"),
            Some(n) => out.push_str(&format!(" __packed __aligned({n})")),
            None if self.c_packed => out.push_str(" __packed"),
            None => {}
        }
        if let Some(n) = &self.align {
            out.push_str(&format!(" __aligned({n})"));
        }
        out
    }
}

/// A #[derive(CStruct)] struct or union: its name and fields, in order. Its
/// repr may be per configuration: cfg_attr(PRED, repr(...)).
#[derive(Debug)]
pub struct CStructDef {
    pub name:      String,
    pub kind:      CKind,
    pub cfg:       Option<Cfg>,
    pub repr:      CRepr,
    pub cfg_reprs: Vec<(Cfg, CRepr)>,
    pub fields:    Vec<CFieldDef>,
    /// C has it as a typedef of this name: #[c_typedef], the struct's own name,
    /// for typedef struct { ... } name; #[c_typedef(NAME)] for typedef struct
    /// tag { ... } NAME - the tag the struct's name, NAME the same or not.
    pub typedef:   Option<String>,
    /// The typedef's struct has no tag: #[c_typedef], not #[c_typedef(NAME)].
    pub typedef_anon: bool,
    /// #[c_align("EXPR")]: C's alignment as C spells it - SMP_CACHE_BYTES,
    /// sizeof(long) - which Rust's repr(align) can't: Rust's is per target,
    /// by cfg_attr, and the generated header asserts the two agree.
    pub c_align:   Option<String>,
}

impl CStructDef {
    /// #[repr(C)], in every configuration.
    pub fn repr_c(&self) -> bool {
        self.repr.repr_c || (!self.cfg_reprs.is_empty() && self.cfg_reprs.iter().all(|(_, r)| r.repr_c))
    }

    /// The type as C names it: struct foo, or a typedef's name.
    #[allow(dead_code)]
    pub fn c_type(&self) -> String {
        match &self.typedef {
            Some(t) => t.clone(),
            None => format!("{} {}", self.kind.keyword(), self.name),
        }
    }
}

/// A bitfield-struct type (#[bitfield(uN)]): its storage, and its members
/// least significant first - (name, Rust type, width). A member as wide as its
/// type is a plain member in C; narrower, a bitfield of that unit.
#[derive(Debug)]
#[allow(dead_code)] // cfg and storage: read by rust_types_gen, not the macros
pub struct CBitfieldDef {
    pub name:    String,
    pub cfg:     Option<Cfg>,
    pub storage: String,
    pub fields:  Vec<(String, String, u32)>,
}

/// One field: its name (as C has it - type, not type_), its Rust type as written, its C
/// declaration if C sees it, and whether that declaration names it.
#[derive(Debug)]
pub struct CFieldDef {
    pub name:     String,
    pub ty:       String,
    pub c_decl:   Option<String>,
    /// C doesn't name it: #[c_anon("...")], C's declaration in c_decl, or a
    /// bare #[c_anon], its type written in its place - see in_place_def().
    // Read by rust_types_gen, not the macros.
    #[allow(dead_code)]
    pub anon:     bool,
    /// #[c_inline]: C names it, but not its type, which is written in its
    /// place.
    #[allow(dead_code)]
    pub inline:   bool,
    /// #[c_struct_group]: likewise, as the kernel's struct_group(name, ...) -
    /// its members C's directly too.
    #[allow(dead_code)]
    pub group:    bool,
    /// #[c_bitfield]: its type is a bitfield-struct type, whose members C
    /// declares in its place.
    pub bitfield: bool,
    /// #[bits(N)], in a bitfield-struct type: its width.
    pub bits:     Option<u32>,
    pub cfg:      Option<Cfg>,
}

impl CFieldDef {
    /// The last segment of the field's type path, generics dropped:
    /// `bcachefs_types::Foo<T>` -> `Foo`.
    #[allow(dead_code)]
    pub fn ty_name(&self) -> &str {
        let base = self.ty.split('<').next().unwrap_or(&self.ty);
        base.rsplit("::").next().unwrap_or(base).trim()
    }

    /// The field's name as Rust spells it: a keyword gets a _.
    #[allow(dead_code)]
    pub fn rust_name(&self) -> String {
        rust_ident(&self.name)
    }
}

/// C carried as is.
#[derive(Debug)]
pub struct CVerbatim {
    pub cfg:  Option<Cfg>,
    pub text: String,
}

/// An x-macro list.
#[derive(Debug)]
pub struct CXMacro {
    pub cfg:      Option<Cfg>,
    pub name:     String,
    pub callback: String,
    pub entries:  Vec<CXEntry>,
}

/// An entry of an x-macro list.
#[derive(Debug)]
pub enum CXEntry {
    /// (args): its arguments' C text.
    Args(String),
    /// SUB() - another list's entries, here - or SUB(MAP(params) =>
    /// (template)): SUB's entries, each passed through MAP, which C defines
    /// as #define MAP(params) x(template).
    Sub { list: String, map: Option<CXMap> },
}

#[derive(Debug)]
pub struct CXMap {
    pub name:     String,
    pub params:   Vec<String>,
    /// The arguments MAP gives x, as C text: params, and ## pastes.
    pub template: String,
}

/// Bitfields in one flags word: @macro_name is LE64_BITMASK, LE32_BITMASK,
/// LE16_BITMASK or BITMASK; @c_type the C struct type, "struct foo"; @field
/// the flags word, as C and Rust both write it: "flags[0]".
#[derive(Debug)]
pub struct CBitmask {
    pub cfg:        Option<Cfg>,
    pub macro_name: String,
    pub c_type:     String,
    pub rust_type:  String,
    pub field:      String,
    pub strip:      Option<String>,
    pub entries:    Vec<CBitmaskEntry>,
}

#[derive(Debug)]
pub struct CBitmaskEntry {
    pub name:  String,
    pub start: String,
    pub end:   String,
}

impl CBitmask {
    /// The Rust accessor for @name: PREFIX_ stripped, lowercased - and with a
    /// leading _ if that starts with a digit: BCH_SB_128_BIT_MACS.
    #[allow(dead_code)]
    pub fn method(&self, name: &str) -> String {
        let n = self.strip.as_deref().and_then(|p| name.strip_prefix(p)).unwrap_or(name);
        let n = n.to_lowercase();
        if n.starts_with(|c: char| c.is_ascii_digit()) {
            format!("_{n}")
        } else {
            rust_ident(&n)
        }
    }
}

/// c_ioctl!: ioctl numbers, C's #define NAME _IOW(type, nr, arg).
#[derive(Debug)]
pub struct CIoctl {
    pub cfg:     Option<Cfg>,
    pub entries: Vec<CIoctlEntry>,
}

/// One ioctl: @dir is its _IO*() macro - _IO, _IOR, _IOW or _IOWR - and
/// @ty and @nr its type and number, as written. @arg is the argument's Rust
/// type, None for _IO; @c_arg its C type, from #[c("...")], when the Rust
/// type doesn't say it.
#[derive(Debug)]
pub struct CIoctlEntry {
    pub name:  String,
    pub dir:   String,
    pub ty:    String,
    pub nr:    String,
    pub arg:   Option<String>,
    pub c_arg: Option<String>,
}

/// What an enum's values can be - fs/enum_kind.h's markers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CEnumKind {
    /// Only ever those listed: a Rust enum.
    Closed,
    /// Anything, from outside: a newtype with the values as constants.
    Open,
    /// Masks, or'd: a bitflags type.
    Flags,
}

impl CEnumKind {
    #[allow(dead_code)]
    pub fn marker(self) -> &'static str {
        match self {
            CEnumKind::Closed => "__enum_closed",
            CEnumKind::Open   => "__enum_open",
            CEnumKind::Flags  => "__enum_flags",
        }
    }
}

/// c_enum!: an enum, and what its values can be. @name is None for an
/// anonymous one: constants. A variant's value is Rust source, or none for
/// the one after the last; the generator replaces them with what they
/// evaluate to.
#[derive(Debug)]
pub struct CEnumDef {
    pub cfg:      Option<Cfg>,
    pub kind:     CEnumKind,
    pub name:     Option<String>,
    /// The integer type: u32, i32, u64.
    pub repr:     String,
    pub variants: Vec<(String, Option<String>)>,
}

/// c_typedef!: a type alias, C's typedef. @c_decl is its C declarator - the
/// declaration less `typedef` - where the canonical rendering of @ty isn't.
#[derive(Debug)]
pub struct CTypedef {
    pub cfg:    Option<Cfg>,
    pub name:   String,
    pub ty:     String,
    pub c_decl: Option<String>,
}

/// c_same!: a Rust type with the layout of a C type. The generator checks it:
/// size, alignment, and the offsets of @fields, which both name. (No cfg: a
/// c_same! a build leaves out has no record, so nothing to check.)
#[derive(Debug)]
pub struct CSame {
    #[allow(dead_code)]
    pub c_type:  String,
    pub rust_ty: String,
    pub fields:  Vec<String>,
}

/// c_const!: an integer constant, C's #define.
#[derive(Debug)]
pub struct CConst {
    pub cfg:    Option<Cfg>,
    pub name:   String,
    pub ty:     String,
    /// The #define's type: @ty's, or int under #[c_int].
    pub c_type: String,
    pub value:  String,
}

impl CConst {
    /// The #define's value. An evaluated one, the records path's, is
    /// written so C gives it c_type: `5U`, `5ULL`, `((size_t) 5)`. A
    /// constant's type is what C does with it - %zu, integer promotion,
    /// signed against unsigned - and a bare number would be int, or
    /// whatever's wide enough. --emit's value is the Rust expression,
    /// as C.
    fn c_value(&self) -> Result<String, String> {
        let Ok(v) = self.value.parse::<i128>() else {
            return Ok(rust_expr_to_c(&self.value));
        };
        let suffix = match self.c_type.as_str() {
            "int" | "s32"                   => Some(("", i32::MIN as i128, i32::MAX as i128)),
            "unsigned" | "u32"              => Some(("U", 0, u32::MAX as i128)),
            "long"                          => Some(("L", i64::MIN as i128, i64::MAX as i128)),
            "unsigned long"                 => Some(("UL", 0, u64::MAX as i128)),
            "long long" | "s64"             => Some(("LL", i64::MIN as i128, i64::MAX as i128)),
            "unsigned long long" | "u64"    => Some(("ULL", 0, u64::MAX as i128)),
            _                               => None,
        };
        Ok(match suffix {
            Some((_, min, max)) if v < min || v > max =>
                return Err(format!("c_const! {}: {v} isn't a {}, its C type", self.name, self.c_type)),
            // C's negative numbers are a positive literal negated: the
            // type's minimum has no literal of the type
            Some((_, min, _)) if v == min && v < 0 =>
                return Err(format!("c_const! {}: {v}, the minimum of {}, has no C literal", self.name, self.c_type)),
            Some((s, _, _)) => format!("{v}{s}"),
            None => {
                let wide = if i32::try_from(v).is_ok() { "" } else if v < 0 { "LL" } else { "ULL" };
                format!("(({}) {v}{wide})", self.c_type)
            }
        })
    }
}

/// Whether integer type @ty, a Rust one, is signed: how a constant's
/// record, `as u64`, reads back.
fn rust_int_signed(ty: &str) -> bool {
    let last = ty.rsplit("::").next().unwrap_or(ty).trim();
    last.starts_with('i') || matches!(last, "c_schar" | "c_short" | "c_int" | "c_long" | "c_longlong")
}

/// A C function Rust calls, from c_extern!.
#[derive(Debug)]
pub struct CFnDecl {
    pub cfg:      Option<Cfg>,
    pub name:     String,
    /// Each parameter's name and Rust type.
    pub params:   Vec<(String, String)>,
    pub variadic: bool,
    pub ret:      Option<String>,
}

/// A C variable Rust uses, from c_extern!: with #[c("...")], its C
/// declaration where the Rust type doesn't say it - C's const, which a Rust
/// static has no way to.
#[derive(Debug)]
pub struct CStaticDecl {
    pub cfg:     Option<Cfg>,
    pub name:    String,
    pub ty:      String,
    pub mutable: bool,
    pub c_decl:  Option<String>,
}

/// c_extern!: what Rust calls of a C header - fs/foo/bar_c.rs, of
/// fs/foo/bar.h. Rust gets an extern "C" block; C, the prototypes, which
/// bar.h includes, so a declaration that has drifted from the C one is a
/// conflicting-types error in the C build. And rust_c_extern!: what C calls
/// of Rust - the same prototypes, Rust's side the definitions.
#[derive(Debug)]
pub struct CExtern {
    pub cfg:     Option<Cfg>,
    pub fns:     Vec<CFnDecl>,
    pub statics: Vec<CStaticDecl>,
}

/// One arm of a tagged_union!: its name, payload type, and tag value.
#[derive(Debug, Clone)]
pub struct CTaggedArm {
    pub name:  String,
    pub ty:    String,
    pub value: String,
}

/// tagged_union!: a tagged union with a stable representation.
///
///   tagged_union! {
///       pub struct disk_accounting_pos {
///           tag type_: u8 = disk_accounting_type,
///           arms from BCH_DISK_ACCOUNTING_TYPES(f, nr, ..) => f: bch_acct_ ## f = nr,
///           pad: c::bpos,
///           packed,
///       }
///   }
///
///   tagged_union! {
///       pub union bch_extent_entry {
///           tag type_: u32 = bch_extent_entry_type by ExtentEntryType,
///           arms from BCH_EXTENT_ENTRY_TYPES(f, n) => f: bch_extent_ ## f = n,
///       }
///   }
///
/// The storage is what C has: a `struct` is the tag, then a union of the
/// arms; a `union` is the union of the arms alone, the tag being something
/// the arms' own bytes say - computed by the Determinant named with `by`.
/// Either is overlaid with the pad, if any. The arms come from an x-macro
/// list, `arms from LIST(params) => template`, its entries bound to the
/// params and `##` pasting tokens - or are written out, `arm name: Type =
/// value`, which is the form its record has. The arms' types can be defined
/// in the block too, `[#[...]] [pub] struct|union NAME { ... }`: they go to
/// nestify::nest!, with the declaration's `#[...]*`.
#[derive(Debug)]
pub struct CTaggedUnion {
    pub cfg:       Option<Cfg>,
    pub name:      String,
    /// `union NAME`: no stored tag - the arms alone.
    pub union:     bool,
    /// The tag's name: for a `struct`, its field; for either, the accessor.
    pub tag_field: String,
    pub tag_ty:    String,
    pub tag_enum:  String,
    /// `by DETERMINANT`: the tag, computed by a hand-written Determinant.
    pub by:        Option<String>,
    /// `arms from LIST(params) => template`: the list, its parameters, and the
    /// template, as source.
    pub from:      Option<(String, Vec<String>, String)>,
    pub arms:      Vec<CTaggedArm>,
    pub pad:       Option<String>,
    /// `packed`: C's __packed - for a struct, the payload right after the
    /// tag whatever the arms' alignment, which is what the format says.
    pub packed:    bool,
    /// `#[...]*` on the declaration: attributes for the arm types defined in
    /// it, propagated as nestify does - as source, the `*` included.
    // Read by rust_types_gen's --emit, not the macros.
    #[allow(dead_code)]
    pub arm_attrs: Vec<String>,
    /// Arm types defined in the block, for nestify::nest!.
    pub defs:      Vec<CTaggedDef>,
}

/// An arm's type, defined in its tagged_union!'s block: its name, and its
/// definition as source.
#[derive(Debug, Clone)]
pub struct CTaggedDef {
    pub name: String,
    // Read by rust_types_gen's --emit, not the macros.
    #[allow(dead_code)]
    pub text: String,
}

/// Where a definition's `struct|union NAME { ... }` starts in @item, past its
/// attributes and visibility - if it is one.
fn tagged_def_at(item: &[Tok]) -> Option<usize> {
    let j = skip_vis(item, skip_attrs(item, 0).ok()?).ok()?;
    match (item.get(j), item.get(j + 1), item.get(j + 2)) {
        (Some(Tok::Ident(kw)), Some(Tok::Ident(_)), Some(Tok::Punct('{')))
            if kw == "struct" || kw == "union" => Some(j),
        _ => None,
    }
}

#[derive(Debug)]
pub enum CItem {
    Extern(CExtern),
    TaggedUnion(CTaggedUnion),
    Struct(CStructDef),
    Bitfield(CBitfieldDef),
    Verbatim(CVerbatim),
    XMacro(CXMacro),
    Bitmask(CBitmask),
    Ioctl(CIoctl),
    Enum(CEnumDef),
    Const(CConst),
    Typedef(CTypedef),
    Same(CSame),
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final",
    "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "union",
    "gen",
];

/// A C name as Rust spells it: a keyword gets a _ - type is type_, as
/// bindgen has it.
pub fn rust_ident(name: &str) -> String {
    if RUST_KEYWORDS.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

/// The C name of Rust identifier @name: rust_ident() undone.
pub fn c_ident(name: &str) -> String {
    match name.strip_suffix('_') {
        Some(kw) if RUST_KEYWORDS.contains(&kw) => kw.to_string(),
        _ => name.to_string(),
    }
}

// ---- tokens ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Punct(char),
    /// A string literal: its value, and its source text.
    Str(String, String),
    Lit(String),
}

impl Tok {
    fn is_punct(&self, c: char) -> bool {
        *self == Tok::Punct(c)
    }

    fn is_ident(&self, s: &str) -> bool {
        matches!(self, Tok::Ident(i) if i == s)
    }
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let s: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;

    while i < s.len() {
        let c = s[i];

        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && s.get(i + 1) == Some(&'/') {
            while i < s.len() && s[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && s.get(i + 1) == Some(&'*') {
            // Rust block comments nest.
            let mut depth = 0;
            loop {
                if i + 1 >= s.len() {
                    return Err("unterminated block comment".into());
                }
                if s[i] == '/' && s[i + 1] == '*' {
                    depth += 1;
                    i += 2;
                } else if s[i] == '*' && s[i + 1] == '/' {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c == '"' {
            let start = i;
            let mut v = String::new();
            i += 1;
            loop {
                match s.get(i) {
                    None => return Err("unterminated string literal".into()),
                    Some('"') => break,
                    Some('\\') => {
                        match s.get(i + 1) {
                            Some('n') => v.push('\n'),
                            Some('t') => v.push('\t'),
                            Some(&e) => v.push(e),
                            None => return Err("unterminated string literal".into()),
                        }
                        i += 2;
                    }
                    Some(&ch) => {
                        v.push(ch);
                        i += 1;
                    }
                }
            }
            i += 1;
            toks.push(Tok::Str(v, s[start..i].iter().collect()));
        } else if c == 'r' && matches!(s.get(i + 1), Some('"') | Some('#')) {
            // Raw string r"..." / r#"..."#, or raw identifier r#ident.
            let start = i;
            let mut j = i + 1;
            let mut hashes = 0;
            while s.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if s.get(j) != Some(&'"') {
                let id_start = j;
                while j < s.len() && (s[j].is_alphanumeric() || s[j] == '_') {
                    j += 1;
                }
                toks.push(Tok::Ident(s[id_start..j].iter().collect()));
                i = j;
                continue;
            }
            j += 1;
            let v_start = j;
            loop {
                if j >= s.len() {
                    return Err("unterminated raw string literal".into());
                }
                if s[j] == '"' && (0..hashes).all(|h| s.get(j + 1 + h) == Some(&'#')) {
                    break;
                }
                j += 1;
            }
            let v = s[v_start..j].iter().collect();
            i = j + 1 + hashes;
            toks.push(Tok::Str(v, s[start..i].iter().collect()));
        } else if c == '\'' {
            // A char literal, or a lifetime.
            if s.get(i + 1) == Some(&'\\') {
                let mut j = i + 2;
                while j < s.len() && s[j] != '\'' {
                    j += 1;
                }
                toks.push(Tok::Lit(s[i..=j.min(s.len() - 1)].iter().collect()));
                i = j + 1;
            } else if s.get(i + 2) == Some(&'\'') {
                toks.push(Tok::Lit(s[i..i + 3].iter().collect()));
                i += 3;
            } else {
                let mut j = i + 1;
                while j < s.len() && (s[j].is_alphanumeric() || s[j] == '_') {
                    j += 1;
                }
                toks.push(Tok::Lit(s[i..j].iter().collect()));
                i = j;
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < s.len() && (s[i].is_alphanumeric() || s[i] == '_') {
                i += 1;
            }
            toks.push(Tok::Ident(s[start..i].iter().collect()));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < s.len() && (s[i].is_alphanumeric() || s[i] == '_') {
                i += 1;
            }
            toks.push(Tok::Lit(s[start..i].iter().collect()));
        } else {
            toks.push(Tok::Punct(c));
            i += 1;
        }
    }

    Ok(toks)
}

/// The index just past the bracket group opening at @i, with nested groups of
/// every kind skipped.
fn skip_group(toks: &[Tok], i: usize) -> Result<usize, String> {
    let mut depth = 0i32;
    let mut j = i;
    while j < toks.len() {
        match &toks[j] {
            Tok::Punct('(' | '[' | '{') => depth += 1,
            Tok::Punct(')' | ']' | '}') => {
                depth -= 1;
                if depth == 0 {
                    return Ok(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    Err("unbalanced brackets".into())
}

/// Split @toks on ',' at bracket depth 0; an empty last part is dropped.
fn split_commas(toks: &[Tok]) -> Vec<&[Tok]> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        match t {
            Tok::Punct('(' | '[' | '{') => depth += 1,
            Tok::Punct(')' | ']' | '}') => depth -= 1,
            Tok::Punct(',') if depth == 0 => {
                parts.push(&toks[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < toks.len() {
        parts.push(&toks[start..]);
    }
    parts
}

/// Render tokens as Rust source, `::` and generics intact.
fn tokens_to_rust(toks: &[Tok]) -> String {
    // A space only between two words: punctuation stays as written, so `<<`
    // and `->` come back whole - the tokenizer splits them.
    let word = |t: &Tok| !matches!(t, Tok::Punct(_));
    let mut out = String::new();
    for (n, t) in toks.iter().enumerate() {
        if n > 0 && word(&toks[n - 1]) && word(t) {
            out.push(' ');
        }
        match t {
            Tok::Ident(s) | Tok::Lit(s) => out.push_str(s),
            Tok::Punct(c) => out.push(*c),
            Tok::Str(_, raw) => out.push_str(raw),
        }
    }
    out
}

/// Render tokens as C source: a space only between two words, so C's
/// multi-character operators - `->`, `<<` - come out whole.
fn tokens_to_c(toks: &[Tok]) -> String {
    let word = |t: &Tok| !matches!(t, Tok::Punct(_));
    let mut out = String::new();
    for (n, t) in toks.iter().enumerate() {
        if n > 0 && (word(&toks[n - 1]) && word(t) ||
                     toks[n - 1].is_punct(',') || t.is_punct('|') || toks[n - 1].is_punct('|')) {
            out.push(' ');
        }
        match t {
            Tok::Ident(s) | Tok::Lit(s) => out.push_str(s),
            Tok::Punct(c) => out.push(*c),
            Tok::Str(_, raw) => out.push_str(raw),
        }
    }
    out
}

// ---- attributes ------------------------------------------------------------

/// An attribute at @i (`#` `[`...`]`): its tokens between the brackets, and
/// the index after it. None if there isn't one there.
fn attr_at(toks: &[Tok], i: usize) -> Result<Option<(&[Tok], usize)>, String> {
    if !toks.get(i).is_some_and(|t| t.is_punct('#')) ||
       !toks.get(i + 1).is_some_and(|t| t.is_punct('[')) {
        return Ok(None);
    }
    let end = skip_group(toks, i + 1)?;
    Ok(Some((&toks[i + 2..end - 1], end)))
}

/// An attribute's arguments, if its path is @name: `repr`, `derive`, `c`.
fn attr_args<'a>(attr: &'a [Tok], name: &str) -> Option<&'a [Tok]> {
    match attr {
        [Tok::Ident(n), Tok::Punct('('), rest @ ..] if n == name =>
            rest.split_last().map(|(_, args)| args),
        _ => None,
    }
}

/// Parse a cfg predicate.
fn parse_cfg(toks: &[Tok]) -> Result<Cfg, String> {
    match toks {
        [Tok::Ident(n)] => Ok(Cfg::Name(n.clone())),
        [Tok::Ident(k), Tok::Punct('='), Tok::Str(v, _)] => Ok(Cfg::KeyValue(k.clone(), v.clone())),
        [Tok::Ident(op), Tok::Punct('('), rest @ .., Tok::Punct(')')] => {
            let args = split_commas(rest).into_iter().map(parse_cfg).collect::<Result<Vec<_>, _>>()?;
            match op.as_str() {
                "not" if args.len() == 1 => Ok(Cfg::Not(Box::new(args.into_iter().next().unwrap()))),
                "all" => Ok(Cfg::All(args)),
                "any" => Ok(Cfg::Any(args)),
                _ => Err(format!("cfg({}) not understood", tokens_to_rust(toks))),
            }
        }
        _ => Err(format!("cfg({}) not understood", tokens_to_rust(toks))),
    }
}

/// Item attributes we read; others are skipped.
#[derive(Default)]
struct ItemAttrs {
    cfg:       Option<Cfg>,
    derives:   bool,
    repr:      CRepr,
    cfg_reprs: Vec<(Cfg, CRepr)>,
    /// #[bitfield(uN)]: a bitfield-struct type, its storage.
    bitfield:  Option<String>,
    /// #[c_typedef] or #[c_typedef(NAME)].
    typedef:   Option<Option<String>>,
    /// #[c_align("EXPR")].
    c_align:   Option<String>,
    /// #[c("...")] on an item: its C declaration.
    c_decl:    Option<String>,
}

fn parse_repr(args: &[Tok], into: &mut CRepr) -> Result<(), String> {
    for r in split_commas(args) {
        match r {
            [Tok::Ident(c)] if c == "C" => into.repr_c = true,
            [Tok::Ident(p)] if p == "packed" => into.packed = Some("1".into()),
            [Tok::Ident(p), Tok::Punct('('), n @ .., Tok::Punct(')')] if p == "packed" =>
                into.packed = Some(tokens_to_rust(n)),
            [Tok::Ident(al), Tok::Punct('('), n @ .., Tok::Punct(')')] if al == "align" =>
                into.align = Some(tokens_to_rust(n)),
            _ => return Err(format!("repr({}) not supported", tokens_to_rust(r))),
        }
    }
    Ok(())
}

/// Read the attributes at @i, returning them and the index after.
fn item_attrs(toks: &[Tok], mut i: usize) -> Result<(ItemAttrs, usize), String> {
    let mut a = ItemAttrs::default();
    while let Some((attr, next)) = attr_at(toks, i)? {
        if let Some(args) = attr_args(attr, "cfg") {
            a.cfg = Some(parse_cfg(args)?);
        }
        if let Some(args) = attr_args(attr, "derive") {
            a.derives |= args.iter().any(|t| t.is_ident("CStruct"));
        }
        if let Some(args) = attr_args(attr, "repr") {
            parse_repr(args, &mut a.repr)?;
        }
        if let Some(args) = attr_args(attr, "cfg_attr") {
            // cfg_attr(PRED, repr(...)): a repr for one configuration.
            let parts = split_commas(args);
            if let [pred, rest @ ..] = parts.as_slice() {
                if let [r] = rest {
                    if let Some(rargs) = attr_args(r, "repr") {
                        let mut repr = CRepr::default();
                        parse_repr(rargs, &mut repr)?;
                        a.cfg_reprs.push((parse_cfg(pred)?, repr));
                    }
                }
            }
        }
        if let Some(args) = attr_args(attr, "bitfield") {
            a.bitfield = split_commas(args).first().map(|t| tokens_to_rust(t));
        }
        if matches!(attr, [Tok::Ident(n)] if n == "c_typedef") {
            a.typedef = Some(None);
        }
        if matches!(attr, [Tok::Ident(n)] if n == "c_packed") {
            a.repr.c_packed = true;
        }
        if let Some([Tok::Str(s, _)]) = attr_args(attr, "c_align") {
            a.c_align = Some(s.clone());
        }
        if let Some([Tok::Str(s, _)]) = attr_args(attr, "c") {
            a.c_decl = Some(s.clone());
        }
        if let Some([Tok::Ident(t)]) = attr_args(attr, "c_typedef") {
            a.typedef = Some(Some(t.clone()));
        }
        i = next;
    }
    for (_, r) in &mut a.cfg_reprs {
        r.c_packed = a.repr.c_packed;
    }
    Ok((a, i))
}

/// Skip a visibility at @i: `pub`, or `pub(...)`.
fn skip_vis(toks: &[Tok], mut i: usize) -> Result<usize, String> {
    if toks.get(i).is_some_and(|t| t.is_ident("pub")) {
        i += 1;
        if toks.get(i).is_some_and(|t| t.is_punct('(')) {
            i = skip_group(toks, i)?;
        }
    }
    Ok(i)
}

fn ident_at(toks: &[Tok], i: usize) -> Option<&str> {
    match toks.get(i) {
        Some(Tok::Ident(s)) => Some(s),
        _ => None,
    }
}

// ---- structs ---------------------------------------------------------------

/// The struct or union at @i, after its attributes @a, and the index after
/// it; None if there isn't one.
fn parse_struct(toks: &[Tok], i: usize, a: ItemAttrs) -> Result<Option<(CStructDef, usize)>, String> {
    let mut i = skip_vis(toks, i)?;
    let kind = match ident_at(toks, i) {
        Some("struct") => CKind::Struct,
        Some("union") => CKind::Union,
        _ => return Ok(None),
    };
    let Some(name) = ident_at(toks, i + 1) else { return Ok(None) };
    let name = name.to_string();
    i += 2;

    if !toks.get(i).is_some_and(|t| t.is_punct('{')) {
        return Err(format!("{name}: a CStruct has named fields, and no generics"));
    }
    let body_end = skip_group(toks, i)? - 1;
    i += 1;

    let mut fields = Vec::new();
    while i < body_end {
        let mut c_decl = None;
        let mut anon = false;
        let mut inline = false;
        let mut group = false;
        let mut bitfield = false;
        let mut bits = None;
        let mut cfg = None;
        while let Some((attr, next)) = attr_at(toks, i)? {
            if matches!(attr, [Tok::Ident(n)] if n == "c_bitfield") {
                bitfield = true;
            }
            if matches!(attr, [Tok::Ident(n)] if n == "c_anon") {
                anon = true;
            }
            if matches!(attr, [Tok::Ident(n)] if n == "c_inline") {
                inline = true;
            }
            if matches!(attr, [Tok::Ident(n)] if n == "c_struct_group") {
                group = true;
            }
            if let Some(args) = attr_args(attr, "bits") {
                match args {
                    [Tok::Lit(n)] => bits = Some(n.parse::<u32>()
                        .map_err(|_| format!("{name}: #[bits({n})] isn't a width"))?),
                    _ => return Err(format!("{name}: #[bits(N)] takes a width")),
                }
            }
            for (n, is_anon) in [("c", false), ("c_anon", true)] {
                if let Some(args) = attr_args(attr, n) {
                    match args {
                        [Tok::Str(s, _)] => {
                            c_decl = Some(s.clone());
                            anon = is_anon;
                        }
                        _ => return Err(format!("{name}: #[{n}(...)] takes one string, a C declaration")),
                    }
                }
            }
            if let Some(args) = attr_args(attr, "cfg") {
                cfg = Some(parse_cfg(args)?);
            }
            i = next;
        }
        i = skip_vis(toks, i)?;

        let Some(field) = ident_at(toks, i) else {
            return Err(format!("{name}: expected a field name"));
        };
        let field = c_ident(field);
        if !toks.get(i + 1).is_some_and(|t| t.is_punct(':')) {
            return Err(format!("{name}.{field}: expected `:`"));
        }
        i += 2;

        // The type runs to a `,` outside any brackets, or the body's end.
        // `<`/`>` nest too - except the `>` of `->`.
        let ty_start = i;
        let mut depth = 0i32;
        while i < body_end {
            match &toks[i] {
                Tok::Punct('(' | '[' | '{' | '<') => depth += 1,
                Tok::Punct(')' | ']' | '}') => depth -= 1,
                Tok::Punct('>') if !toks[i - 1].is_punct('-') => depth -= 1,
                Tok::Punct(',') if depth == 0 => break,
                _ => {}
            }
            i += 1;
        }
        let ty = tokens_to_rust(&toks[ty_start..i]);
        if ty.is_empty() {
            return Err(format!("{name}.{field}: missing type"));
        }
        i += 1; // the `,`

        if let (Some(decl), false) = (&c_decl, anon) {
            let names_field = decl
                .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .any(|w| w == field);
            if !names_field {
                return Err(format!(
                    "{name}.{field}: #[c(\"{decl}\")] must declare the field by its own name - \
                     #[c_anon] is for a declaration that doesn't"));
            }
        }
        if (inline || group) && (anon || c_decl.is_some() || (inline && group)) {
            return Err(format!("{name}.{field}: #[c_inline] and #[c_struct_group] are the field's type, \
                                written in its place - one of them, and not a #[c] or #[c_anon] too"));
        }
        fields.push(CFieldDef { name: field, ty, c_decl, anon, inline, group, bitfield, bits, cfg });
    }

    let typedef_anon = matches!(a.typedef, Some(None));
    let typedef = a.typedef.map(|t| t.unwrap_or_else(|| name.clone()));
    Ok(Some((CStructDef {
        name, kind, cfg: a.cfg, repr: a.repr, cfg_reprs: a.cfg_reprs, fields, typedef,
        typedef_anon, c_align: a.c_align,
    }, body_end + 1)))
}

/// A bitfield-struct type at @i, after its attributes - the struct with
/// #[bitfield(@storage)] - and the index after it.
fn parse_bitfield(toks: &[Tok], i: usize, cfg: Option<Cfg>, storage: String)
    -> Result<Option<(CBitfieldDef, usize)>, String>
{
    let Some((def, next)) = parse_struct(toks, i, ItemAttrs::default())? else {
        return Ok(None);
    };
    let mut fields = Vec::new();
    for f in def.fields {
        let width = match f.bits {
            Some(w) => w,
            None => int_bits(&f.ty).ok_or_else(|| format!(
                "{}.{}: a member without #[bits(N)] must be an integer", def.name, f.name))?,
        };
        fields.push((f.name, f.ty, width));
    }
    Ok(Some((CBitfieldDef { name: def.name, cfg, storage, fields }, next)))
}

/// The width of the unit C lays out a bitfield of type @ty in: an integer's,
/// and bool's - _Bool's byte.
#[allow(dead_code)]
fn unit_bits(ty: &str) -> Option<u32> {
    if ty.trim() == "bool" { Some(8) } else { int_bits(ty) }
}

/// The width of integer type @ty, in bits.
fn int_bits(ty: &str) -> Option<u32> {
    Some(match ty.trim() {
        "u8" | "i8" => 8,
        "u16" | "i16" => 16,
        "u32" | "i32" => 32,
        "u64" | "i64" => 64,
        _ => return None,
    })
}

// ---- c_xmacro!, c_bitmask! -------------------------------------------------

/// A macro invocation's body at @i - `( ... )` or `{ ... }` - and the index
/// after it, a trailing `;` included.
fn macro_body(toks: &[Tok], i: usize) -> Result<(&[Tok], usize), String> {
    if !toks.get(i).is_some_and(|t| t.is_punct('(') || t.is_punct('{')) {
        return Err("expected a macro body".into());
    }
    let end = skip_group(toks, i)?;
    let mut next = end;
    if toks.get(next).is_some_and(|t| t.is_punct(';')) {
        next += 1;
    }
    Ok((&toks[i + 1..end - 1], next))
}

/// Skip attributes - the doc comments a macro's body can start with.
fn skip_attrs(toks: &[Tok], mut i: usize) -> Result<usize, String> {
    while let Some((_, next)) = attr_at(toks, i)? {
        i = next;
    }
    Ok(i)
}

/// c_xmacro!'s body: NAME(x) { (args), ... }
fn parse_xmacro(body: &[Tok], cfg: Option<Cfg>) -> Result<CXMacro, String> {
    let i = skip_attrs(body, 0)?;
    let (Some(name), Some(callback)) = (ident_at(body, i), ident_at(body, i + 2)) else {
        return Err("c_xmacro!: expected NAME(callback) { (entry), ... }".into());
    };
    if !body.get(i + 1).is_some_and(|t| t.is_punct('(')) || !body.get(i + 3).is_some_and(|t| t.is_punct(')')) {
        return Err(format!("c_xmacro! {name}: expected NAME(callback)"));
    }
    let (list, _) = macro_body(body, i + 4)?;
    let mut entries = Vec::new();
    for e in split_commas(list) {
        match e {
            [Tok::Punct('('), inner @ .., Tok::Punct(')')] =>
                entries.push(CXEntry::Args(tokens_to_c(inner))),
            [Tok::Ident(sub), Tok::Punct('('), Tok::Punct(')')] =>
                entries.push(CXEntry::Sub { list: sub.clone(), map: None }),
            [Tok::Ident(sub), Tok::Punct('('), Tok::Ident(map), Tok::Punct('('), rest @ ..] => {
                // SUB(MAP(params) => (template))
                let close = rest.iter().position(|t| t.is_punct(')'))
                    .ok_or_else(|| format!("c_xmacro! {name}: {sub}({map}(...: unterminated"))?;
                let params = split_commas(&rest[..close]).iter()
                    .map(|p| match p {
                        [Tok::Ident(p)] => Ok(p.clone()),
                        _ => Err(format!("c_xmacro! {name}: {map}'s parameters are names")),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let template = match &rest[close + 1..] {
                    [Tok::Punct('='), Tok::Punct('>'), Tok::Punct('('), t @ .., Tok::Punct(')'), Tok::Punct(')')] =>
                        tokens_to_c(t),
                    _ => return Err(format!("c_xmacro! {name}: expected {sub}({map}(params) => (args))")),
                };
                entries.push(CXEntry::Sub {
                    list: sub.clone(),
                    map: Some(CXMap { name: map.clone(), params, template }),
                });
            }
            _ => return Err(format!("c_xmacro! {name}: an entry is (args), SUB(), or SUB(MAP(params) => (args))")),
        }
    }
    Ok(CXMacro { cfg, name: name.to_string(), callback: callback.to_string(), entries })
}

/// c_bitmask!'s body: LE64_BITMASK(struct foo, field), strip PREFIX_ { NAME(start, end), ... }
fn parse_bitmask(body: &[Tok], cfg: Option<Cfg>) -> Result<CBitmask, String> {
    let i = skip_attrs(body, 0)?;
    let Some(macro_name) = ident_at(body, i) else {
        return Err("c_bitmask!: expected LE64_BITMASK(struct foo, field)".into());
    };
    if !matches!(macro_name, "LE64_BITMASK" | "LE32_BITMASK" | "LE16_BITMASK" | "BITMASK") {
        return Err(format!("c_bitmask!: {macro_name}: not a bitmask macro"));
    }
    let args_end = skip_group(body, i + 1)?;
    let args = split_commas(&body[i + 2..args_end - 1]);
    let [ty, field] = args.as_slice() else {
        return Err(format!("c_bitmask! {macro_name}: expected (struct foo, field)"));
    };
    let rust_type = match ty {
        [Tok::Ident(s), Tok::Ident(n)] if s == "struct" => n.clone(),
        _ => return Err(format!("c_bitmask! {macro_name}: expected a struct type")),
    };

    let mut i = args_end;
    if body.get(i).is_some_and(|t| t.is_punct(',')) {
        i += 1;
    }
    let mut strip = None;
    if body.get(i).is_some_and(|t| t.is_ident("strip")) {
        strip = ident_at(body, i + 1).map(String::from);
        i += 2;
    }

    let (list, _) = macro_body(body, i)?;
    let mut entries = Vec::new();
    for e in split_commas(list) {
        match e {
            [Tok::Ident(n), Tok::Punct('('), range @ .., Tok::Punct(')')] => {
                let parts = split_commas(range);
                let [start, end] = parts.as_slice() else {
                    return Err(format!("c_bitmask! {n}: expected NAME(start, end)"));
                };
                entries.push(CBitmaskEntry {
                    name:  n.clone(),
                    start: tokens_to_c(start),
                    end:   tokens_to_c(end),
                });
            }
            _ => return Err(format!("c_bitmask! {rust_type}: an entry is NAME(start, end)")),
        }
    }

    Ok(CBitmask {
        cfg,
        macro_name: macro_name.to_string(),
        c_type: tokens_to_c(ty),
        rust_type,
        field: tokens_to_c(field),
        strip,
        entries,
    })
}

/// c_ioctl!'s body: NAME = _IOW(type, nr, ARG), ... - _IO without the ARG -
/// with #[c("...")] before an entry for its argument's C type, where the Rust
/// type doesn't say it.
fn parse_ioctl(body: &[Tok], cfg: Option<Cfg>) -> Result<CIoctl, String> {
    let mut entries = Vec::new();
    for e in split_commas(body) {
        let mut c_arg = None;
        let mut i = 0;
        while let Some((attr, next)) = attr_at(e, i)? {
            if let Some([Tok::Str(s, _)]) = attr_args(attr, "c") {
                c_arg = Some(s.clone());
            }
            i = next;
        }
        let [Tok::Ident(name), Tok::Punct('='), Tok::Ident(dir), Tok::Punct('('), args @ .., Tok::Punct(')')] = &e[i..] else {
            return Err("c_ioctl!: an entry is NAME = _IOW(type, nr, ARG)".into());
        };
        let (ty, nr, arg) = match (dir.as_str(), split_commas(args).as_slice()) {
            ("_IO", [ty, nr]) => (tokens_to_c(ty), tokens_to_c(nr), None),
            ("_IOR" | "_IOW" | "_IOWR", [ty, nr, arg]) => (tokens_to_c(ty), tokens_to_c(nr), Some(tokens_to_rust(arg))),
            ("_IO", _) => return Err(format!("c_ioctl! {name}: expected _IO(type, nr)")),
            ("_IOR" | "_IOW" | "_IOWR", _) => return Err(format!("c_ioctl! {name}: expected {dir}(type, nr, ARG)")),
            _ => return Err(format!("c_ioctl! {name}: {dir} isn't _IO, _IOR, _IOW or _IOWR")),
        };
        if arg.is_none() && c_arg.is_some() {
            return Err(format!("c_ioctl! {name}: #[c(...)] is the argument's C type, and _IO has none"));
        }
        entries.push(CIoctlEntry { name: name.clone(), dir: dir.clone(), ty, nr, arg, c_arg });
    }
    if entries.is_empty() {
        return Err("c_ioctl!: expected NAME = _IOW(type, nr, ARG), ...".into());
    }
    Ok(CIoctl { cfg, entries })
}

/// c_enum!'s body: #[open|closed|flags] pub enum NAME: REPR { A = expr, B, ... }
/// - NAME `_` for an anonymous enum.
fn parse_enum(body: &[Tok], cfg: Option<Cfg>) -> Result<CEnumDef, String> {
    let mut kind = None;
    let mut i = 0;
    while let Some((attr, next)) = attr_at(body, i)? {
        for (n, k) in [("open", CEnumKind::Open), ("closed", CEnumKind::Closed), ("flags", CEnumKind::Flags)] {
            if matches!(attr, [Tok::Ident(a)] if a == n) {
                kind = Some(k);
            }
        }
        i = next;
    }
    i = skip_vis(body, i)?;
    if !body.get(i).is_some_and(|t| t.is_ident("enum")) {
        return Err("c_enum!: expected #[open|closed|flags] pub enum NAME: REPR { ... }".into());
    }
    let name = match ident_at(body, i + 1) {
        Some("_") => None,
        Some(n) => Some(n.to_string()),
        None => return Err("c_enum!: expected a name, or _".into()),
    };
    let what = name.clone().unwrap_or_else(|| "(anonymous)".into());
    let kind = kind.ok_or_else(|| format!("c_enum! {what}: #[open], #[closed] or #[flags]"))?;
    if !body.get(i + 2).is_some_and(|t| t.is_punct(':')) {
        return Err(format!("c_enum! {what}: expected `: REPR`"));
    }
    let repr = ident_at(body, i + 3)
        .filter(|r| int_bits(r).is_some())
        .ok_or_else(|| format!("c_enum! {what}: REPR is an integer type"))?
        .to_string();
    let (list, _) = macro_body(body, i + 4)?;

    let mut variants = Vec::new();
    for v in split_commas(list) {
        let j = skip_attrs(v, 0)?;
        let Some(vname) = ident_at(v, j) else {
            return Err(format!("c_enum! {what}: expected a variant name"));
        };
        let value = match &v[j + 1..] {
            [] => None,
            [Tok::Punct('='), expr @ ..] if !expr.is_empty() => Some(tokens_to_rust(expr)),
            _ => return Err(format!("c_enum! {what}::{vname}: expected `= value`, or nothing")),
        };
        variants.push((vname.to_string(), value));
    }
    Ok(CEnumDef { cfg, kind, name, repr, variants })
}

/// c_typedef!'s body: [#[c("C declarator")]] pub type NAME = TYPE;
fn parse_typedef(body: &[Tok], cfg: Option<Cfg>) -> Result<CTypedef, String> {
    let mut c_decl = None;
    let mut i = 0;
    while let Some((attr, next)) = attr_at(body, i)? {
        if let Some([Tok::Str(s, _)]) = attr_args(attr, "c") {
            c_decl = Some(s.clone());
        }
        i = next;
    }
    let i = skip_vis(body, i)?;
    match &body[i..] {
        [Tok::Ident(t), Tok::Ident(name), Tok::Punct('='), ty @ ..] if t == "type" && !ty.is_empty() => {
            let ty = ty.strip_suffix(&[Tok::Punct(';')]).unwrap_or(ty);
            Ok(CTypedef { cfg, name: name.clone(), ty: tokens_to_rust(ty), c_decl })
        }
        _ => Err("c_typedef!: expected pub type NAME = TYPE;".into()),
    }
}

/// c_same!'s body: struct|union NAME == TYPE [{ field, ... }]
fn parse_same(body: &[Tok]) -> Result<CSame, String> {
    let i = skip_attrs(body, 0)?;
    let (Some(kw), Some(name)) = (ident_at(body, i), ident_at(body, i + 1)) else {
        return Err("c_same!: expected struct NAME == TYPE".into());
    };
    if !matches!(kw, "struct" | "union") ||
        !body.get(i + 2).is_some_and(|t| t.is_punct('=')) ||
        !body.get(i + 3).is_some_and(|t| t.is_punct('=')) {
        return Err("c_same!: expected struct NAME == TYPE".into());
    }
    let rest = &body[i + 4..];
    let (ty, fields) = match rest.iter().position(|t| t.is_punct('{')) {
        Some(b) => {
            let (list, _) = macro_body(rest, b)?;
            let fields = split_commas(list).into_iter()
                .map(|f| match f {
                    [Tok::Ident(n)] => Ok(n.clone()),
                    _ => Err(format!("c_same! {name}: a field list is names")),
                })
                .collect::<Result<Vec<_>, _>>()?;
            (&rest[..b], fields)
        }
        None => (rest, Vec::new()),
    };
    if ty.is_empty() {
        return Err(format!("c_same! {name}: expected a type"));
    }
    Ok(CSame { c_type: format!("{kw} {name}"), rust_ty: tokens_to_rust(ty), fields })
}

/// c_const!'s body: `#[c_int]? pub const NAME: TYPE = EXPR;`. The #define
/// has TYPE's C type; #[c_int], int's: for a constant C had as a plain
/// number, so its type is the one C was written against, whatever type
/// Rust wants it in.
fn parse_const(body: &[Tok], cfg: Option<Cfg>) -> Result<CConst, String> {
    let mut i = 0;
    let mut c_int = false;
    while let Some((attr, next)) = attr_at(body, i)? {
        c_int |= matches!(attr, [Tok::Ident(n)] if n == "c_int");
        i = next;
    }
    let i = skip_vis(body, i)?;
    let err = || "c_const!: expected pub const NAME: TYPE = EXPR;".to_string();
    let [Tok::Ident(c), Tok::Ident(name), Tok::Punct(':'), rest @ ..] = &body[i..] else {
        return Err(err());
    };
    let eq = rest.iter().position(|t| t.is_punct('=')).ok_or_else(err)?;
    let (ty, expr) = (&rest[..eq], &rest[eq + 1..]);
    let expr = expr.strip_suffix(&[Tok::Punct(';')]).unwrap_or(expr);
    if c != "const" || ty.is_empty() || expr.is_empty() {
        return Err(err());
    }
    let c_type = match c_int {
        true => "int".to_string(),
        false => canon_base(ty).ok_or_else(|| format!("c_const! {name}: {} has no C type", tokens_to_rust(ty)))?,
    };
    Ok(CConst { cfg, name: name.clone(), ty: tokens_to_rust(ty), c_type, value: tokens_to_rust(expr) })
}

/// c_extern!'s body: `pub fn NAME(p: T, ...) -> R;` and `pub static [mut]
/// NAME: T;`, each with a #[cfg] if it has one.
fn parse_extern(body: &[Tok], cfg: Option<Cfg>) -> Result<CExtern, String> {
    let mut ex = CExtern { cfg, fns: Vec::new(), statics: Vec::new() };
    let mut i = 0;
    while i < body.len() {
        let (a, j) = item_attrs(body, i)?;
        let j = skip_vis(body, j)?;
        // the item's ';' - not an array type's
        let mut semi = j;
        let mut depth = 0;
        while semi < body.len() && !(depth == 0 && body[semi].is_punct(';')) {
            match &body[semi] {
                Tok::Punct('(' | '[' | '{') => depth += 1,
                Tok::Punct(')' | ']' | '}') => depth -= 1,
                _ => {}
            }
            semi += 1;
        }
        if semi == body.len() {
            return Err("c_extern!: an item without a ';'".into());
        }
        let item = &body[j..semi];
        match item {
            [Tok::Ident(f), Tok::Ident(name), Tok::Punct('('), ..] if f == "fn" => {
                let close = skip_group(item, 2)?;
                let mut params = Vec::new();
                let mut variadic = false;
                for p in split_commas(&item[3..close - 1]) {
                    match p {
                        [] => {}
                        [Tok::Punct('.'), Tok::Punct('.'), Tok::Punct('.')] => variadic = true,
                        [Tok::Ident(n), Tok::Punct(':'), ty @ ..] if !ty.is_empty() =>
                            params.push((n.clone(), tokens_to_rust(ty))),
                        _ => return Err(format!("c_extern!: fn {name}: parameter {} not understood",
                                                tokens_to_rust(p))),
                    }
                }
                let ret = match &item[close..] {
                    [] => None,
                    [Tok::Punct('-'), Tok::Punct('>'), ty @ ..] if !ty.is_empty() => Some(tokens_to_rust(ty)),
                    rest => return Err(format!("c_extern!: fn {name}: {} after the parameters",
                                               tokens_to_rust(rest))),
                };
                ex.fns.push(CFnDecl { cfg: a.cfg, name: name.clone(), params, variadic, ret });
            }
            [Tok::Ident(s), rest @ ..] if s == "static" => {
                let (mutable, rest) = match rest {
                    [Tok::Ident(m), rest @ ..] if m == "mut" => (true, rest),
                    _ => (false, rest),
                };
                match rest {
                    [Tok::Ident(name), Tok::Punct(':'), ty @ ..] if !ty.is_empty() =>
                        ex.statics.push(CStaticDecl { cfg: a.cfg, name: name.clone(), ty: tokens_to_rust(ty),
                                                      mutable, c_decl: a.c_decl }),
                    _ => return Err("c_extern!: expected static NAME: TYPE;".into()),
                }
            }
            _ => return Err(format!("c_extern!: {} isn't a fn or a static", tokens_to_rust(item))),
        }
        i = semi + 1;
    }
    Ok(ex)
}

/// tagged_union!'s body: `[vis] struct|union NAME { tag NAME: TYPE = TAG_ENUM
/// [by DETERMINANT], arms from LIST(params) => template | arm NAME: TYPE =
/// VALUE, ..., [pad: TYPE] }` - `by` for a union, and only for a union: a
/// struct's tag is its stored field.
fn parse_tagged_union(body: &[Tok], cfg: Option<Cfg>) -> Result<CTaggedUnion, String> {
    // `#[...]*`: for the arm types defined here, not the storage
    let mut i = 0;
    let mut arm_attrs = Vec::new();
    while let Some((_, next)) = attr_at(body, i)? {
        if body.get(next).is_some_and(|t| t.is_punct('*')) {
            arm_attrs.push(tokens_to_rust(&body[i..next + 1]));
            i = next + 1;
        } else {
            i = next;
        }
    }
    let i = skip_vis(body, i)?;
    let (union, name, inner) = match &body[i..] {
        [Tok::Ident(s), Tok::Ident(name), Tok::Punct('{'), inner @ .., Tok::Punct('}')]
            if s == "struct" || s == "union" => (s == "union", name.clone(), inner),
        _ => return Err("tagged_union!: expected struct|union NAME { ... }".into()),
    };
    let mut tu = CTaggedUnion { cfg, name, union, tag_field: String::new(), tag_ty: String::new(),
                                tag_enum: String::new(), by: None, from: None, arms: Vec::new(),
                                pad: None, packed: false, arm_attrs, defs: Vec::new() };
    for item in split_commas(inner) {
        let eq = item.iter().rposition(|t| t.is_punct('='));
        match item {
            [] => {}
            _ if tagged_def_at(item).is_some() => {
                let j = tagged_def_at(item).unwrap();
                tu.defs.push(CTaggedDef { name: ident_at(item, j + 1).unwrap().to_string(),
                                          text: tokens_to_rust(item) });
            }
            [Tok::Ident(kw), Tok::Ident(field), Tok::Punct(':'), rest @ ..] if kw == "tag" => {
                let eq = rest.iter().position(|t| t.is_punct('='))
                    .ok_or("tagged_union!: tag NAME: TYPE = TAG_ENUM [by DETERMINANT]")?;
                let by = rest.iter().position(|t| matches!(t, Tok::Ident(s) if s == "by"));
                tu.tag_field = field.clone();
                tu.tag_ty = tokens_to_rust(&rest[..eq]);
                tu.tag_enum = tokens_to_rust(&rest[eq + 1..by.unwrap_or(rest.len())]);
                tu.by = by.map(|b| tokens_to_rust(&rest[b + 1..]));
            }
            [Tok::Ident(kw), Tok::Ident(from), Tok::Ident(list), Tok::Punct('('), ..] if kw == "arms" && from == "from" => {
                let close = skip_group(item, 3)?;
                let params = split_commas(&item[4..close - 1]).into_iter()
                    .map(tokens_to_rust)
                    .collect();
                let template = match &item[close..] {
                    [Tok::Punct('='), Tok::Punct('>'), t @ ..] if !t.is_empty() => tokens_to_rust(t),
                    _ => return Err("tagged_union!: arms from LIST(params) => name: Type = value".into()),
                };
                tu.from = Some((list.clone(), params, template));
            }
            [Tok::Ident(kw), Tok::Ident(arm), Tok::Punct(':'), ..] if kw == "arm" => {
                let eq = eq.ok_or("tagged_union!: arm NAME: TYPE = VALUE")?;
                tu.arms.push(CTaggedArm { name: arm.clone(), ty: tokens_to_rust(&item[3..eq]),
                                          value: tokens_to_rust(&item[eq + 1..]) });
            }
            [Tok::Ident(kw), Tok::Punct(':'), ty @ ..] if kw == "pad" && !ty.is_empty() =>
                tu.pad = Some(tokens_to_rust(ty)),
            [Tok::Ident(kw)] if kw == "packed" => tu.packed = true,
            _ => return Err(format!("tagged_union!: {} not understood", tokens_to_rust(item))),
        }
    }
    if tu.tag_field.is_empty() {
        return Err("tagged_union!: no tag".into());
    }
    match (tu.union, &tu.by) {
        (true, None) => return Err(format!("tagged_union! {}: a union has no stored tag - \
                                            `by DETERMINANT` says how its arms' bytes give it",
                                           tu.name)),
        (false, Some(_)) => return Err(format!("tagged_union! {}: a struct's tag is its stored \
                                                field - `by` is for a union", tu.name)),
        _ => {}
    }
    Ok(tu)
}

/// tagged_union!'s arms from its list's entries - each the source of one
/// entry's arguments: bound to the template's params, `..` the rest, and
/// `A ## B` pasted into one identifier.
#[allow(dead_code)]
pub fn tagged_union_arms(tu: &CTaggedUnion, entries: &[String]) -> Result<Vec<CTaggedArm>, String> {
    let (list, params, template) = tu.from.as_ref().ok_or("tagged_union!: no arms from a list")?;
    let template = tokenize(template)?;
    let mut arms = Vec::new();
    for entry in entries {
        let etoks = tokenize(entry)?;
        let args = split_commas(&etoks);
        let mut bound: Vec<(&str, &[Tok])> = Vec::new();
        for (n, p) in params.iter().enumerate() {
            if p == ".." {
                break;
            }
            let arg = args.get(n).ok_or_else(|| format!("{list}: entry ({entry}) has no {p}"))?;
            bound.push((p, arg));
        }
        // substitute, then paste
        let mut subst: Vec<Tok> = Vec::new();
        for t in &template {
            match t {
                Tok::Ident(s) => match bound.iter().find(|(p, _)| p == s) {
                    Some((_, arg)) => subst.extend(arg.iter().cloned()),
                    None => subst.push(t.clone()),
                },
                _ => subst.push(t.clone()),
            }
        }
        let mut out: Vec<Tok> = Vec::new();
        let mut k = 0;
        while k < subst.len() {
            if subst[k].is_punct('#') && subst.get(k + 1).is_some_and(|t| t.is_punct('#')) {
                let (Some(Tok::Ident(a) | Tok::Lit(a)), Some(Tok::Ident(b) | Tok::Lit(b))) =
                    (out.pop(), subst.get(k + 2).cloned()) else {
                    return Err(format!("{list}: ## between non-identifiers in the template"));
                };
                out.push(Tok::Ident(a + &b));
                k += 3;
                continue;
            }
            out.push(subst[k].clone());
            k += 1;
        }
        let colon = out.iter().position(|t| t.is_punct(':'));
        let eq = out.iter().rposition(|t| t.is_punct('='));
        match (&out[..], colon, eq) {
            ([Tok::Ident(name), ..], Some(1), Some(eq)) if eq > 2 => arms.push(CTaggedArm {
                name:  name.clone(),
                ty:    tokens_to_rust(&out[2..eq]),
                value: tokens_to_rust(&out[eq + 1..]),
            }),
            _ => return Err(format!("{list}: the template gives {} - not name: Type = value",
                                    tokens_to_rust(&out))),
        }
    }
    Ok(arms)
}

/// The record's form of @tu, with @arms: each written out.
#[allow(dead_code)]
pub fn tagged_union_record_text(tu: &CTaggedUnion, arms: &[CTaggedArm]) -> String {
    let by = tu.by.as_ref().map(|d| format!(" by {d}")).unwrap_or_default();
    let mut t = format!("tagged_union! {{ {} {{ tag {}: {} = {}{by}, ",
                        tu.c_type(), tu.tag_field, tu.tag_ty, tu.tag_enum);
    for a in arms {
        t.push_str(&format!("arm {}: {} = {}, ", a.name, a.ty, a.value));
    }
    if let Some(p) = &tu.pad {
        t.push_str(&format!("pad: {p}, "));
    }
    if tu.packed {
        t.push_str("packed, ");
    }
    t + "} }"
}

impl CTaggedUnion {
    /// What C calls it: `struct NAME` or `union NAME`.
    pub fn c_type(&self) -> String {
        format!("{} {}", if self.union { "union" } else { "struct" }, self.name)
    }
}

/// tagged_union!'s C: for a struct, the tag, then an anonymous union of the
/// arms - inside an anonymous union with the pad, if there is one; for a
/// union, the arms and the pad. `packed`: the struct and the arms' union are
/// __packed, or the union.
fn emit_tagged_union(tu: &CTaggedUnion, kind: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let c = |ty: &str, decl: &str| {
        // a payload is a type the file defines, by its bare name, or c::NAME
        let bare = ty.trim().trim_start_matches("c::").trim_start_matches("cs::").trim();
        match kind(bare) {
            Some(k) if !bare.contains(|ch: char| !(ch.is_alphanumeric() || ch == '_')) =>
                Some(format!("{k} {decl}")),
            _ => proto_c(ty, decl, kind),
        }
        .ok_or_else(|| format!("tagged_union! {}: {ty} has no C rendering", tu.name))
    };
    if tu.union {
        let mut body = String::new();
        for a in &tu.arms {
            body.push_str(&format!("\t{};\n", c(&a.ty, &c_ident(&a.name))?));
        }
        if let Some(p) = &tu.pad {
            body.push_str(&format!("\t{};\n", c(p, "_pad")?));
        }
        let packed = if tu.packed { " __packed" } else { "" };
        return under_cfg(&tu.cfg, format!("union {} {{\n{body}}}{packed};\n", tu.name));
    }
    let packed = if tu.packed { " __packed" } else { "" };
    let mut arms = String::new();
    for a in &tu.arms {
        arms.push_str(&format!("\t\t{};\n", c(&a.ty, &c_ident(&a.name))?));
    }
    let tag = c(&tu.tag_ty, &c_ident(&tu.tag_field))?;
    let tagged = format!("\t{tag};\n\tunion {{\n{arms}\t}}{packed};\n");
    match &tu.pad {
        Some(p) => {
            let indented: String = tagged.lines().map(|l| format!("\t\t{l}\n")).collect();
            let body = format!("\tunion {{\n\t\tstruct {{\n{indented}\t\t}}{packed};\n\t\t{};\n\t}};\n",
                               c(p, "_pad")?);
            under_cfg(&tu.cfg, format!("struct {} {{\n{body}}};\n", tu.name))
        }
        None => under_cfg(&tu.cfg, format!("struct {} {{\n{tagged}}}{packed};\n", tu.name)),
    }
}

/// The C declaration of @decl as Rust type @ty, for a prototype: canon_c()'s,
/// a zero-length array being an extern's unsized one.
pub fn proto_c(ty: &str, decl: &str, kind: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    canon_c(ty, decl, true, kind)
}

/// The C declaration of @decl as Rust type @ty - or None if @ty has no C
/// rendering. A pointer keeps its const - `*const T` is `const T *` - a
/// function pointer is one, c::NAME is what @kind says C calls it (`struct
/// foo`, `enum foo`, a typedef's own name), and a zero-length array is a
/// flexible array: x[] at the end (@last), x[0] elsewhere.
pub fn canon_c(ty: &str, decl: &str, last: bool, kind: &dyn Fn(&str) -> Option<String>)
    -> Option<String>
{
    let toks = tokenize(ty).ok()?;
    proto_toks(&toks, decl.to_string(), false, last, kind)
}

fn proto_toks(toks: &[Tok], decl: String, konst: bool, last: bool,
              kind: &dyn Fn(&str) -> Option<String>)
    -> Option<String>
{
    let q = if konst { "const " } else { "" };
    match toks {
        // Option<unsafe extern "C" fn(ARGS) -> RET>: a function pointer.
        [Tok::Ident(o), Tok::Punct('<'), inner @ .., Tok::Punct('>')] if o == "Option" => {
            let f = inner.iter().position(|t| t.is_ident("fn"))?;
            if !inner.get(f + 1)?.is_punct('(') {
                return None;
            }
            let close = skip_group(inner, f + 1).ok()?;
            let args = split_commas(&inner[f + 2..close - 1]).into_iter()
                .filter(|p| !p.is_empty())
                .map(|p| match p {
                    [Tok::Punct('.'), Tok::Punct('.'), Tok::Punct('.')] => Some("...".to_string()),
                    // a named parameter: C's, named - the `:` not half of a
                    // path's `::`, as in an unnamed c::foo
                    [Tok::Ident(n), Tok::Punct(':'), ty @ ..] if ty.first().is_some_and(|t| !t.is_punct(':')) => {
                        let n = if n == "_" { String::new() } else { c_ident(n) };
                        proto_toks(ty, n, false, false, kind)
                    }
                    ty => proto_toks(ty, String::new(), false, false, kind),
                })
                .collect::<Option<Vec<_>>>()?;
            let args = if args.is_empty() { "void".to_string() } else { args.join(", ") };
            let inner_decl = format!("(*{q}{decl})({args})");
            match &inner[close..] {
                [] => Some(format!("void {inner_decl}")),
                [Tok::Punct('-'), Tok::Punct('>'), ret @ ..] =>
                    proto_toks(ret, inner_decl, false, false, kind),
                _ => None,
            }
        }
        [Tok::Punct('['), inner @ .., Tok::Punct(']')] => {
            let semi = inner.iter().rposition(|t| t.is_punct(';'))?;
            let n = c_dim(&inner[semi + 1..])?;
            let n = if n == "0" && last { String::new() } else { n };
            let decl = if decl.starts_with('*') { format!("({decl})") } else { decl };
            proto_toks(&inner[..semi], format!("{decl}[{n}]"), konst, false, kind)
        }
        [Tok::Punct('*'), Tok::Ident(m), inner @ ..] if m == "mut" || m == "const" =>
            proto_toks(inner, format!("*{q}{decl}"), m == "const", false, kind),
        // never returns: __noreturn is an attribute, not the type
        [Tok::Punct('!')] => Some(format!("void {decl}")),
        [Tok::Ident(ns), Tok::Punct(':'), Tok::Punct(':'), Tok::Ident(name)] if is_c_ns(ns) => {
            let base = kind(name).unwrap_or_else(|| canon_base(toks).unwrap());
            Some(format!("{q}{base} {decl}").trim_end().to_string())
        }
        _ => canon_base(toks).map(|b| format!("{q}{b} {decl}").trim_end().to_string()),
    }
}

/// c_extern!'s C: a prototype for each fn, an extern declaration for each
/// static.
fn emit_extern(ex: &CExtern, kind: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let c = |ty: &str, decl: &str, what: &str| proto_c(ty, decl, kind)
        .ok_or_else(|| format!("{what}: {ty} has no C rendering"));
    let mut t = String::new();
    // A struct a prototype names before it's declared would be one of the
    // prototype's own, which conflicts with the real one: declare each first.
    // An enum too - incomplete until its definition, as GNU C allows.
    let mut structs = std::collections::BTreeSet::new();
    let types = ex.fns.iter()
        .flat_map(|f| f.params.iter().map(|(_, ty)| ty).chain(f.ret.as_ref()))
        .chain(ex.statics.iter().filter(|s| s.c_decl.is_none()).map(|s| &s.ty));
    for ty in types {
        let rendered = proto_c(ty, "", kind).unwrap_or_default();
        let mut words = rendered.split(|ch: char| !(ch.is_alphanumeric() || ch == '_'));
        while let Some(w) = words.next() {
            if w == "struct" || w == "union" || w == "enum" {
                if let Some(n) = words.find(|n| !n.is_empty()) {
                    structs.insert(format!("{w} {n};\n"));
                }
            }
        }
    }
    t.extend(structs);
    for f in &ex.fns {
        let mut params = f.params.iter()
            .map(|(_, ty)| c(ty, "", &format!("fn {}", f.name)))
            .collect::<Result<Vec<_>, _>>()?;
        if f.variadic {
            params.push("...".into());
        }
        let params = if params.is_empty() { "void".to_string() } else { params.join(", ") };
        let decl = format!("{}({params})", f.name);
        let proto = match &f.ret {
            None => format!("void {decl}"),
            Some(r) => c(r, &decl, &format!("fn {}", f.name))?,
        };
        t.push_str(&under_cfg(&f.cfg, format!("{proto};\n"))?);
    }
    for s in &ex.statics {
        let decl = match &s.c_decl {
            Some(d) => d.clone(),
            None => c(&s.ty, &s.name, &format!("static {}", s.name))?,
        };
        t.push_str(&under_cfg(&s.cfg, format!("extern {decl};\n"))?);
    }
    under_cfg(&ex.cfg, t)
}

// ---- emitting C ------------------------------------------------------------
//
// One emitter for every path that writes C: the same items give the same C.

/// The paths C's namespace goes by: c - and cs, in modules whose c is still
/// bindgen's (btree/types.rs and the like, which have code of their own).
fn is_c_ns(s: &str) -> bool {
    s == "c" || s == "cs"
}

/// The C base type Rust type @toks renders as - the canonical rendering, by
/// which a field needs no #[c]: integers by their plain names, zerocopy's
/// endian integers as __leN/__beN, c::foo as struct foo - unless foo ends in
/// _t, a typedef.
fn canon_base(toks: &[Tok]) -> Option<String> {
    let path: Vec<&str> = toks.iter().filter_map(|t| match t {
        Tok::Ident(s) => Some(s.as_str()),
        Tok::Punct(':') => None,
        _ => Some("?"),
    }).collect();
    let s = match path.as_slice() {
        ["u8"] => "u8", ["u16"] => "u16", ["u32"] => "u32", ["u64"] => "u64",
        ["i8"] => "s8", ["i16"] => "s16", ["i32"] => "s32", ["i64"] => "s64",
        ["bool"] => "bool", ["usize"] => "size_t", ["isize"] => "ssize_t",
        [.., "c_void"] => "void", [.., "c_char"] => "char",
        [.., "c_short"] => "short", [.., "c_ushort"] => "unsigned short",
        [.., "c_int"] => "int", [.., "c_uint"] => "unsigned",
        [.., "c_long"] => "long", [.., "c_ulong"] => "unsigned long",
        [.., "c_longlong"] => "long long", [.., "c_ulonglong"] => "unsigned long long",
        [e, u] if (*e == "le" || *e == "be") && u.starts_with('U') => {
            return Some(format!("__{e}{}", &u[1..]));
        }
        [ns, name] if is_c_ns(ns) => {
            return Some(if name.ends_with("_t") { name.to_string() } else { format!("struct {name}") });
        }
        _ => return None,
    };
    Some(s.to_string())
}

/// A value's Rust source as C, for the --emit path, which has no evaluated
/// values: the same tokens, but for a constant's c:: path. The records path
/// has numbers.
fn rust_expr_to_c(x: &str) -> String {
    match tokenize(x) {
        Ok(toks) => {
            let toks: Vec<Tok> = toks.into_iter()
                .filter(|t| !matches!(t, Tok::Ident(s) if is_c_ns(s)))
                .collect::<Vec<_>>();
            // `c :: NAME` -> `NAME`: the path's ::, now leading, dropped.
            let mut out = Vec::new();
            let mut k = 0;
            while k < toks.len() {
                if toks[k].is_punct(':') && toks.get(k + 1).is_some_and(|t| t.is_punct(':')) &&
                    (k == 0 || !matches!(toks[k - 1], Tok::Ident(_))) {
                    k += 2;
                    continue;
                }
                out.push(toks[k].clone());
                k += 1;
            }
            tokens_to_c(&out)
        }
        Err(_) => x.to_string(),
    }
}

/// @text, under #if @cfg if there is one.
fn under_cfg(cfg: &Option<Cfg>, text: String) -> Result<String, String> {
    Ok(match cfg {
        Some(c) => format!("#if {}\n{text}#endif\n", c.to_c()?),
        None => text,
    })
}

/// A bitfield member's C name: bitfield-struct takes a leading '_' for
/// padding, so C's _foo is foo_raw in Rust.
fn bitfield_c_name(name: &str) -> String {
    match name.strip_suffix("_raw") {
        Some(stem) => format!("_{stem}"),
        None => c_ident(name),
    }
}

/// The members of bitfield type @bf, as C declares them: least significant
/// first on little-endian bitfields, the reverse on big-endian - so each is at
/// the same bits of the same native-endian word either way.
/// A bitfield-struct member that's padding: Rust's, where C leaves bits
/// unused.
#[allow(dead_code)]
fn is_bitfield_pad(name: &str) -> bool {
    name.starts_with("__pad")
}

fn emit_bitfield(bf: &CBitfieldDef) -> Result<String, String> {
    let decl = |(name, ty, width): &(String, String, u32)| -> Result<String, String> {
        let base = canon_base(&tokenize(ty)?)
            .ok_or_else(|| format!("{}.{name}: {ty} has no C type", bf.name))?;
        let n = bitfield_c_name(name);
        Ok(if Some(*width) == int_bits(ty) {
            format!("\t{base} {n};\n")
        } else {
            format!("\t{base} {n}:{width};\n")
        })
    };
    // Padding - __pad, __padN, where C skips to a member's next unit or past
    // the run's end - C does by itself, from the start of the word; from its
    // end, on big-endian, it's an unnamed member.
    let mut le = String::new();
    for f in bf.fields.iter().filter(|f| !is_bitfield_pad(&f.0)) {
        le.push_str(&decl(f)?);
    }
    let mut be = String::new();
    for f in bf.fields.iter().rev() {
        if is_bitfield_pad(&f.0) {
            let base = canon_base(&tokenize(&f.1)?)
                .ok_or_else(|| format!("{}.{}: {} has no C type", bf.name, f.0, f.1))?;
            be.push_str(&format!("\t{base} :{};\n", f.2));
        } else {
            be.push_str(&decl(f)?);
        }
    }
    Ok(format!("#if defined(__LITTLE_ENDIAN_BITFIELD)\n{le}\
                #elif defined(__BIG_ENDIAN_BITFIELD)\n{be}\
                #else\n#error edit for your odd byteorder.\n#endif\n"))
}

/// Whether C writes @f's type in its place: a bare #[c_anon], #[c_inline],
/// or #[c_struct_group].
fn in_place(f: &CFieldDef) -> bool {
    f.inline || f.group || (f.anon && f.c_decl.is_none())
}

/// Rust type @ty as an element type and C's array dimensions - `[[T; 2]; N]`
/// is T and [N][2] - or @ty itself and none. None if a dimension has no C
/// rendering.
fn array_of(ty: &str) -> Option<(String, String)> {
    let toks = tokenize(ty).ok()?;
    let mut elem: &[Tok] = &toks;
    let mut dims = String::new();
    while let [Tok::Punct('['), inner @ .., Tok::Punct(']')] = elem {
        let semi = inner.iter().rposition(|t| t.is_punct(';'))?;
        dims.push_str(&format!("[{}]", c_dim(&inner[semi + 1..])?));
        elem = &inner[..semi];
    }
    Some((tokens_to_rust(elem), dims))
}

/// An array dimension as C: a number, or a constant C has by name - a usize
/// one, or another `as usize`.
fn c_dim(toks: &[Tok]) -> Option<String> {
    match toks {
        [Tok::Lit(n)] => Some(n.trim_end_matches("usize").to_string()),
        [Tok::Ident(c), Tok::Punct(':'), Tok::Punct(':'), Tok::Ident(name), rest @ ..] if is_c_ns(c) =>
            match rest {
                [] => Some(name.clone()),
                [Tok::Ident(as_), Tok::Ident(us)] if as_ == "as" && us == "usize" => Some(name.clone()),
                _ => None,
            },
        _ => None,
    }
}

/// The type field @f declares in its place, if it does: a struct or union of
/// @items - for #[c_inline], the element type of an array of one.
#[allow(dead_code)]
pub fn in_place_def<'a>(f: &CFieldDef, items: &'a [CItem]) -> Option<&'a CStructDef> {
    if !in_place(f) {
        return None;
    }
    let (elem, dims) = array_of(&f.ty)?;
    if !dims.is_empty() && !f.inline {
        return None;
    }
    items.iter().find_map(|i| match i {
        CItem::Struct(d) if d.name == elem.trim() => Some(d),
        _ => None,
    })
}

/// The types of @items that fields declare in their place: C has no
/// definition of its own for them.
#[allow(dead_code)]
pub fn in_place_types(items: &[CItem]) -> Vec<&str> {
    items.iter()
        .filter_map(|i| match i {
            CItem::Struct(d) => Some(d),
            _ => None,
        })
        .flat_map(|d| d.fields.iter())
        .filter_map(|f| in_place_def(f, items))
        .map(|d| d.name.as_str())
        .collect()
}

/// A zero-length array in a union: C can't have a flexible array member
/// there, but the kernel's __DECLARE_FLEX_ARRAY wraps one in an anonymous
/// struct, which can.
fn union_flex_array(ty: &str, name: &str) -> Option<String> {
    let toks = tokenize(ty).ok()?;
    let [Tok::Punct('['), inner @ .., Tok::Punct(']')] = toks.as_slice() else {
        return None;
    };
    let semi = inner.iter().rposition(|t| t.is_punct(';'))?;
    if !matches!(&inner[semi + 1..], [Tok::Lit(n)] if n == "0") {
        return None;
    }
    Some(format!("__DECLARE_FLEX_ARRAY({}, {name})", canon_base(&inner[..semi])?))
}

/// The attributes after @def's closing brace - per configuration, if its
/// repr is, unless C says it once for all of them.
fn c_attrs(def: &CStructDef) -> Result<String, String> {
    Ok(if let Some(a) = &def.c_align {
        format!(" __aligned({a})")
    } else if def.cfg_reprs.is_empty() {
        def.repr.to_c()
    } else {
        let mut a = String::from("\n");
        for (k, (cfg, repr)) in def.cfg_reprs.iter().enumerate() {
            let kw = if k == 0 { "#if" } else { "#elif" };
            a.push_str(&format!("{kw} {}\n{}\n", cfg.to_c()?, repr.to_c()));
        }
        a.push_str("#endif\n");
        a
    })
}

/// A struct's C definition. @opaque gives the C declaration of a Rust-only
/// field - one with no #[c] and no canonical rendering.
fn emit_struct(def: &CStructDef, items: &[CItem],
               opaque: &dyn Fn(&CStructDef, &CFieldDef) -> Result<String, String>,
               kind: &dyn Fn(&str) -> Option<String>)
    -> Result<String, String>
{
    if !def.repr_c() {
        return Err(format!("{} must be #[repr(C)]: C shares it", def.name));
    }
    let body = emit_members(def, items, opaque, kind)?;
    let attrs = c_attrs(def)?;
    let kw = def.kind.keyword();
    under_cfg(&def.cfg, match &def.typedef {
        Some(t) if def.typedef_anon => format!("typedef {kw} {{\n{body}}}{attrs} {t};\n"),
        Some(t) => format!("typedef {kw} {} {{\n{body}}}{attrs} {t};\n", def.name),
        None => format!("{kw} {} {{\n{body}}}{attrs};\n", def.name),
    })
}

/// @def's members, a tab in - an anonymous member's a tab further.
fn emit_members(def: &CStructDef, items: &[CItem],
                opaque: &dyn Fn(&CStructDef, &CFieldDef) -> Result<String, String>,
                kind: &dyn Fn(&str) -> Option<String>)
    -> Result<String, String>
{
    // Consecutive fields under the same cfg share one #if.
    let mut body = String::new();
    let mut open: Option<String> = None;
    for (n, f) in def.fields.iter().enumerate() {
        let last = n + 1 == def.fields.len();
        let text = if f.bitfield {
            let bf = items.iter().find_map(|i| match i {
                CItem::Bitfield(b) if b.name == f.ty.trim() => Some(b),
                _ => None,
            }).ok_or_else(|| format!("{}.{}: #[c_bitfield], but {} isn't a #[bitfield] type here",
                                     def.name, f.name, f.ty))?;
            emit_bitfield(bf)?
        } else if in_place(f) {
            let attr = match (f.inline, f.group) {
                (true, _) => "c_inline",
                (_, true) => "c_struct_group",
                _ => "c_anon",
            };
            let inner = in_place_def(f, items).ok_or_else(|| format!(
                "{}.{}: #[{attr}], but {} isn't a struct or union here", def.name, f.name, f.ty))?;
            if !inner.repr_c() || inner.typedef.is_some() {
                return Err(format!("{}.{}: #[{attr}]'s type {} must be #[repr(C)], and not a typedef",
                                   def.name, f.name, inner.name));
            }
            let members: String = emit_members(inner, items, opaque, kind)?
                .lines()
                .map(|l| if l.starts_with('#') { format!("{l}\n") } else { format!("\t{l}\n") })
                .collect();
            if f.group {
                // the kernel's: a union of the members, and the same as a
                // struct of that name
                let plain = CRepr { repr_c: true, ..CRepr::default() };
                if inner.kind != CKind::Struct || inner.repr != plain
                    || inner.c_align.is_some() || !inner.cfg_reprs.is_empty() {
                    return Err(format!("{}.{}: #[c_struct_group]'s type {} must be a plain #[repr(C)] struct: \
                                        struct_group() takes no attributes", def.name, f.name, inner.name));
                }
                format!("\tstruct_group({},\n{members}\t);\n", f.name)
            } else {
                let name = match array_of(&f.ty) {
                    Some((_, dims)) if f.inline => format!(" {}{dims}", f.name),
                    _ => String::new(),
                };
                format!("\t{} {{\n{members}\t}}{}{name};\n", inner.kind.keyword(), c_attrs(inner)?)
            }
        } else if f.c_decl.as_deref() == Some("") {
            // #[c_anon("")]: Rust's alone, and nothing in C - a zero-sized
            // alignment marker, say.
            String::new()
        } else if let Some(decl) = &f.c_decl {
            // An x-macro expansion, as written, ends with its #undef: the
            // members are its, and a ; would be on the directive's line.
            let directive = decl.lines().last().is_some_and(|l| l.trim_start().starts_with('#'));
            format!("\t{decl}{}\n", if directive { "" } else { ";" })
        } else if let Some(decl) = union_flex_array(&f.ty, &f.name).filter(|_| def.kind == CKind::Union) {
            format!("\t{decl};\n")
        } else if let Some(decl) = canon_c(&f.ty, &f.name, last, kind) {
            format!("\t{decl};\n")
        } else {
            format!("\t{};\n", opaque(def, f)?)
        };
        let cond = f.cfg.as_ref().map(Cfg::to_c).transpose()?;
        if cond != open {
            if open.is_some() {
                body.push_str("#endif\n");
            }
            if let Some(c) = &cond {
                body.push_str(&format!("#if {c}\n"));
            }
            open = cond;
        }
        body.push_str(&text);
    }
    if open.is_some() {
        body.push_str("#endif\n");
    }
    Ok(body)
}

/// The C for @items, in order. @opaque as for emit_struct().
#[allow(dead_code)]
pub fn emit_items(items: &[CItem],
                  opaque: &dyn Fn(&CStructDef, &CFieldDef) -> Result<String, String>,
                  kind: &dyn Fn(&str) -> Option<String>)
    -> Result<String, String>
{
    let in_place = in_place_types(items);
    let mut out = String::new();
    for item in items {
        let text = match item {
            CItem::Extern(ex) => emit_extern(ex, kind)?,
            CItem::TaggedUnion(tu) => emit_tagged_union(tu, kind)?,
            // Declared where its #[c_anon] or #[c_inline] field is.
            CItem::Struct(def) if in_place.contains(&def.name.as_str()) => continue,
            CItem::Struct(def) => emit_struct(def, items, opaque, kind)
                .map_err(|e| format!("struct {}: {e}", def.name))?,
            // Declared in the C of the struct that has one.
            CItem::Bitfield(_) => continue,
            CItem::Verbatim(v) => under_cfg(&v.cfg, format!("{}\n", v.text))?,
            CItem::XMacro(x) => {
                // The mappings for the lists spliced in, first: C expands
                // them where the list is used.
                let mut t = String::new();
                for e in &x.entries {
                    if let CXEntry::Sub { map: Some(m), .. } = e {
                        t.push_str(&format!("#define {}({})\t\\\n\t{}({})\n",
                                            m.name, m.params.join(", "), x.callback, m.template));
                    }
                }
                t.push_str(&format!("#define {}()", x.name));
                for e in &x.entries {
                    match e {
                        CXEntry::Args(a) => t.push_str(&format!("\t\\\n\t{}({a})", x.callback)),
                        CXEntry::Sub { list, .. } => t.push_str(&format!("\t\\\n\t{list}()")),
                    }
                }
                under_cfg(&x.cfg, t + "\n")?
            }
            CItem::Bitmask(b) => {
                let mut t = String::new();
                for e in &b.entries {
                    t.push_str(&format!("{}({}, {}, {}, {}, {});\n",
                                        b.macro_name, e.name, b.c_type, b.field, e.start, e.end));
                }
                under_cfg(&b.cfg, t)?
            }
            CItem::Ioctl(io) => {
                let mut t = String::new();
                for e in &io.entries {
                    let arg = match (&e.arg, &e.c_arg) {
                        (None, _) => String::new(),
                        (Some(_), Some(c)) => format!(", {c}"),
                        (Some(a), None) => {
                            let c = canon_c(a, "", false, kind)
                                .ok_or_else(|| format!("{}: {a} has no C rendering - it needs a #[c(\"...\")]", e.name))?;
                            format!(", {}", c.trim())
                        }
                    };
                    t.push_str(&format!("#define {}\t{}({}, {}{arg})\n", e.name, e.dir, e.ty, e.nr));
                }
                under_cfg(&io.cfg, t)?
            }
            CItem::Enum(e) => {
                let mut t = format!("enum {}{} {{\n", e.kind.marker(),
                                    e.name.as_deref().map(|n| format!(" {n}")).unwrap_or_default());
                for (v, value) in &e.variants {
                    match value {
                        Some(x) => t.push_str(&format!("\t{v} = {},\n", rust_expr_to_c(x))),
                        None => t.push_str(&format!("\t{v},\n")),
                    }
                }
                under_cfg(&e.cfg, t + "};\n")?
            }
            CItem::Const(c) => under_cfg(&c.cfg, format!("#define {}\t{}\n", c.name, c.c_value()?))?,
            CItem::Typedef(t) => {
                let decl = match &t.c_decl {
                    Some(d) => d.clone(),
                    None => canon_c(&t.ty, &t.name, false, kind)
                        .ok_or_else(|| format!("typedef {}: {} has no C rendering - it needs a #[c(\"...\")]",
                                               t.name, t.ty))?,
                };
                under_cfg(&t.cfg, format!("typedef {decl};\n"))?
            }
            // C's own definition: the generator asserts it's the Rust type's.
            CItem::Same(_) => continue,
        };
        out.push('\n');
        out.push_str(&text);
    }
    Ok(out)
}

// ---- entry points ----------------------------------------------------------

/// Parse a derive's input: exactly one struct or union.
#[allow(dead_code)]
pub fn parse_cstruct(src: &str) -> Result<CStructDef, String> {
    let toks = tokenize(src)?;
    let (a, i) = item_attrs(&toks, 0)?;
    parse_struct(&toks, i, a)?
        .map(|(def, _)| def)
        .ok_or_else(|| "#[derive(CStruct)] is for structs and unions with named fields".to_string())
}

/// Parse c_xmacro!'s input.
#[allow(dead_code)]
pub fn parse_xmacro_input(src: &str) -> Result<CXMacro, String> {
    parse_xmacro(&tokenize(src)?, None)
}

/// Parse c_bitmask!'s input.
#[allow(dead_code)]
pub fn parse_bitmask_input(src: &str) -> Result<CBitmask, String> {
    parse_bitmask(&tokenize(src)?, None)
}

/// Every C-shared item in a source file, in order.
#[allow(dead_code)]
pub fn parse_items(src: &str) -> Result<Vec<CItem>, String> {
    let toks = tokenize(src)?;
    let mut out = Vec::new();

    let mut i = 0;
    while i < toks.len() {
        // An item: its attributes, then what it is. Anything else - an impl,
        // a fn, a use - is stepped over a token at a time; its attributes are
        // read and dropped with it.
        let (a, j) = item_attrs(&toks, i)?;

        match (ident_at(&toks, j), toks.get(j + 1)) {
            (Some("c_verbatim"), Some(t)) if t.is_punct('!') => {
                let (body, next) = macro_body(&toks, j + 2)?;
                let text = match body {
                    [Tok::Str(s, _)] => s.trim_matches('\n').to_string(),
                    _ => return Err("c_verbatim!: takes one string".into()),
                };
                out.push(CItem::Verbatim(CVerbatim { cfg: a.cfg, text }));
                i = next;
                continue;
            }
            (Some("c_xmacro"), Some(t)) if t.is_punct('!') => {
                let (body, next) = macro_body(&toks, j + 2)?;
                out.push(CItem::XMacro(parse_xmacro(body, a.cfg)?));
                i = next;
                continue;
            }
            (Some("c_bitmask"), Some(t)) if t.is_punct('!') => {
                let (body, next) = macro_body(&toks, j + 2)?;
                out.push(CItem::Bitmask(parse_bitmask(body, a.cfg)?));
                i = next;
                continue;
            }
            (Some("c_ioctl"), Some(t)) if t.is_punct('!') => {
                let (body, next) = macro_body(&toks, j + 2)?;
                out.push(CItem::Ioctl(parse_ioctl(body, a.cfg)?));
                i = next;
                continue;
            }
            (Some("c_enum"), Some(t)) if t.is_punct('!') => {
                let (body, next) = macro_body(&toks, j + 2)?;
                out.push(CItem::Enum(parse_enum(body, a.cfg)?));
                i = next;
                continue;
            }
            (Some("c_const"), Some(t)) if t.is_punct('!') => {
                let (body, next) = macro_body(&toks, j + 2)?;
                out.push(CItem::Const(parse_const(body, a.cfg)?));
                i = next;
                continue;
            }
            (Some("c_typedef"), Some(t)) if t.is_punct('!') => {
                let (body, next) = macro_body(&toks, j + 2)?;
                out.push(CItem::Typedef(parse_typedef(body, a.cfg)?));
                i = next;
                continue;
            }
            (Some("tagged_union"), Some(t)) if t.is_punct('!') => {
                let (body, next) = macro_body(&toks, j + 2)?;
                out.push(CItem::TaggedUnion(parse_tagged_union(body, a.cfg)?));
                i = next;
                continue;
            }
            // rust_c_extern!'s C is c_extern!'s: the prototypes
            (Some("c_extern" | "rust_c_extern"), Some(t)) if t.is_punct('!') => {
                let (body, next) = macro_body(&toks, j + 2)?;
                out.push(CItem::Extern(parse_extern(body, a.cfg)?));
                i = next;
                continue;
            }
            (Some("c_same"), Some(t)) if t.is_punct('!') => {
                let (body, next) = macro_body(&toks, j + 2)?;
                out.push(CItem::Same(parse_same(body)?));
                i = next;
                continue;
            }
            // The macros record the items nest! unpacks to; source text has
            // nestify's syntax, which this doesn't read.
            (Some("nest"), Some(t)) if t.is_punct('!') =>
                return Err("nest!: --emit doesn't read nestify's syntax - the records path does".into()),
            _ => {}
        }

        if let Some(storage) = a.bitfield.clone() {
            if let Some((def, next)) = parse_bitfield(&toks, j, a.cfg.clone(), storage)? {
                out.push(CItem::Bitfield(def));
                i = next;
                continue;
            }
        }
        if a.derives {
            if let Some((def, next)) = parse_struct(&toks, j, a)? {
                out.push(CItem::Struct(def));
                i = next;
                continue;
            }
        }
        i = j.max(i + 1);
    }
    Ok(out)
}
