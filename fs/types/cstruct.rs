// SPDX-License-Identifier: GPL-2.0
//
// Parsing the C-shared items of Rust source, and writing their C - shared by
// the two halves of CStruct (see lib.rs), which must agree on every one:
//   - the macros (fs/cstruct-macros), which parse their input when the fs
//     crate compiles, and record it;
//   - rust_types_gen (main.rs), which parses the recorded text again and
//     writes the C.
//
// Both include!() this file, so it's plain `//` comments, and both are built
// on syn - the kernel build's, vendor/kernel-rust - with a bare rustc.
//
// The macros get their input as a TokenStream and record its to_string(), so
// an item is parsed by the same code from the same text both times.
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
//   #[c_lock("u8 lock")] on a Locked<R, G, P> (locking/lock.rs): C gets the
//   lock's declaration, then G and P - CStructs of the same file, unless ()
//   - each an anonymous member, as Locked lays them out. The record carries
//   R's size and Locked's offsets of G and P, so the lock and their fields are
//   asserted where Rust has them, as the struct's own.
//
//   #[c_name(NAME)] on the struct: C's name for it, where Rust's differs - a
//   family's C definition, X__head, whose name the family's type X<S> has in
//   Rust (util/vstructs.rs). C knows it only as NAME.
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

use quote::ToTokens as _;

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
    /// The C preprocessor condition: a CONFIG_ name is IS_ENABLED(NAME) -
    /// what rustc_cfg's cfg(CONFIG_X) is, true for =m too, which defines only
    /// CONFIG_X_MODULE - and any other name defined(NAME); target_endian is
    /// __BYTE_ORDER__, target_pointer_width __BITS_PER_LONG.
    #[allow(dead_code)]
    pub fn to_c(&self) -> Result<String, String> {
        Ok(match self {
            Cfg::Name(n) if n.starts_with("CONFIG_") => format!("IS_ENABLED({n})"),
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
/// c_packed, #[c_packed]: C's is __packed though Rust's isn't. Every field sits
/// where C's packing puts it - at its own alignment, as the layout asserts
/// check; a field C packs off its alignment is one of zerocopy's le:: types,
/// aligned to 1. For __packed __aligned(N), which Rust's repr can't say:
/// align(N) and #[c_packed].
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

/// A type parameter of a CStruct: its bounds and default, as written.
#[derive(Debug)]
pub struct TypeParam {
    pub name:    String,
    pub bounds:  String,
    pub default: String,
}

/// A #[derive(CStruct)] struct or union: its name and fields, in order. Its
/// repr may be per configuration: cfg_attr(PRED, repr(...)).
#[derive(Debug)]
pub struct CStructDef {
    /// C's name - its Rust one too, but for #[c_name]: rust_name.
    pub name:      String,
    /// Its Rust name, where C's differs - #[c_name(NAME)]: a family's C
    /// definition, X__head, whose own name the family's type X<S> takes (see
    /// util/vstructs.rs). rust_ident() is the name to impl and lay out by.
    pub rust_name: Option<String>,
    /// Its lifetime parameters, as written.
    pub lifetimes: Vec<String>,
    /// Its type parameters, each with a default, which is what C's one
    /// struct is: one layout for every T.
    pub type_params: Vec<TypeParam>,
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
    /// C's alignment as C spells it - SMP_CACHE_BYTES, sizeof(long) - from
    /// an alignment marker leading the struct: Rust's repr(align) takes only
    /// a literal. The generated header asserts the two agree.
    pub c_align:   Option<String>,
}

impl CStructDef {
    /// Its Rust name: C's, but for #[c_name].
    pub fn rust_ident(&self) -> &str {
        self.rust_name.as_deref().unwrap_or(&self.name)
    }

    /// X, if it's a family's definition, X__whole or X__head
    /// (util/vstructs.rs): the type code names.
    pub fn family(&self) -> Option<&str> {
        let rust = self.rust_ident();
        rust.strip_suffix("__whole").or_else(|| rust.strip_suffix("__head"))
    }

    /// Its Rust type, its lifetimes elided - X<'_> - as an expression, or an
    /// impl that doesn't name them, has it.
    #[allow(dead_code)]
    pub fn rust_type(&self) -> String {
        self.rust_type_with(&vec!["'_".to_string(); self.lifetimes.len()])
    }

    /// 'a, T: Bound and X<'a, T>: for an impl over all of the struct's
    /// generics.
    pub fn impl_generics(&self) -> (String, String) {
        let params: Vec<String> = self.lifetimes.iter().cloned()
            .chain(self.type_params.iter().map(|p| if p.bounds.is_empty() { p.name.clone() } else { format!("{}: {}", p.name, p.bounds) }))
            .collect();
        let args: Vec<String> = self.lifetimes.iter().cloned()
            .chain(self.type_params.iter().map(|p| p.name.clone()))
            .collect();
        (params.join(", "), self.rust_type_with(&args))
    }

    fn rust_type_with(&self, args: &[String]) -> String {
        match args {
            [] => self.rust_ident().to_string(),
            l  => format!("{}<{}>", self.rust_ident(), l.join(", ")),
        }
    }

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

/// A bitfield-struct type (#[bitfield(uN)]): its storage, C's declared type of
/// its integer members (ty), and its members least significant first - (name,
/// Rust type, width). A member as wide as its C type is a plain member in C;
/// narrower, a bitfield of that unit.
#[derive(Debug)]
#[allow(dead_code)] // cfg and storage: read by rust_types_gen, not the macros
pub struct CBitfieldDef {
    pub name:    String,
    pub cfg:     Option<Cfg>,
    pub storage: String,
    pub ty:      String,
    pub fields:  Vec<(String, String, u32)>,
}

impl CBitfieldDef {
    /// The type C declares a member of Rust type @rust_ty: an integer, the
    /// bitfield's ty - C lays a bitfield out by its members' declared type,
    /// Rust reads each at its width rounded up; bool and enums, themselves.
    #[allow(dead_code)] // rust_types_gen's
    pub fn c_type<'a>(&'a self, rust_ty: &'a str) -> &'a str {
        let t = rust_ty.trim();
        if t.starts_with('u') && int_bits(t).is_some() { &self.ty } else { rust_ty }
    }
}

/// One field: its name (as C has it - type, not type_), its Rust type as written, its C
/// declaration if C sees it, and whether that declaration names it.
#[derive(Debug)]
pub struct CFieldDef {
    pub name:     String,
    pub ty:       String,
    /// Its visibility, as written: `pub`, `pub(crate)`, or empty.
    pub vis:      String,
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
    /// #[c_lock("...")], on a Locked<R, G, P>: the lock's C declaration, which
    /// G's and P's members follow, written in its place.
    pub lock:     Option<String>,
    /// #[c_bitfield]: its type is a bitfield-struct type, whose members C
    /// declares in its place.
    pub bitfield: bool,
    /// #[bits(N)], in a bitfield-struct type: its width.
    pub bits:     Option<u32>,
    /// #[counted_by(FIELD)], on a trailing `[T; 0]`: the field its length
    /// is - C's __counted_by(), as the struct's FlexArray impl.
    pub counted_by: Option<String>,
    /// #[placed_at]: the field #[placed] added, its place's At - which
    /// #[derive(Init)]'s init() takes. (cstruct-macros' place.rs.)
    #[allow(dead_code)] // read by the macros, not rust_types_gen
    pub placed_at: bool,
    pub cfg:      Option<Cfg>,
    /// C's __aligned() on the member: from an alignment marker before it.
    #[allow(dead_code)]
    pub c_align:  Option<String>,
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
    /// The arguments MAP gives x: params, and concat(A, b) pastes.
    pub template: String,
}

/// Bitfields in one flags word: @macro_name is LE64_BITMASK, LE32_BITMASK,
/// LE16_BITMASK or BITMASK; @c_type the C struct type, "struct foo"; @field
/// the flags word, as C and Rust both write it: "flags[0]".
#[derive(Debug)]
pub struct CBitmask {
    pub cfg:        Option<Cfg>,
    pub macro_name: String,
    /// The struct as written, "struct foo": Rust's name, which the header
    /// generator renders as C's.
    pub c_type:     String,
    /// The type the accessors are on: "foo" - or for `struct Head<foo>`, a
    /// family's C definition, "foo__head".
    pub rust_type:  String,
    /// C's struct, as Rust names its definition: foo, foo__head, or for
    /// `struct foo<STATE>`, foo__whole - the name the generator resolves, by
    /// the record's path to it: another type may be foo too.
    pub def:        String,
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

    /// The attribute that says it: #[closed], #[open], #[flags].
    pub fn attr(self) -> &'static str {
        match self {
            CEnumKind::Closed => "closed",
            CEnumKind::Open   => "open",
            CEnumKind::Flags  => "flags",
        }
    }
}

/// Items from an x-macro list: `LIST(params) => template`. Each entry's
/// arguments are bound to the params - `..` the rest - and `concat(A, b)` in the
/// template is pasted into one identifier.
#[derive(Debug, Clone)]
pub struct CFromList {
    pub list:     String,
    pub params:   Vec<String>,
    pub template: String,
}

/// c_enum!: an enum, and what its values can be. @name is None for an
/// anonymous one: constants.
#[derive(Debug)]
pub struct CEnumDef {
    pub cfg:      Option<Cfg>,
    pub kind:     CEnumKind,
    pub name:     Option<String>,
    /// The integer type: u32, i32, u64.
    pub repr:     String,
    pub variants: Vec<CEnumVariant>,
    /// `LIST(params) => VARIANT [= value]`: variants from lists, each in
    /// place before the written one at its index - expand_enum_list() makes
    /// them, a list at a time.
    pub from:     Vec<(usize, CFromList)>,
    /// #[own_to_text]: it prints as more than its name - an open or flags
    /// enum c_enum! otherwise gives ToText.
    pub own_to_text: bool,
    /// #[c_name(NAME)]: C's name for it, where Rust's differs.
    pub c_name:   Option<String>,
}

impl CEnumDef {
    /// Its name as C has it: C's, or for #[c_name], that.
    pub fn c_ident(&self) -> Option<&str> {
        self.c_name.as_deref().or(self.name.as_deref())
    }
}

/// A c_enum! variant. Its value is Rust source, or none for the one after the
/// last; the generator replaces them with what they evaluate to. Its label is
/// what it prints as, `#[label("..")]`: a list's variant's is the argument its
/// template pastes into the name - `stripe`, of `concat(BCH_WATERMARK_, name)` - as
/// C's string tables have it; none, its name.
#[derive(Debug)]
pub struct CEnumVariant {
    pub name:  String,
    pub value: Option<String>,
    pub label: Option<String>,
}

/// c_typedef!: a type alias, C's typedef. @c_decl is its C declarator - the
/// declaration less `typedef` - where the canonical rendering of @ty isn't.
#[derive(Debug)]
pub struct CTypedef {
    pub cfg:       Option<Cfg>,
    /// C's name - its Rust one too, but for #[c_name]: rust_name.
    pub name:      String,
    pub rust_name: Option<String>,
    pub ty:        String,
    pub c_decl:    Option<String>,
}

impl CTypedef {
    #[allow(dead_code)]
    pub fn rust_ident(&self) -> &str {
        self.rust_name.as_deref().unwrap_or(&self.name)
    }
}

/// c_rcu_array!: an RcuArray<@elem[, @head]> C reads, as `struct @c_name {
/// struct rcu_head rcu; size_t @nr; [@head's fields;] @elem @elems[]; }` - a
/// field of type @alias is, to C, a __rcu pointer to one.
#[derive(Debug)]
pub struct CRcuArray {
    pub cfg:    Option<Cfg>,
    pub alias:  String,
    pub ty:     String,
    pub elem:   String,
    pub head:   Option<String>,
    pub c_name: String,
    pub nr:     String,
    pub elems:  String,
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
    /// The #define's value: the number rustc evaluated, written so C gives
    /// it c_type: `5U`, `5ULL`, `((size_t) 5)`. A
    /// constant's type is what C does with it - %zu, integer promotion,
    /// signed against unsigned - and a bare number would be int, or
    /// whatever's wide enough.
    fn c_value(&self) -> Result<String, String> {
        let v = self.value.parse::<i128>()
            .map_err(|_| format!("{}: value {} isn't a number - the records path evaluates every constant",
                                 self.name, self.value))?;
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
#[allow(dead_code)]
fn rust_int_signed(ty: &str) -> bool {
    let last = ty.rsplit("::").next().unwrap_or(ty).trim();
    last.starts_with('i') || matches!(last, "c_schar" | "c_short" | "c_int" | "c_long" | "c_longlong")
}

/// A C function Rust calls, from c_extern!. #[must_use] is C's
/// __must_check, #[cold] its __cold, and a return type of ! its __noreturn;
/// #[c_printf(FMT, ARGS)], C's alone, is its __printf(FMT, ARGS).
#[derive(Debug)]
pub struct CFnDecl {
    pub cfg:      Option<Cfg>,
    pub name:     String,
    /// Each parameter's name and Rust type.
    pub params:   Vec<(String, String)>,
    pub variadic: bool,
    pub ret:      Option<String>,
    pub must_use: bool,
    pub cold:     bool,
    pub printf:   Option<String>,
}

/// A C variable Rust uses, from c_extern!. One that isn't `mut` is C's
/// const; #[c("...")] is its C declaration where the Rust type doesn't say
/// it.
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
/// conflicting-types error in the C build. And a #[c_export] fn: what C calls
/// of Rust - its prototype, Rust's side the definition.
#[derive(Debug)]
pub struct CExtern {
    pub cfg:     Option<Cfg>,
    pub fns:     Vec<CFnDecl>,
    pub statics: Vec<CStaticDecl>,
    /// A #[c_export] fn's: a run of them is one block of prototypes.
    pub export:  bool,
}

/// @item onto @items, where a #[c_export] after another joins its block: a
/// run of them is one block of prototypes, as a c_extern!'s is.
#[allow(dead_code)] // rust_types_gen's
pub fn push_item(items: &mut Vec<CItem>, mut item: CItem) {
    if let (CItem::Extern(ex), Some(CItem::Extern(prev))) = (&mut item, items.last_mut()) {
        if ex.export && prev.export {
            prev.fns.append(&mut ex.fns);
            prev.statics.append(&mut ex.statics);
            return;
        }
    }
    items.push(item);
}

/// One arm of a tagged_union!: its name, payload type, and tag value.
#[derive(Debug, Clone)]
pub struct CTaggedArm {
    pub name:  String,
    pub ty:    String,
    pub value: String,
}

/// tagged_union!: a tagged union with a stable representation - what the
/// enum form (cstruct-macros' tagged_union.rs) is parsed into, and its
/// record, which is this struct|union form:
///
///   pub struct DiskAccountingPos {
///       tag type_: u8 = c::disk_accounting_type,
///       arm nr_inodes: NrInodes = 0, ...
///       pad: Bpos,
///       size: pad,
///       packed,
///   }
///
/// The storage is what C has: the tag, then a union of the arms, overlaid
/// with the pad, if any. The arms are `arm name: Type = value`. Their types
/// can be defined in the block too,
/// `[#[...]] [pub] struct|union NAME { ... }`: they go to nestify::nest!,
/// with the declaration's `#[...]*`.
///
/// `size: pad` says what its size is a function of: nothing, a constant,
/// the pad's. Or it's the header's count, `size: header` - the storage is
/// another's, `union NAME in STORAGE { .., size: header }`, a header its
/// arms embed and run past - and what's generated is only the typed view of
/// it; its arms' types are another's too. Or it's the tag's, `size: arm`,
/// `union NAME in HEAD { .., size: arm }`: each value as long as the arm its
/// tag selects, the head the start of every arm.
///
/// For a struct C hands over in memory rather than one read off disk -
/// reconcile_scan, say - two more, both `size: pad`'s: `tag NAME: enum TYPE`,
/// the tag a closed C enum, stored as it, the arms' values its variants;
/// and `plain`, every arm plain data, an arm type not defined here a type of
/// its own's - a typedef of an integer - rather than a #[counted] family's
/// head.
#[derive(Debug)]
pub struct CTaggedUnion {
    pub cfg:       Option<Cfg>,
    pub name:      String,
    /// #[c_name(NAME)]: C's name, where Rust's differs.
    pub c_name:    Option<String>,
    /// `union NAME in STORAGE`: unsized, over another's storage - see
    /// storage; else `struct NAME`, its own sized storage, which this
    /// defines.
    pub union:     bool,
    /// `in STORAGE`: the storage is that type, defined elsewhere, and its C
    /// too; what's generated is the typed view of it.
    pub storage:   Option<String>,
    /// The tag's name: its field, and the accessor.
    pub tag_field: String,
    pub tag_ty:    String,
    /// `= TAG_ENUM`: the tag as handed out, a newtype over an integer the
    /// tag widens to - or empty, the tag as it is.
    pub tag_enum:  String,
    /// `tag NAME: enum TYPE`: the tag is a C enum, closed - stored as it,
    /// the arms' values its variants. Only in memory: no bytes from outside,
    /// which could hold a value that isn't one.
    pub tag_is_enum: bool,
    /// `plain`: every arm is plain data - one not defined here a type of its
    /// own's, not a #[counted] family's head.
    pub plain:     bool,
    pub arms:      Vec<CTaggedArm>,
    pub pad:       Option<String>,
    /// `packed`: C's __packed - for a struct, the payload right after the
    /// tag whatever the arms' alignment, which is what the format says.
    pub packed:    bool,
    /// `size: ..`: what its size is a function of.
    pub size:      CTaggedSize,
    /// Arm types defined in the block, for nestify::nest!.
    pub defs:      Vec<CTaggedDef>,
}

/// What a tagged_union!'s size is a function of, `size: pad|header|arm`:
/// the rest follows from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CTaggedSize {
    /// Nothing - a constant, its pad's: a struct, its own storage, which
    /// holds every arm, and a value of plain data.
    Pad,
    /// Its header's count: a union over another's storage, an unsized
    /// vstruct, its arms embedding the header and running past it as far as
    /// the count says - a family (util/vstructs.rs), each state's reference
    /// to which has its arm's bytes.
    Header,
    /// Its tag: the arm the tag selects, whose size it is - a union, its
    /// storage the head every arm starts with, the tag in it; a value read
    /// off bytes and written back to them, no C of its own (ArmSized).
    Arm,
}

impl CTaggedSize {
    fn keyword(self) -> &'static str {
        match self {
            Self::Pad    => "pad",
            Self::Header => "header",
            Self::Arm    => "arm",
        }
    }
}

/// An arm's type, defined in its tagged_union!'s block.
#[derive(Debug, Clone)]
pub struct CTaggedDef {
    pub name: String,
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
    RcuArray(CRcuArray),
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

/// A token, as the text is written from: a word - an identifier, a literal,
/// a lifetime - or a punctuation character.
enum Flat {
    Word(String),
    Punct(char),
}

/// syn's tokens as the tokenizer has them: a group its delimiters, a
/// lifetime one word, a raw identifier without its r#.
fn flat(ts: &proc_macro2::TokenStream) -> Vec<Flat> {
    use proc_macro2::{Delimiter, TokenTree};
    let mut out = Vec::new();
    let mut it = ts.clone().into_iter().peekable();
    while let Some(t) = it.next() {
        match t {
            TokenTree::Ident(i) => out.push(Flat::Word(i.to_string().trim_start_matches("r#").to_string())),
            TokenTree::Literal(l) => out.push(Flat::Word(l.to_string())),
            TokenTree::Punct(p) if p.as_char() == '\'' && matches!(it.peek(), Some(TokenTree::Ident(_))) => {
                let Some(TokenTree::Ident(i)) = it.next() else { unreachable!() };
                out.push(Flat::Word(format!("'{i}")));
            }
            TokenTree::Punct(p) => out.push(Flat::Punct(p.as_char())),
            TokenTree::Group(g) => {
                let delims = match g.delimiter() {
                    Delimiter::Parenthesis => Some(('(', ')')),
                    Delimiter::Bracket     => Some(('[', ']')),
                    Delimiter::Brace       => Some(('{', '}')),
                    Delimiter::None        => None,
                };
                out.extend(delims.map(|(o, _)| Flat::Punct(o)));
                out.extend(flat(&g.stream()));
                out.extend(delims.map(|(_, c)| Flat::Punct(c)));
            }
        }
    }
    out
}

/// @t's Rust source, as tokens_to_rust() writes it.
pub fn rust_text(t: &impl quote::ToTokens) -> String {
    join_flat(&flat(&t.to_token_stream()), false)
}

/// @t as C, as tokens_to_c() writes it.
fn c_text(t: &impl quote::ToTokens) -> String {
    join_flat(&flat(&t.to_token_stream()), true)
}

/// A space only between two words: punctuation stays as written, so `<<`
/// and `->` come back whole - the tokenizer splits them. As C (@c), a space
/// after a comma and around a |, too.
fn join_flat(toks: &[Flat], c: bool) -> String {
    let word = |t: &Flat| matches!(t, Flat::Word(_));
    let punct = |t: &Flat, ch: char| matches!(t, Flat::Punct(p) if *p == ch);
    let mut out = String::new();
    for (n, t) in toks.iter().enumerate() {
        if n > 0 && (word(&toks[n - 1]) && word(t) ||
                     c && (punct(&toks[n - 1], ',') || punct(t, '|') || punct(&toks[n - 1], '|'))) {
            out.push(' ');
        }
        match t {
            Flat::Word(s) => out.push_str(s),
            Flat::Punct(ch) => out.push(*ch),
        }
    }
    out
}

// ---- attributes ------------------------------------------------------------

/// A family attribute's arguments (cstruct-macros' vstructs.rs):
/// `#[counted(PATH, tail = ..)]`, `#[unsized_vstruct(tail = ..)]`,
/// `#[grown(from = FIELD)]`; `bkey`, for a key's value.
#[derive(Debug, Default)]
pub struct FamilyArgs {
    /// The one argument that isn't `key = value`: the count, a field path.
    pub count: Option<String>,
    /// `tail = [E]` or `tail = NAME: [E]`: the tail's name, if it's named,
    /// and E.
    pub tail: Option<(Option<String>, String)>,
    /// `from = FIELD`.
    pub from: Option<String>,
    /// `bkey`: a key's value - its btree/bkey/types.rs impls too.
    pub bkey: bool,
}

/// @src, a family attribute's arguments: an error for one it doesn't take -
/// @takes, of "count", "tail" and "from" - or one not written as its kind is.
/// The caller names the attribute and the struct.
pub fn parse_family_args(src: &str, takes: &[&str]) -> Result<FamilyArgs, String> {
    use proc_macro2::{Delimiter, TokenStream, TokenTree};
    let text = |t: &[TokenTree]| rust_text(&t.iter().cloned().collect::<TokenStream>());
    let mut args = FamilyArgs::default();
    for part in split_commas(src.parse().map_err(|e| format!("({src}): {e}"))?) {
        let part: Vec<TokenTree> = part.into_iter().collect();
        let (kind, set) = match part.as_slice() {
            [] => continue,
            [TokenTree::Ident(k), TokenTree::Punct(eq), v @ ..] if eq.as_char() == '=' => {
                let bad = || format!("({}) not understood", text(&part));
                match ident(k).as_str() {
                    "tail" => {
                        let (name, slice) = match v {
                            [TokenTree::Ident(n), TokenTree::Punct(c), s @ ..] if c.as_char() == ':' => (Some(ident(n)), s),
                            s => (None, s),
                        };
                        let elem = match slice {
                            [TokenTree::Group(g)] if g.delimiter() == Delimiter::Bracket && !g.stream().is_empty() &&
                                !g.stream().into_iter().any(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ';')) =>
                                rust_text(&g.stream()),
                            _ => return Err(format!("tail = [E] - a slice - not tail = {}", text(v))),
                        };
                        ("tail", args.tail.replace((name, elem)).is_some())
                    }
                    "from" => match v {
                        [TokenTree::Ident(f)] => ("from", args.from.replace(ident(f)).is_some()),
                        _ => return Err(bad()),
                    },
                    k => return Err(format!("no argument {k}")),
                }
            }
            [TokenTree::Ident(k)] if k == "bkey" => ("bkey", core::mem::replace(&mut args.bkey, true)),
            p => ("count", args.count.replace(text(p)).is_some()),
        };
        if !takes.contains(&kind) {
            return Err(format!("doesn't take ({})", text(&part)));
        }
        if set {
            return Err(format!("{kind} given twice"));
        }
    }
    Ok(args)
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
    /// #[bitfield(.., ty = T)]: C's declared type of its integer members.
    bitfield_ty: Option<String>,
    /// #[c_typedef] or #[c_typedef(NAME)].
    typedef:   Option<Option<String>>,
    /// #[c_name(NAME)]: C's name for it, where Rust's differs.
    c_name:    Option<String>,
    /// #[c("...")] on an item: its C declaration.
    c_decl:    Option<String>,
    /// #[c_export] on a fn.
    c_export:  bool,
    /// #[must_use] on a fn.
    must_use:  bool,
    /// #[cold] on a fn.
    cold:      bool,
    /// #[c_printf(FMT, ARGS)] on a fn: its arguments.
    c_printf:  Option<String>,
}

// ---- structs ---------------------------------------------------------------

/// The struct or union @d, after its attributes @a; None if it's an enum.
fn parse_struct(d: &syn::DeriveInput, a: ItemAttrs) -> Result<Option<CStructDef>, String> {
    let name = ident(&d.ident);
    let (kind, fields) = match &d.data {
        syn::Data::Struct(s) => (CKind::Struct, match &s.fields {
            syn::Fields::Named(f) => Some(&f.named),
            _ => None,
        }),
        syn::Data::Union(u) => (CKind::Union, Some(&u.fields.named)),
        syn::Data::Enum(_) => return Ok(None),
    };
    let fields_of = fields
        .filter(|_| d.generics.type_params().all(|p| p.default.is_some()) && d.generics.const_params().next().is_none())
        .ok_or_else(|| format!("{name}: a CStruct has named fields, and no generics but lifetimes and type \
                                parameters with defaults - an unsized vstruct's states are each its \
                                own definition: #[unsized_vstruct]"))?;
    // a type parameter C has no notion of: one layout for every T
    let type_params = d.generics.type_params()
        .map(|p| TypeParam {
            name:    ident(&p.ident),
            bounds:  p.bounds.iter().map(rust_text).collect::<Vec<_>>().join(" + "),
            default: p.default.as_ref().map(rust_text).expect("checked above"),
        })
        .collect();
    // what it borrows, which C has no notion of
    let lifetimes = d.generics.lifetimes().map(|l| format!("'{}", l.lifetime.ident)).collect();

    let mut fields = Vec::new();
    for f in fields_of {
        let mut c_decl = None;
        let mut anon = false;
        let mut lock = None;
        let mut bits = None;
        let mut counted_by = None;
        let mut cfg = None;
        let bare = |n: &str| f.attrs.iter().any(|a| matches!(&a.meta, syn::Meta::Path(p) if p.is_ident(n)));
        for attr in &f.attrs {
            let is = |n: &str| attr.path().is_ident(n);
            // #[bits(N, ...)]: the width, then bitfield-struct's options -
            // from = and into = for a member's type - which C doesn't see
            if let (true, syn::Meta::List(l)) = (is("bits"), &attr.meta) {
                let width = split_commas(l.tokens.clone()).into_iter().next().unwrap_or_default();
                bits = Some(rust_text(&width).parse::<u32>()
                    .map_err(|_| format!("{name}: #[bits({})] takes a width first", rust_text(&l.tokens)))?);
            }
            for (n, is_anon) in [("c", false), ("c_anon", true)] {
                if let (true, syn::Meta::List(_)) = (is(n), &attr.meta) {
                    c_decl = Some(attr.parse_args::<syn::LitStr>()
                        .map_err(|_| format!("{name}: #[{n}(...)] takes one string, a C declaration"))?.value());
                    anon = is_anon;
                }
            }
            if is("c_lock") {
                lock = Some(attr.parse_args::<syn::LitStr>()
                    .map_err(|_| format!("{name}: #[c_lock(...)] takes one string, the lock's C declaration"))?.value());
            }
            if is("counted_by") {
                counted_by = Some(ident(&attr.parse_args::<syn::Ident>()
                    .map_err(|_| format!("{name}: #[counted_by(FIELD)] takes the field that counts it"))?));
            }
            if is("cfg") {
                cfg = Some(cfg_of(&attr.parse_args().map_err(|e| format!("{name}: cfg: {e}"))?)?);
            }
        }
        anon |= bare("c_anon");
        let (inline, group, bitfield, placed_at) =
            (bare("c_inline"), bare("c_struct_group"), bare("c_bitfield"), bare("placed_at"));
        let vis = rust_text(&f.vis);
        let field = c_ident(&ident(f.ident.as_ref().expect("named fields")));
        let ty = rust_text(&f.ty);

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
        if let Some(decl) = &lock {
            if anon || inline || group || c_decl.is_some() {
                return Err(format!("{name}.{field}: #[c_lock] is the field's C, alone"));
            }
            if lock_args(&ty)?.is_none() {
                return Err(format!("{name}.{field}: #[c_lock] is for a Locked<R, G, P>, not {ty}"));
            }
            if lock_name(decl).is_none() {
                return Err(format!("{name}.{field}: #[c_lock(\"{decl}\")] must end with the lock's name"));
            }
        }
        fields.push(CFieldDef { name: field, ty, vis, c_decl, anon, inline, group, lock, bitfield, bits,
                                counted_by, placed_at, cfg, c_align: None });
    }

    // A field of an alignment marker type is no field of C's, but its
    // __aligned(): the struct's if it comes first, the next member's if not.
    let mut c_align = None;
    for k in 0..fields.len() {
        let Some(expr) = align_marker(&fields[k].ty) else { continue };
        let f = &mut fields[k];
        if f.c_decl.is_some() || f.cfg.is_some() {
            return Err(format!("{name}.{}: an alignment marker is C's __aligned(), not a declaration \
                                of its own, and in every configuration", f.name));
        }
        f.c_decl = Some(String::new());
        f.anon = true;
        if k == 0 {
            c_align = Some(expr.to_string());
        } else if k + 1 < fields.len() {
            fields[k + 1].c_align = Some(expr.to_string());
        } else {
            return Err(format!("{name}.{}: an alignment marker aligns what follows it, and nothing does",
                               fields[k].name));
        }
    }

    let (name, rust_name) = match a.c_name {
        Some(c) => (c, Some(name)),
        None    => (name, None),
    };
    let typedef_anon = matches!(a.typedef, Some(None));
    let typedef = a.typedef.map(|t| t.unwrap_or_else(|| name.clone()));
    Ok(Some(CStructDef {
        name, rust_name, lifetimes, type_params, kind, cfg: a.cfg, repr: a.repr, cfg_reprs: a.cfg_reprs, fields, typedef,
        typedef_anon, c_align,
    }))
}

/// C's alignment, as C spells it, for a field of one of types/lib.rs's
/// alignment markers - zero-sized, aligned as C's __aligned() is.
fn align_marker(ty: &str) -> Option<&'static str> {
    match ty.trim().rsplit("::").next()? {
        "CacheAligned" => Some("SMP_CACHE_BYTES"),
        _ => None,
    }
}

/// A bitfield-struct type at @i, after its attributes - the struct with
/// #[bitfield(@storage)] - and the index after it.
fn parse_bitfield(d: &syn::DeriveInput, cfg: Option<Cfg>, storage: String, ty: Option<String>)
    -> Result<Option<CBitfieldDef>, String>
{
    let Some(def) = parse_struct(d, ItemAttrs::default())? else {
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
    let ty = ty.unwrap_or_else(|| storage.clone());
    Ok(Some(CBitfieldDef { name: def.name, cfg, storage, ty, fields }))
}

/// The width of the unit C lays out a bitfield of type @ty in: an integer's,
/// and bool's - _Bool's byte.
#[allow(dead_code)]
fn unit_bits(ty: &str) -> Option<u32> {
    if ty.trim() == "bool" { Some(8) } else { int_bits(ty) }
}

/// The width of integer type @ty, in bits.
pub fn int_bits(ty: &str) -> Option<u32> {
    Some(match ty.trim() {
        "u8" | "i8" => 8,
        "u16" | "i16" => 16,
        "u32" | "i32" => 32,
        "u64" | "i64" => 64,
        _ => return None,
    })
}

// ---- c_xmacro!, c_bitmask! -------------------------------------------------

/// c_xmacro!'s body: NAME(x) { (args), ... }
fn parse_xmacro(m: &syn::Macro) -> Result<CXMacro, String> {
    m.parse_body_with(|input: syn::parse::ParseStream<'_>| {
        input.call(syn::Attribute::parse_outer)?;
        let name: syn::Ident = input.parse()?;
        let cb;
        syn::parenthesized!(cb in input);
        let callback: syn::Ident = cb.parse()?;
        let list;
        syn::braced!(list in input);

        let mut entries = Vec::new();
        while !list.is_empty() {
            entries.push(if list.peek(syn::token::Paren) {
                let args;
                syn::parenthesized!(args in list);
                CXEntry::Args(c_text(&args.parse::<proc_macro2::TokenStream>()?))
            } else {
                // SUB(), or SUB(MAP(params) => (template))
                let sub: syn::Ident = list.parse()?;
                let inner;
                syn::parenthesized!(inner in list);
                let map = match inner.is_empty() {
                    true => None,
                    false => {
                        let map: syn::Ident = inner.parse()?;
                        let params;
                        syn::parenthesized!(params in inner);
                        let params = syn::punctuated::Punctuated::<syn::Ident, syn::Token![,]>
                            ::parse_terminated(&params)?.iter().map(ident).collect();
                        inner.parse::<syn::Token![=>]>()?;
                        let template;
                        syn::parenthesized!(template in inner);
                        let template = c_text(&template.parse::<proc_macro2::TokenStream>()?);
                        Some(CXMap { name: ident(&map), params, template })
                    }
                };
                CXEntry::Sub { list: ident(&sub), map }
            });
            if !list.is_empty() {
                list.parse::<syn::Token![,]>()?;
            }
        }
        Ok(CXMacro { cfg: None, name: ident(&name), callback: ident(&callback), entries })
    }).map_err(|e| format!("c_xmacro!: an entry is (args), SUB(), or SUB(MAP(params) => (args)) - {e}"))
}

/// c_bitmask!'s body: LE64_BITMASK(struct foo, field), strip PREFIX_ { NAME(start, end), ... }
/// @ts split at its commas - its groups' are their own.
fn split_commas(ts: proc_macro2::TokenStream) -> Vec<proc_macro2::TokenStream> {
    let mut out = vec![proc_macro2::TokenStream::new()];
    for t in ts {
        match &t {
            proc_macro2::TokenTree::Punct(p) if p.as_char() == ',' => out.push(proc_macro2::TokenStream::new()),
            _ => out.last_mut().unwrap().extend([t]),
        }
    }
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// A parse's error, as the parsers' are: a String.
fn syn_err(what: &str) -> impl Fn(syn::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

fn parse_bitmask(m: &syn::Macro) -> Result<CBitmask, String> {
    m.parse_body_with(|input: syn::parse::ParseStream<'_>| {
        input.call(syn::Attribute::parse_outer)?;
        let macro_name: syn::Ident = input.parse()?;
        if !matches!(ident(&macro_name).as_str(), "LE64_BITMASK" | "LE32_BITMASK" | "LE16_BITMASK" | "BITMASK") {
            return Err(syn::Error::new(macro_name.span(), format!("{macro_name}: not a bitmask macro")));
        }
        let args;
        syn::parenthesized!(args in input);
        args.parse::<syn::Token![struct]>()?;
        let ty: syn::Path = args.parse()?;
        args.parse::<syn::Token![,]>()?;
        let field = c_text(&args.parse::<proc_macro2::TokenStream>()?);

        // NAME, or NAME<ARG>
        let bad = || syn::Error::new_spanned(&ty, format!(
            "{macro_name}: expected a struct type - struct foo, struct Head<foo> for a family's, or struct \
             foo<STATE> for a family's type in a state"));
        let (n, arg) = match ty.segments.iter().collect::<Vec<_>>().as_slice() {
            [s] => (ident(&s.ident), match &s.arguments {
                syn::PathArguments::None => None,
                syn::PathArguments::AngleBracketed(a) => match a.args.iter().collect::<Vec<_>>().as_slice() {
                    [syn::GenericArgument::Type(syn::Type::Path(t))] => Some(t.path.get_ident().map(ident).ok_or_else(bad)?),
                    _ => return Err(bad()),
                },
                _ => return Err(bad()),
            }),
            _ => return Err(bad()),
        };
        let (rust_type, c_type, def) = match (n.as_str(), arg.as_deref()) {
            (_, None) => (n.clone(), format!("struct {n}"), n.clone()),
            // a family's C definition, X__head (util/vstructs.rs): its header's
            // fields, which each state's definition derefs to
            ("Head", Some(f)) => (format!("{f}__head"), format!("struct {f}"), format!("{f}__head")),
            // a family's type in a state: a #[grown] type's value in place, for
            // its floor's fields - every view of it reaches X<Validated>
            (_, Some(st @ ("Whole" | "Validated" | "Unvalidated"))) =>
                (format!("{n}<crate::util::vstructs::{st}>"), format!("struct {n}"), format!("{n}__whole")),
            _ => return Err(bad()),
        };

        input.parse::<Option<syn::Token![,]>>()?;
        let strip = match input.peek(syn::Ident) && input.fork().parse::<syn::Ident>()? == "strip" {
            true => {
                input.parse::<syn::Ident>()?;
                Some(ident(&input.parse()?))
            }
            false => None,
        };

        let list;
        syn::braced!(list in input);
        let mut entries = Vec::new();
        while !list.is_empty() {
            let name: syn::Ident = list.parse()?;
            let range;
            syn::parenthesized!(range in list);
            let [start, end] = split_commas(range.parse()?).try_into().map_err(|_|
                syn::Error::new(name.span(), format!("{name}: expected NAME(start, end)")))?;
            entries.push(CBitmaskEntry { name: ident(&name), start: c_text(&start), end: c_text(&end) });
            list.parse::<Option<syn::Token![,]>>()?;
        }

        Ok(CBitmask { cfg: None, macro_name: ident(&macro_name), c_type, rust_type, def, field, strip, entries })
    }).map_err(syn_err("c_bitmask!"))
}

/// c_ioctl!'s body: NAME = _IOW(type, nr, ARG), ... - _IO without the ARG -
/// with #[c("...")] before an entry for its argument's C type, where the Rust
/// type doesn't say it.
fn parse_ioctl(m: &syn::Macro) -> Result<CIoctl, String> {
    let entries = m.parse_body_with(|input: syn::parse::ParseStream<'_>| {
        let mut entries = Vec::new();
        while !input.is_empty() {
            let mut c_arg = None;
            for a in input.call(syn::Attribute::parse_outer)? {
                if a.path().is_ident("c") {
                    c_arg = Some(a.parse_args::<syn::LitStr>()?.value());
                }
            }
            let name: syn::Ident = input.parse()?;
            input.parse::<syn::Token![=]>()?;
            let dir: syn::Ident = input.parse()?;
            let args;
            syn::parenthesized!(args in input);
            let err = |e: String| syn::Error::new(name.span(), format!("{name}: {e}"));
            let args = split_commas(args.parse()?);
            let (ty, nr, arg) = match (ident(&dir).as_str(), args.as_slice()) {
                ("_IO", [ty, nr]) => (c_text(ty), c_text(nr), None),
                ("_IOR" | "_IOW" | "_IOWR", [ty, nr, arg]) => (c_text(ty), c_text(nr), Some(rust_text(arg))),
                ("_IO", _) => return Err(err("expected _IO(type, nr)".into())),
                ("_IOR" | "_IOW" | "_IOWR", _) => return Err(err(format!("expected {dir}(type, nr, ARG)"))),
                _ => return Err(err(format!("{dir} isn't _IO, _IOR, _IOW or _IOWR"))),
            };
            if arg.is_none() && c_arg.is_some() {
                return Err(err("#[c(...)] is the argument's C type, and _IO has none".into()));
            }
            entries.push(CIoctlEntry { name: ident(&name), dir: ident(&dir), ty, nr, arg, c_arg });
            input.parse::<Option<syn::Token![,]>>()?;
        }
        Ok(entries)
    }).map_err(syn_err("c_ioctl!: an entry is NAME = _IOW(type, nr, ARG)"))?;
    if entries.is_empty() {
        return Err("c_ioctl!: expected NAME = _IOW(type, nr, ARG), ...".into());
    }
    Ok(CIoctl { cfg: None, entries })
}

/// c_enum!'s body: #[open|closed|flags] pub enum NAME: REPR { A = expr, B, ... }
/// - NAME `_` for an anonymous enum.
fn parse_enum(m: &syn::Macro) -> Result<CEnumDef, String> {
    use syn::parse::{ParseStream, Parser as _};
    m.parse_body_with(|input: ParseStream<'_>| {
        let mut kind = None;
        let mut own_to_text = false;
        let mut c_name = None;
        for a in input.call(syn::Attribute::parse_outer)? {
            for (n, k) in [("open", CEnumKind::Open), ("closed", CEnumKind::Closed), ("flags", CEnumKind::Flags)] {
                if a.path().is_ident(n) {
                    kind = Some(k);
                }
            }
            own_to_text |= a.path().is_ident("own_to_text");
            if a.path().is_ident("c_name") {
                c_name = a.parse_args::<syn::Ident>().ok().map(|n| ident(&n));
            }
        }
        input.parse::<syn::Visibility>()?;
        input.parse::<syn::Token![enum]>()?;
        let name = match input.parse::<Option<syn::Token![_]>>()? {
            Some(_) => None,
            None => Some(ident(&input.parse()?)),
        };
        let what = name.clone().unwrap_or_else(|| "(anonymous)".into());
        let kind = kind.ok_or_else(|| input.error(format!("{what}: #[open], #[closed] or #[flags]")))?;
        input.parse::<syn::Token![:]>()?;
        let repr = ident(&input.parse()?);
        if int_bits(&repr).is_none() {
            return Err(input.error(format!("{what}: REPR is an integer type")));
        }
        let list;
        syn::braced!(list in input);

        let mut variants = Vec::new();
        let mut from = Vec::new();
        for v in split_commas(list.parse()?) {
            if let Some(f) = parse_from_list(&v) {
                from.push((variants.len(), f));
                continue;
            }
            variants.push((|input: ParseStream<'_>| {
                let mut label = None;
                for a in input.call(syn::Attribute::parse_outer)? {
                    if a.path().is_ident("label") {
                        label = Some(a.parse_args::<syn::LitStr>()?.value());
                    }
                }
                let vname: syn::Ident = input.parse()?;
                let value = match input.parse::<Option<syn::Token![=]>>()? {
                    Some(_) => Some(rust_text(&input.parse::<proc_macro2::TokenStream>()?)),
                    None => None,
                };
                if !input.is_empty() || value.as_deref() == Some("") {
                    return Err(input.error(format!("{what}::{vname}: expected `= value`, or nothing")));
                }
                Ok(CEnumVariant { name: ident(&vname), value, label })
            }).parse2(v)?);
        }
        Ok(CEnumDef { cfg: None, kind, name, repr, variants, from, own_to_text, c_name })
    }).map_err(syn_err("c_enum!: expected #[open|closed|flags] pub enum NAME: REPR { ... }"))
}

/// c_typedef!'s body: [#[c("C declarator")]] [#[c_name(NAME)]] pub type NAME = TYPE;
fn parse_typedef(m: &syn::Macro) -> Result<CTypedef, String> {
    let t: syn::ItemType = m.parse_body().map_err(|e| format!("c_typedef!: expected pub type NAME = TYPE; - {e}"))?;
    let mut c_decl = None;
    let mut c_name = None;
    for a in &t.attrs {
        if a.path().is_ident("c") {
            c_decl = a.parse_args::<syn::LitStr>().ok().map(|s| s.value());
        }
        if a.path().is_ident("c_name") {
            c_name = a.parse_args::<syn::Ident>().ok().map(|n| ident(&n));
        }
    }
    let (name, rust_name) = match c_name {
        Some(c) => (c, Some(ident(&t.ident))),
        None    => (ident(&t.ident), None),
    };
    Ok(CTypedef { cfg: None, name, rust_name, ty: rust_text(&t.ty), c_decl })
}

/// @i as the tokenizer has an identifier: a raw one without its r#.
fn ident(i: &syn::Ident) -> String {
    i.to_string().trim_start_matches("r#").to_string()
}

/// c_rcu_array!'s body: pub type ALIAS = RcuArray<T[, H]> as struct NAME { NR, ELEMS };
fn parse_rcu_array(m: &syn::Macro) -> Result<CRcuArray, String> {
    const EXPECTED: &str = "c_rcu_array!: expected pub type ALIAS = RcuArray<T[, H]> as struct NAME { NR, ELEMS };";
    let (alias, ty, c_name, nr, elems) = m.parse_body_with(|input: syn::parse::ParseStream<'_>| {
        input.call(syn::Attribute::parse_outer)?;
        input.parse::<syn::Visibility>()?;
        input.parse::<syn::Token![type]>()?;
        let alias: syn::Ident = input.parse()?;
        input.parse::<syn::Token![=]>()?;
        let ty: syn::Type = input.parse()?;
        input.parse::<syn::Token![as]>()?;
        input.parse::<syn::Token![struct]>()?;
        let c_name: syn::Ident = input.parse()?;
        let fields;
        syn::braced!(fields in input);
        let fields = syn::punctuated::Punctuated::<syn::Ident, syn::Token![,]>::parse_terminated(&fields)?;
        input.parse::<Option<syn::Token![;]>>()?;
        let [nr, elems] = fields.into_iter().collect::<Vec<_>>().try_into().map_err(|_| syn::Error::new(alias.span(),
            format!("{alias}: its fields are {{ NR, ELEMS }}: the length's name and the array's")))?;
        Ok((ident(&alias), rust_text(&ty), ident(&c_name), ident(&nr), ident(&elems)))
    }).map_err(syn_err(EXPECTED))?;
    let (elem, head) = match container_args(&ty, "RcuArray")?.as_deref() {
        Some([elem]) => (elem.clone(), None),
        Some([elem, head]) => (elem.clone(), Some(head.clone())),
        _ => return Err(format!("c_rcu_array! {alias}: {ty} isn't an RcuArray<T> or RcuArray<T, H>")),
    };
    Ok(CRcuArray { cfg: None, alias, ty, elem, head, c_name, nr, elems })
}

/// c_same!'s body: struct|union NAME == TYPE [{ field, ... }]
fn parse_same(m: &syn::Macro) -> Result<CSame, String> {
    m.parse_body_with(|input: syn::parse::ParseStream<'_>| {
        use syn::ext::IdentExt as _;
        input.call(syn::Attribute::parse_outer)?;
        let kw = input.call(syn::Ident::parse_any)?;
        if kw != "struct" && kw != "union" {
            return Err(syn::Error::new(kw.span(), "expected struct NAME == TYPE"));
        }
        let name: syn::Ident = input.parse()?;
        input.parse::<syn::Token![==]>()?;
        let ty: syn::Type = input.parse()?;
        let fields = match input.peek(syn::token::Brace) {
            true => {
                let list;
                syn::braced!(list in input);
                syn::punctuated::Punctuated::<syn::Ident, syn::Token![,]>::parse_terminated(&list)?
                    .iter().map(ident).collect()
            }
            false => Vec::new(),
        };
        Ok(CSame { c_type: format!("{kw} {}", ident(&name)), rust_ty: rust_text(&ty), fields })
    }).map_err(syn_err("c_same!: expected struct NAME == TYPE [{ field, .. }]"))
}

/// One of c_const!'s constants: `#[c_int]? pub const NAME: TYPE = EXPR`. The #define
/// has TYPE's C type; #[c_int], int's: for a constant C had as a plain
/// number, so its type is the one C was written against, whatever type
/// Rust wants it in.
fn parse_consts(m: &syn::Macro) -> Result<Vec<CItem>, String> {
    let consts = m.parse_body_with(|input: syn::parse::ParseStream<'_>| {
        let mut v = Vec::new();
        while !input.is_empty() {
            v.push(input.parse::<syn::ItemConst>()?);
        }
        Ok(v)
    }).map_err(|e| format!("c_const!: {e}"))?;

    consts.into_iter().map(|c| {
        let name = c.ident.to_string();
        let ty = c.ty.to_token_stream().to_string();
        let c_type = match c.attrs.iter().any(|a| a.path().is_ident("c_int")) {
            true => "int".to_string(),
            false => canon_base(&c.ty).ok_or_else(|| format!("c_const! {name}: {ty} has no C type"))?,
        };
        Ok(CItem::Const(CConst { cfg: None, name, ty, c_type, value: c.expr.to_token_stream().to_string() }))
    }).collect()
}

/// c_extern!'s body: `pub fn NAME(p: T, ...) -> R;` and `pub static [mut]
/// NAME: T;`, each with a #[cfg] if it has one.
/// cfg(@m), as Cfg.
fn cfg_of(m: &syn::Meta) -> Result<Cfg, String> {
    let bad = || format!("cfg({}) not understood", rust_text(m));
    match m {
        syn::Meta::Path(p) => p.get_ident().map(|i| Cfg::Name(ident(i))).ok_or_else(bad),
        syn::Meta::NameValue(nv) => match (nv.path.get_ident(), &nv.value) {
            (Some(k), syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(v), .. })) =>
                Ok(Cfg::KeyValue(ident(k), v.value())),
            _ => Err(bad()),
        },
        syn::Meta::List(l) => {
            let args = l.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
                .map_err(|_| bad())?.iter().map(cfg_of).collect::<Result<Vec<_>, _>>()?;
            match l.path.get_ident().map(ident).as_deref() {
                Some("not") if args.len() == 1 => Ok(Cfg::Not(Box::new(args.into_iter().next().unwrap()))),
                Some("all") => Ok(Cfg::All(args)),
                Some("any") => Ok(Cfg::Any(args)),
                _ => Err(bad()),
            }
        }
    }
}

/// repr(@l)'s C packing and alignment, into @into.
fn repr_of(l: &syn::MetaList, into: &mut CRepr) -> Result<(), String> {
    let reprs = l.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
        .map_err(|e| format!("repr({}): {e}", rust_text(&l.tokens)))?;
    for r in &reprs {
        match r {
            syn::Meta::Path(p) if p.is_ident("C") => into.repr_c = true,
            syn::Meta::Path(p) if p.is_ident("packed") => into.packed = Some("1".into()),
            syn::Meta::List(n) if n.path.is_ident("packed") => into.packed = Some(rust_text(&n.tokens)),
            syn::Meta::List(n) if n.path.is_ident("align") => into.align = Some(rust_text(&n.tokens)),
            _ => return Err(format!("repr({}) not supported", rust_text(r))),
        }
    }
    Ok(())
}

/// The attributes of an item we read, from @attrs.
fn item_attrs_of(attrs: &[syn::Attribute]) -> Result<ItemAttrs, String> {
    use proc_macro2::TokenTree;
    let mut a = ItemAttrs::default();
    for attr in attrs {
        let list = match &attr.meta {
            syn::Meta::List(l) => Some(l),
            _ => None,
        };
        let is = |n: &str| attr.path().is_ident(n);
        let bare = |n: &str| matches!(&attr.meta, syn::Meta::Path(p) if p.is_ident(n));
        if let (true, Some(l)) = (is("cfg"), list) {
            a.cfg = Some(cfg_of(&l.parse_args().map_err(|e| format!("cfg: {e}"))?)?);
        }
        if let (true, Some(l)) = (is("derive"), list) {
            a.derives |= flat(&l.tokens).iter().any(|t| matches!(t, Flat::Word(w) if w == "CStruct"));
        }
        if let (true, Some(l)) = (is("repr"), list) {
            repr_of(l, &mut a.repr)?;
        }
        if let (true, Some(l)) = (is("cfg_attr"), list) {
            // cfg_attr(PRED, repr(...)): a repr for one configuration.
            let args = l.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
                .map_err(|e| format!("cfg_attr: {e}"))?;
            if let [pred, syn::Meta::List(r)] = args.iter().collect::<Vec<_>>().as_slice() {
                if r.path.is_ident("repr") {
                    let mut repr = CRepr::default();
                    repr_of(r, &mut repr)?;
                    a.cfg_reprs.push((cfg_of(pred)?, repr));
                }
            }
        }
        if let (true, Some(l)) = (is("bitfield"), list) {
            let args = split_commas(l.tokens.clone());
            a.bitfield = args.first().map(rust_text);
            a.bitfield_ty = args.iter().find_map(|t| match t.clone().into_iter().collect::<Vec<_>>().as_slice() {
                [TokenTree::Ident(k), TokenTree::Punct(eq), ty @ ..] if k == "ty" && eq.as_char() == '=' =>
                    Some(rust_text(&ty.iter().cloned().collect::<proc_macro2::TokenStream>())),
                _ => None,
            });
        }
        a.typedef = if bare("c_typedef") { Some(None) } else { a.typedef };
        a.repr.c_packed |= bare("c_packed");
        a.c_export |= bare("c_export");
        a.must_use |= is("must_use");
        a.cold |= bare("cold");
        if let (true, Some(l)) = (is("c_printf"), list) {
            a.c_printf = Some(c_text(&l.tokens));
        }
        if let (true, Ok(s)) = (is("c"), attr.parse_args::<syn::LitStr>()) {
            a.c_decl = Some(s.value());
        }
        if let (true, Ok(t)) = (is("c_typedef"), attr.parse_args::<syn::Ident>()) {
            a.typedef = Some(Some(ident(&t)));
        }
        // #[c_name], or what it expands to - a family's macro sees it
        // unexpanded, the derives expanded
        if let (true, Ok(n)) = (is("c_name") || is("c_name_of"), attr.parse_args::<syn::Ident>()) {
            a.c_name = Some(ident(&n));
        }
    }
    for (_, r) in &mut a.cfg_reprs {
        r.c_packed = a.repr.c_packed;
    }
    Ok(a)
}

/// A fn's parameters - name and @ty's of the type, each - and what follows
/// them, @ty of its return type: from @what's signature.
type Params = Vec<(String, String)>;
fn fn_params(what: &str, sig: &syn::Signature, ty: &mut dyn FnMut(&syn::Type) -> Result<String, String>)
    -> Result<(Params, Option<String>), String>
{
    let name = ident(&sig.ident);
    let mut params = Vec::new();
    for p in &sig.inputs {
        let syn::FnArg::Typed(t) = p else {
            return Err(format!("{what}: fn {name}: parameter {} not understood", rust_text(p)));
        };
        let n = match &*t.pat {
            syn::Pat::Ident(i) if i.by_ref.is_none() && i.subpat.is_none() => ident(&i.ident),
            syn::Pat::Wild(_) => "_".into(),
            _ => return Err(format!("{what}: fn {name}: parameter {} not understood", rust_text(p))),
        };
        params.push((n, ty(&t.ty)?));
    }
    let ret = match &sig.output {
        syn::ReturnType::Default => None,
        syn::ReturnType::Type(_, t) => Some(ty(t)?),
    };
    Ok((params, ret))
}

fn parse_extern(m: &syn::Macro) -> Result<CExtern, String> {
    let items = m.parse_body_with(|input: syn::parse::ParseStream<'_>| {
        let mut v = Vec::new();
        while !input.is_empty() {
            v.push(input.parse::<syn::ForeignItem>()?);
        }
        Ok(v)
    }).map_err(syn_err("c_extern!: an item is a fn or a static, and its ';'"))?;

    let mut ex = CExtern { cfg: None, fns: Vec::new(), statics: Vec::new(), export: false };
    for item in &items {
        match item {
            syn::ForeignItem::Fn(f) => {
                let a = item_attrs_of(&f.attrs)?;
                let (params, ret) = fn_params("c_extern!", &f.sig, &mut |t| Ok(rust_text(t)))?;
                ex.fns.push(CFnDecl { cfg: a.cfg, name: ident(&f.sig.ident), params, variadic: f.sig.variadic.is_some(),
                                      ret, must_use: a.must_use, cold: a.cold, printf: a.c_printf });
            }
            syn::ForeignItem::Static(s) => {
                let a = item_attrs_of(&s.attrs)?;
                ex.statics.push(CStaticDecl { cfg: a.cfg, name: ident(&s.ident), ty: rust_text(&s.ty),
                                              mutable: matches!(s.mutability, syn::StaticMutability::Mut(_)),
                                              c_decl: a.c_decl });
            }
            _ => return Err(format!("c_extern!: {} isn't a fn or a static", rust_text(item))),
        }
    }
    Ok(ex)
}

/// Rust's types for C's that a #[c_export] fn may take, as unqualified() has
/// them, and C's, as c_extern! does: #[repr(transparent)], or c_same! - one
/// layout either way. References are c_export_type()'s.
const C_EXPORT_TYPES: &[(&str, &str)] = &[
    ("BkeySC<'_>",     "c::bkey_s_c"),
    ("BkeyS<'_>",      "c::bkey_s"),
    ("BtreeIter<'_>",  "c::btree_iter"),
];

/// Of C_EXPORT_TYPES, those C changes behind a shared reference - an Opaque,
/// or Cells: their pointer isn't const.
const C_EXPORT_OPAQUE: &[&str] = &["BtreeTrans<'_>"];

/// @ty with each path's qualifiers dropped: `&crate::util::ffi::Opaque<crate::fs::Fs>`
/// is `&Opaque<Fs>`.
fn unqualified(ty: &syn::Type) -> String {
    struct Last;
    impl syn::visit_mut::VisitMut for Last {
        fn visit_path_mut(&mut self, p: &mut syn::Path) {
            if let Some(last) = p.segments.pop() {
                p.segments = [last.into_value()].into_iter().collect();
            }
            p.leading_colon = None;
            syn::visit_mut::visit_path_mut(self, p);
        }
    }
    let mut ty = ty.clone();
    syn::visit_mut::VisitMut::visit_type_mut(&mut Last, &mut ty);
    rust_text(&ty)
}

/// The C type, as c_extern! has it, of a #[c_export] fn's parameter or return
/// type @ty - and whether it isn't @ty, the value converting by transmute. A
/// reference is C's pointer: &T const T *, &mut T T *, and Option<&T> one
/// that may be NULL; Opaque<T> and MaybeUninit<T> are T, and an Opaque's
/// pointer isn't const, as C changes what's behind it. C_EXPORT_TYPES' are
/// as it has them, and a closure fn's Running<T> is what C calls it with, a
/// struct work_struct *. Any other type is C's own: a path to it written
/// c::NAME, as `crate::c::foo` from a macro_rules! isn't.
fn c_export_type(ty: &syn::Type) -> (String, bool) {
    let is = |r: &str, t: &syn::Type| type_of(r).is_some_and(|r| unqualified(&r) == unqualified(t));
    if let Some(r @ syn::Type::Reference(_)) = arg_of(ty, "Option") {
        return c_export_type(r);
    }
    if arg_of(ty, "Running").is_some() {
        return ("*mut c::work_struct".into(), true);
    }
    if let syn::Type::Reference(r) = ty {
        let (mut mutable, mut pointee) = (r.mutability.is_some(), &*r.elem);
        if let Some(t) = arg_of(pointee, "Opaque") {
            (mutable, pointee) = (true, t);
        } else if let Some(t) = arg_of(pointee, "MaybeUninit") {
            pointee = t;
        } else if C_EXPORT_OPAQUE.iter().any(|r| is(r, pointee)) {
            mutable = true;
        }
        let q = if mutable { "mut" } else { "const" };
        return (format!("*{q} {}", c_export_type(pointee).0), true);
    }
    if let Some((_, c)) = C_EXPORT_TYPES.iter().find(|(r, _)| is(r, ty)) {
        return (c.to_string(), true);
    }
    // the path's qualifiers before c::, as c::
    struct CNs;
    impl syn::visit_mut::VisitMut for CNs {
        fn visit_path_mut(&mut self, p: &mut syn::Path) {
            if let Some(ns) = p.segments.iter().rposition(|s| is_c_ns(&ident(&s.ident))) {
                let rest: Vec<_> = p.segments.iter().skip(ns + 1).cloned().collect();
                p.segments = std::iter::once(syn::PathSegment::from(syn::Ident::new("c", proc_macro2::Span::call_site())))
                    .chain(rest).collect();
                p.leading_colon = None;
            }
            syn::visit_mut::visit_path_mut(self, p);
        }
    }
    let mut ty = ty.clone();
    syn::visit_mut::VisitMut::visit_type_mut(&mut CNs, &mut ty);
    (rust_text(&ty), false)
}

/// A #[c_export] fn: its prototype, as c_extern!'s, with each type C's
/// (c_export_type()); and for the definition with C's signature that calls
/// it, each of its types that isn't C's - its parameters', its return's.
/// An extern "C" fn is C's symbol itself, and has no such definition.
pub struct CExport {
    pub ex:       CExtern,
    pub extern_c: bool,
    pub params:   Vec<Option<String>>,
    pub ret:      Option<String>,
}

/// A #[c_export] fn, at @i, past its attributes: `[pub] [unsafe] [extern
/// "C"] fn NAME[<'a, ...>](p: T, ...) [-> R] { ... }`; and the index after it.
fn parse_export(f: &syn::ItemFn) -> Result<CExport, String> {
    use syn::visit_mut::VisitMut;
    let a = item_attrs_of(&f.attrs)?;
    let name = ident(&f.sig.ident);
    let extern_c = matches!(&f.sig.abi, Some(syn::Abi { name: Some(n), .. }) if n.value() == "C");

    // Its lifetimes are '_ to the definition with C's signature, which calls
    // it: the call infers them
    let mut lifetimes = Vec::new();
    for g in &f.sig.generics.params {
        match g {
            syn::GenericParam::Lifetime(l) => lifetimes.push(l.lifetime.ident.to_string()),
            _ => return Err(format!("#[c_export] fn {name}: generics are lifetimes only")),
        }
    }
    struct Elide<'a>(&'a [String]);
    impl VisitMut for Elide<'_> {
        fn visit_lifetime_mut(&mut self, l: &mut syn::Lifetime) {
            if self.0.contains(&l.ident.to_string()) {
                *l = syn::Lifetime::new("'_", l.span());
            }
        }
    }

    // each type as C has it, and as Rust has it if it converts
    let mut rust = Vec::new();
    let (params, ret) = fn_params("#[c_export]", &f.sig, &mut |t| {
        let mut t = t.clone();
        Elide(&lifetimes).visit_type_mut(&mut t);
        let (c, converts) = c_export_type(&t);
        rust.push(converts.then(|| rust_text(&t)));
        Ok(c)
    })?;
    let rust_ret = match ret {
        Some(_) => rust.pop().flatten(),
        None => None,
    };
    let f = CFnDecl { cfg: a.cfg, name, params, variadic: false, ret, must_use: a.must_use,
                      cold: a.cold, printf: a.c_printf };
    let ex = CExtern { cfg: None, fns: vec![f], statics: Vec::new(), export: true };
    Ok(CExport { ex, extern_c, params: rust, ret: rust_ret })
}


/// A #[c_export] static, at @i, past `static`: `NAME: TYPE [= EXPR];` - C
/// gets its extern declaration, as a c_extern! static's; and the index after
/// it. Not mut: what C reads of Rust's is C's const.
fn parse_export_static(s: &syn::ForeignItemStatic) -> Result<CExport, String> {
    if !matches!(s.mutability, syn::StaticMutability::None) {
        return Err("#[c_export]: expected static NAME: TYPE = EXPR; - not mut".into());
    }
    let a = item_attrs_of(&s.attrs)?;
    let s = CStaticDecl { cfg: a.cfg, name: ident(&s.ident), ty: rust_text(&s.ty), mutable: false, c_decl: a.c_decl };
    let ex = CExtern { cfg: None, fns: Vec::new(), statics: vec![s], export: true };
    Ok(CExport { ex, extern_c: false, params: Vec::new(), ret: None })
}

/// An exported item - a fn, or a static, whose record has no value: `static
/// NAME: TYPE;`, which syn has as Verbatim - if @item is one.
fn export_of(item: &syn::Item) -> Option<Result<CExport, String>> {
    match item {
        syn::Item::Fn(f) => Some(parse_export(f)),
        syn::Item::Verbatim(v) => syn::parse2::<syn::ForeignItemStatic>(v.clone()).ok().map(|s| parse_export_static(&s)),
        _ => None,
    }
}

/// Whether @item is #[c_export]'s.
fn is_export(item: &syn::Item) -> bool {
    let attrs = match item {
        syn::Item::Fn(f) => f.attrs.clone(),
        syn::Item::Verbatim(v) => match syn::parse2::<syn::ForeignItemStatic>(v.clone()) {
            Ok(s) => s.attrs,
            Err(_) => return false,
        },
        _ => return false,
    };
    attrs.iter().any(|a| matches!(&a.meta, syn::Meta::Path(p) if p.is_ident("c_export")))
}

/// #[c_export]'s text, as its macro records it - checked that C can have
/// the prototype. The macro doesn't see the struct map: a Rust-named type
/// (CamelCase) is taken as a struct here, and rust_types_gen, which has the
/// map, renders it or fails.
#[allow(dead_code)]
pub fn parse_c_export(text: &str) -> Result<CExport, String> {
    let expected = "#[c_export]: expected [pub] [unsafe] [extern \"C\"] fn NAME[<'a, ...>](..), or static";
    let item = syn::parse_str::<syn::Item>(text).map_err(|e| format!("{expected} - {e}"))?;
    let e = export_of(&item).ok_or(expected)??;
    emit_extern(&e.ex, &|n| n.starts_with(char::is_uppercase).then(|| format!("struct {n}")))?;
    Ok(e)
}

/// tagged_union!'s body: `[vis] struct NAME { tag NAME: TYPE [= TAG_ENUM], arms
/// from LIST(params) => template | arm NAME: TYPE = VALUE, ..., pad: TYPE,
/// size: pad, [packed] }`, or `[vis] union NAME in STORAGE { ..., size: header|arm }`.
fn parse_tagged_union(m: &syn::Macro) -> Result<CTaggedUnion, String> {
    use proc_macro2::{Delimiter, TokenStream, TokenTree};
    let body: Vec<TokenTree> = m.tokens.clone().into_iter().collect();
    let text = |t: &[TokenTree]| rust_text(&t.iter().cloned().collect::<TokenStream>());
    let is_ident = |t: Option<&TokenTree>, s: &str| matches!(t, Some(TokenTree::Ident(i)) if i == s);
    let is_punct = |t: Option<&TokenTree>, c: char| matches!(t, Some(TokenTree::Punct(p)) if p.as_char() == c);
    let is_group = |t: Option<&TokenTree>, d: Delimiter| matches!(t, Some(TokenTree::Group(g)) if g.delimiter() == d);
    // past its #[..]s - @attr each one's - and its pub [(..)]
    let skip_attrs_vis = |t: &[TokenTree], attr: &mut dyn FnMut(&proc_macro2::Group, bool)| -> usize {
        let mut i = 0;
        while is_punct(t.get(i), '#') && is_group(t.get(i + 1), Delimiter::Bracket) {
            let TokenTree::Group(g) = &t[i + 1] else { unreachable!() };
            let star = is_punct(t.get(i + 2), '*');
            attr(g, star);
            i += 2 + usize::from(star);
        }
        if is_ident(t.get(i), "pub") {
            i += 1 + usize::from(is_group(t.get(i + 1), Delimiter::Parenthesis));
        }
        i
    };

    // `#[...]*` are for the arm types defined here - nestify propagates them -
    // not the storage: skipped
    let mut c_name = None;
    let i = skip_attrs_vis(&body, &mut |g, star| {
        match syn::parse2::<syn::Meta>(g.stream()) {
            Ok(syn::Meta::List(l)) if !star && l.path.is_ident("c_name") =>
                if let Ok(n) = syn::parse2::<syn::Ident>(l.tokens) {
                    c_name = Some(ident(&n));
                },
            _ => {}
        }
    });
    const EXPECTED: &str = "tagged_union!: expected struct|union NAME [in STORAGE] { ... }";
    let (union, name, rest) = match &body[i..] {
        [TokenTree::Ident(s), TokenTree::Ident(name), rest @ ..] if s == "struct" || s == "union" =>
            (s == "union", ident(name), rest),
        _ => return Err(EXPECTED.into()),
    };
    let (inner, storage) = match rest {
        [TokenTree::Group(g)] if g.delimiter() == Delimiter::Brace => (g.stream(), None),
        [TokenTree::Ident(kw), path @ .., TokenTree::Group(g)]
            if kw == "in" && !path.is_empty() && g.delimiter() == Delimiter::Brace => (g.stream(), Some(text(path))),
        _ => return Err(EXPECTED.into()),
    };
    let mut size = None;
    let mut tu = CTaggedUnion { cfg: None, name, c_name, union, storage, tag_field: String::new(),
                                tag_ty: String::new(), tag_enum: String::new(), tag_is_enum: false,
                                plain: false,
                                arms: Vec::new(), pad: None, packed: false,
                                size: CTaggedSize::Pad, defs: Vec::new() };
    for item in split_commas(inner) {
        let item: Vec<TokenTree> = item.into_iter().collect();
        let eq = item.iter().rposition(|t| is_punct(Some(t), '='));
        // an arm type defined here: [#[..]] [pub] struct|union NAME { .. }
        let j = skip_attrs_vis(&item, &mut |_, _| {});
        match item.get(j..j + 3) {
            Some([TokenTree::Ident(kw), TokenTree::Ident(n), TokenTree::Group(g)])
                if (kw == "struct" || kw == "union") && g.delimiter() == Delimiter::Brace => {
                tu.defs.push(CTaggedDef { name: ident(n) });
                continue;
            }
            _ => {}
        }
        let colon = |t: &TokenTree| is_punct(Some(t), ':');
        match item.as_slice() {
            [] => {}
            [TokenTree::Ident(kw), TokenTree::Ident(field), c, rest @ ..] if kw == "tag" && colon(c) => {
                let rest = match rest {
                    [TokenTree::Ident(e), rest @ ..] if e == "enum" => {
                        tu.tag_is_enum = true;
                        rest
                    }
                    rest => rest,
                };
                let eq = rest.iter().position(|t| is_punct(Some(t), '=')).unwrap_or(rest.len());
                tu.tag_field = ident(field);
                tu.tag_ty = text(&rest[..eq]);
                tu.tag_enum = text(rest.get(eq + 1..).unwrap_or_default());
                if tu.tag_ty.is_empty() || (eq < rest.len() && tu.tag_enum.is_empty()) ||
                   (tu.tag_is_enum && eq < rest.len()) {
                    return Err("tagged_union!: tag NAME: TYPE [= TAG_ENUM], or tag NAME: enum TYPE".into());
                }
            }
            [TokenTree::Ident(kw), TokenTree::Ident(arm), c, ..] if kw == "arm" && colon(c) => {
                let eq = eq.ok_or("tagged_union!: arm NAME: TYPE = VALUE")?;
                tu.arms.push(CTaggedArm { name: ident(arm), ty: text(&item[3..eq]), value: text(&item[eq + 1..]) });
            }
            [TokenTree::Ident(kw), c, ty @ ..] if kw == "pad" && colon(c) && !ty.is_empty() =>
                tu.pad = Some(text(ty)),
            [TokenTree::Ident(kw), c, TokenTree::Ident(of)] if kw == "size" && colon(c) => {
                size = Some(match ident(of).as_str() {
                    "pad"    => CTaggedSize::Pad,
                    "header" => CTaggedSize::Header,
                    "arm"    => CTaggedSize::Arm,
                    _ => return Err(format!("tagged_union!: size: {of} - its size is a function \
                                             of nothing, its pad's, `size: pad`; of its \
                                             header's count, `size: header`; or of its tag, \
                                             its arm's, `size: arm`")),
                });
            }
            [TokenTree::Ident(kw)] if kw == "packed" => tu.packed = true,
            [TokenTree::Ident(kw)] if kw == "plain" => tu.plain = true,
            _ => return Err(format!("tagged_union!: {} not understood", text(&item))),
        }
    }
    if tu.tag_field.is_empty() {
        return Err("tagged_union!: no tag".into());
    }
    tu.size = size.ok_or_else(|| format!("tagged_union! {}: no size - what it's a function of, \
                                          `size: pad`, `size: header` or `size: arm`", tu.name))?;
    tu.check()?;
    Ok(tu)
}

impl CTaggedUnion {
    pub fn check(&self) -> Result<(), String> {
        let tu = self;
        // a constant size is a struct's, its own storage, which holds its arms; a
        // header's count or the arm's own size, a union's over another's storage,
        // the head its arms start with
        let ok = match tu.size {
            CTaggedSize::Pad                       => !tu.union && tu.storage.is_none() && tu.pad.is_some(),
            CTaggedSize::Header | CTaggedSize::Arm => tu.union && tu.storage.is_some() && tu.pad.is_none(),
        };
        if !ok {
            return Err(format!("tagged_union! {}: `struct NAME {{ .., pad: TYPE, size: pad }}` - its \
                                own storage, which holds its arms, the pad's size - or `union NAME \
                                in STORAGE {{ .., size: header }}` - over another's storage, whose \
                                arms run past its header as far as its count says - or `union NAME \
                                in HEAD {{ .., size: arm }}` - the head every arm starts with, each \
                                as long as its arm", tu.name));
        }
        if (tu.plain || tu.tag_is_enum) && tu.size != CTaggedSize::Pad {
            return Err(format!("tagged_union! {}: `plain` and `tag NAME: enum TYPE` are for a \
                                struct's own storage, `size: pad`", tu.name));
        }
        if tu.storage.is_some() {
            if let Some(d) = tu.defs.first() {
                return Err(format!("tagged_union! {}: {} is defined here, but the storage is \
                                    another's - `in STORAGE` - and so are its arms' types",
                                   tu.name, d.name));
            }
        }
        Ok(())
    }
}

/// `LIST(params) => template` at @item - a c_enum! variant - if it is one.
fn parse_from_list(item: &proc_macro2::TokenStream) -> Option<CFromList> {
    use proc_macro2::{Delimiter, TokenTree};
    let t: Vec<TokenTree> = item.clone().into_iter().collect();
    match t.as_slice() {
        [TokenTree::Ident(list), TokenTree::Group(params), TokenTree::Punct(eq), TokenTree::Punct(gt), template @ ..]
            if params.delimiter() == Delimiter::Parenthesis && eq.as_char() == '=' && gt.as_char() == '>' &&
               !template.is_empty() =>
            Some(CFromList {
                list:     ident(list),
                params:   split_commas(params.stream()).iter().map(rust_text).collect(),
                template: rust_text(&template.iter().cloned().collect::<proc_macro2::TokenStream>()),
            }),
        _ => None,
    }
}

/// @template, its concat(A, b, ..)s as @paste spells them - C's A##b, or
/// paste!'s [<A $b>].
#[allow(dead_code)]
pub fn spell_concat(template: &str, paste: impl Fn(&[&str]) -> String) -> Result<String, String> {
    let mut out = template.to_string();
    while let Some(i) = out.find("concat(") {
        let j = i + out[i..].find(')').ok_or_else(|| format!("an unclosed concat( in the template {template}"))?;
        let pasted = paste(&out[i + "concat(".len()..j].split(',').map(str::trim).collect::<Vec<_>>());
        out.replace_range(i..=j, &pasted);
    }
    Ok(out)
}

/// What @f's template makes of one entry of its list, @entry its source: its
/// parameters substituted, then its camel(n)s and concat(A, b, ..)s pasted.
fn from_list_entry(f: &CFromList, entry: &str) -> Result<Vec<proc_macro2::TokenTree>, String> {
    use proc_macro2::{Delimiter, Group, Ident, Span, TokenStream, TokenTree};
    let list = &f.list;
    let lex = |s: &str| s.parse::<TokenStream>().map_err(|e| format!("{list}: {s}: {e}"));
    let args = split_commas(lex(entry)?);
    let mut bound: Vec<(&str, &TokenStream)> = Vec::new();
    for (n, p) in f.params.iter().enumerate() {
        if p == ".." {
            break;
        }
        let arg = args.get(n).ok_or_else(|| format!("{list}: entry ({entry}) has no {p}"))?;
        bound.push((p, arg));
    }

    fn subst(ts: TokenStream, bound: &[(&str, &TokenStream)]) -> TokenStream {
        ts.into_iter().flat_map(|t| match t {
            TokenTree::Ident(i) => match bound.iter().find(|(p, _)| i == p) {
                Some((_, arg)) => (*arg).clone(),
                None => TokenStream::from(TokenTree::Ident(i)),
            },
            TokenTree::Group(g) => TokenStream::from(TokenTree::Group(Group::new(g.delimiter(), subst(g.stream(), bound)))),
            t => TokenStream::from(t),
        }).collect()
    }

    let bad = || format!("{list}: concat() in the template pastes identifiers and literals only");
    let name = |s: String| match s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') &&
                                 s.chars().all(|c| c.is_alphanumeric() || c == '_') {
        true => Ok(TokenTree::Ident(Ident::new(&s, Span::call_site()))),
        false => Err(format!("{list}: pastes {s}, not an identifier")),
    };
    fn paste(ts: TokenStream, name: &dyn Fn(String) -> Result<TokenTree, String>, bad: &dyn Fn() -> String)
        -> Result<Vec<TokenTree>, String>
    {
        let t: Vec<TokenTree> = ts.into_iter().collect();
        let mut out = Vec::new();
        let mut k = 0;
        while k < t.len() {
            match (&t[k], t.get(k + 1)) {
                (TokenTree::Ident(c), Some(TokenTree::Group(g)))
                    if g.delimiter() == Delimiter::Parenthesis && (c == "camel" || c == "concat") =>
                {
                    let parts = split_commas(g.stream()).into_iter().map(|p| {
                        match p.into_iter().collect::<Vec<_>>().as_slice() {
                            [TokenTree::Ident(i)] => Ok(i.to_string()),
                            [TokenTree::Literal(l)] => Ok(l.to_string()),
                            _ => Err(bad()),
                        }
                    }).collect::<Result<Vec<_>, _>>()?;
                    out.push(match (c == "camel", parts.as_slice()) {
                        (true, [n]) => name(n.split('_').map(|w| w[..1].to_uppercase() + &w[1..]).collect())?,
                        (true, _) => return Err(bad()),
                        (false, _) => name(parts.concat())?,
                    });
                    k += 2;
                }
                (TokenTree::Group(g), _) => {
                    let mut inner = Group::new(g.delimiter(), paste(g.stream(), name, bad)?.into_iter().collect());
                    inner.set_span(g.span());
                    out.push(TokenTree::Group(inner));
                    k += 1;
                }
                (t, _) => {
                    out.push(t.clone());
                    k += 1;
                }
            }
        }
        Ok(out)
    }
    paste(subst(lex(&f.template)?, &bound), &name, &bad)
}

/// c_enum! @e's variants from its first list, @entries each the source of one
/// entry's arguments, in their place: each `NAME [= value]`, a value cast to
/// the enum's integer type, as a list's can be any integer type.
#[allow(dead_code)]
pub fn expand_enum_list(e: &mut CEnumDef, entries: &[String]) -> Result<(), String> {
    if e.from.is_empty() {
        return Ok(());
    }
    let (at, f) = e.from.remove(0);
    let mut variants = Vec::new();
    use proc_macro2::{TokenStream, TokenTree};
    let lex = |s: &str| s.parse::<TokenStream>().map_err(|err| format!("{}: {s}: {err}", f.list));
    let text = |t: &[TokenTree]| rust_text(&t.iter().cloned().collect::<TokenStream>());

    // the parameter the template pastes into the name: its argument, the label
    fn idents(ts: TokenStream, out: &mut Vec<String>) {
        for t in ts {
            match t {
                TokenTree::Ident(i) => out.push(ident(&i)),
                TokenTree::Group(g) => idents(g.stream(), out),
                _ => {}
            }
        }
    }
    let before_eq: TokenStream = lex(&f.template)?.into_iter()
        .take_while(|t| !matches!(t, TokenTree::Punct(p) if p.as_char() == '='))
        .collect();
    let mut name_idents = Vec::new();
    idents(before_eq, &mut name_idents);
    let label_param = name_idents.iter().find_map(|s| f.params.iter().position(|p| p == s));

    for entry in entries {
        let toks = from_list_entry(&f, entry)?;
        let (name, value) = match &toks[..] {
            [TokenTree::Ident(name)] => (ident(name), None),
            [TokenTree::Ident(name), TokenTree::Punct(eq), v @ ..] if eq.as_char() == '=' && !v.is_empty() =>
                (ident(name), Some(format!("(({}) as {})", text(v), e.repr))),
            _ => return Err(format!("{}: the template gives {} - not NAME [= value]", f.list, text(&toks))),
        };
        let label = label_param.and_then(|n| split_commas(lex(entry).ok()?).into_iter().nth(n))
            .and_then(|a| match a.into_iter().collect::<Vec<_>>().as_slice() {
                [TokenTree::Ident(l)] => Some(ident(l)),
                _ => None,
            });
        variants.push(CEnumVariant { name, value, label });
    }
    let n = variants.len();
    e.variants.splice(at..at, variants);
    for (later, _) in &mut e.from {
        *later += n;
    }
    Ok(())
}

/// c_enum! @e as source: its variants, and the lists not yet expanded in their
/// place - with all expanded, the record's form.
#[allow(dead_code)]
pub fn enum_text(e: &CEnumDef) -> String {
    let mut t = format!("#[{}] {}{}pub enum {}: {} {{ ", e.kind.attr(),
                        if e.own_to_text { "#[own_to_text] " } else { "" },
                        e.c_name.as_deref().map(|c| format!("#[c_name({c})] ")).unwrap_or_default(),
                        e.name.as_deref().unwrap_or("_"), e.repr);
    let lists = |t: &mut String, i: usize| for (at, f) in &e.from {
        if *at == i {
            t.push_str(&format!("{}({}) => {}, ", f.list, f.params.join(", "), f.template));
        }
    };
    for (i, v) in e.variants.iter().enumerate() {
        lists(&mut t, i);
        if let Some(l) = &v.label {
            t.push_str(&format!("#[label({l:?})] "));
        }
        match &v.value {
            Some(x) => t.push_str(&format!("{} = {x}, ", v.name)),
            None => t.push_str(&format!("{}, ", v.name)),
        }
    }
    lists(&mut t, e.variants.len());
    t + "}"
}

/// The record's form of @tu, with @arms: each written out.
#[allow(dead_code)]
pub fn tagged_union_record_text(tu: &CTaggedUnion, arms: &[CTaggedArm]) -> String {
    let tag_enum = if tu.tag_enum.is_empty() { String::new() } else { format!(" = {}", tu.tag_enum) };
    let enum_kw = if tu.tag_is_enum { "enum " } else { "" };
    let c_name = tu.c_name.as_ref().map_or(String::new(), |c| format!("#[c_name({c})] "));
    let mut t = format!("tagged_union! {{ {c_name}{} {} {{ tag {}: {enum_kw}{}{tag_enum}, ",
                        tu.keyword(), tu.name, tu.tag_field, tu.tag_ty);
    for a in arms {
        t.push_str(&format!("arm {}: {} = {}, ", a.name, a.ty, a.value));
    }
    if let Some(p) = &tu.pad {
        t.push_str(&format!("pad: {p}, "));
    }
    t.push_str(&format!("size: {}, ", tu.size.keyword()));
    if tu.packed {
        t.push_str("packed, ");
    }
    if tu.plain {
        t.push_str("plain, ");
    }
    t + "} }"
}

impl CTaggedUnion {
    fn keyword(&self) -> &'static str {
        if self.union { "union" } else { "struct" }
    }

    /// What C calls it: `struct NAME` or `union NAME`.
    #[allow(dead_code)]
    pub fn c_type(&self) -> String {
        format!("{} {}", self.keyword(), self.c_name.as_deref().unwrap_or(&self.name))
    }
}

/// tagged_union!'s C - a struct's: the storage over another's has its own -
/// the tag, then an anonymous union of the arms, inside an anonymous union
/// with the pad, if there is one. `packed`: the struct and the arms' union
/// are __packed.
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
            under_cfg(&tu.cfg, format!("{} {{\n{body}}};\n", tu.c_type()))
        }
        None => under_cfg(&tu.cfg, format!("{} {{\n{tagged}}}{packed};\n", tu.c_type())),
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
    proto_ty(&type_of(ty)?, decl.to_string(), false, last, kind)
}

fn type_of(s: &str) -> Option<syn::Type> {
    syn::parse_str(s).ok()
}

/// @ty's generic arguments, if it's @name<..> by any path.
fn generic_args<'a>(ty: &'a syn::Type, name: &str) -> Option<Vec<&'a syn::GenericArgument>> {
    let syn::Type::Path(p) = ty else { return None };
    let (last, path) = p.path.segments.iter().collect::<Vec<_>>().split_last().map(|(l, p)| (*l, p.to_vec()))?;
    if p.qself.is_some() || last.ident != name || path.iter().any(|s| !s.arguments.is_none()) {
        return None;
    }
    match &last.arguments {
        syn::PathArguments::AngleBracketed(a) => Some(a.args.iter().collect()),
        _ => None,
    }
}

/// T, if @ty is @name<T> by any path.
fn arg_of<'a>(ty: &'a syn::Type, name: &str) -> Option<&'a syn::Type> {
    match generic_args(ty, name)?.as_slice() {
        [syn::GenericArgument::Type(t)] => Some(t),
        _ => None,
    }
}

/// @ty's path's names, if it's a plain path - no generics but lifetimes, which
/// C doesn't see.
fn path_names(ty: &syn::Type) -> Option<Vec<String>> {
    let lifetimes_only = |a: &syn::PathArguments| match a {
        syn::PathArguments::None => true,
        syn::PathArguments::AngleBracketed(a) => a.args.iter().all(|a| matches!(a, syn::GenericArgument::Lifetime(_))),
        syn::PathArguments::Parenthesized(_) => false,
    };
    match ty {
        syn::Type::Path(p) if p.qself.is_none() && p.path.segments.iter().all(|s| lifetimes_only(&s.arguments)) =>
            Some(p.path.segments.iter().map(|s| ident(&s.ident)).collect()),
        _ => None,
    }
}

fn proto_ty(ty: &syn::Type, decl: String, konst: bool, last: bool,
            kind: &dyn Fn(&str) -> Option<String>)
    -> Option<String>
{
    // Head<T>, a family's C definition (util/vstructs.rs), Volatile<T>, a
    // published word (util/projections.rs), and Cell<T>: to C, T itself
    if let Some(t) = arg_of(ty, "Head").or_else(|| arg_of(ty, "Volatile")).or_else(|| arg_of(ty, "Cell")) {
        return proto_ty(t, decl, konst, last, kind);
    }
    let q = if konst { "const " } else { "" };
    // BkeyI<Head<bch_foo>>, by any path: a key whose value is a family's,
    // whole - C's typed key, struct bkey_i_foo
    if let Some(name) = bkey_i_arg(ty, kind) {
        return Some(format!("{q}struct bkey_i_{name} {decl}").trim_end().to_string());
    }
    // Option<unsafe extern "C" fn(ARGS) -> RET>: a function pointer.
    if let Some(f) = arg_of(ty, "Option") {
        let syn::Type::BareFn(f) = f else { return None };
        let mut args = f.inputs.iter().map(|a| {
            // a named parameter: C's, named
            let n = a.name.as_ref().map(|(n, _)| ident(n)).filter(|n| n != "_").map(|n| c_ident(&n));
            proto_ty(&a.ty, n.unwrap_or_default(), false, false, kind)
        }).collect::<Option<Vec<_>>>()?;
        if f.variadic.is_some() {
            args.push("...".into());
        }
        let args = if args.is_empty() { "void".to_string() } else { args.join(", ") };
        let inner_decl = format!("(*{q}{decl})({args})");
        return match &f.output {
            syn::ReturnType::Default => Some(format!("void {inner_decl}")),
            syn::ReturnType::Type(_, ret) => proto_ty(ret, inner_decl, false, false, kind),
        };
    }
    match ty {
        syn::Type::Array(a) => {
            let n = c_dim(&a.len)?;
            let n = if n == "0" && last { String::new() } else { n };
            let decl = if decl.starts_with('*') { format!("({decl})") } else { decl };
            proto_ty(&a.elem, format!("{decl}[{n}]"), konst, false, kind)
        }
        syn::Type::Ptr(p) => proto_ty(&p.elem, format!("*{q}{decl}"), p.const_token.is_some(), false, kind),
        // never returns: __noreturn is an attribute, not the type
        syn::Type::Never(_) => Some(format!("void {decl}")),
        syn::Type::Paren(p) => proto_ty(&p.elem, decl, konst, last, kind),
        syn::Type::Group(g) => proto_ty(&g.elem, decl, konst, last, kind),
        // c::NAME, or a type of ours by its Rust name, by any path: C's, from
        // the struct map - a CStruct by its name whatever its arguments, being
        // one layout for every T
        _ => {
            let name = match ty {
                syn::Type::Path(p) if p.qself.is_none() => p.path.segments.last().map(|s| ident(&s.ident)),
                _ => None,
            };
            let base = name.and_then(|n| kind(&n)).or_else(|| canon_base(ty))?;
            Some(base_decl(q, &base, &decl))
        }
    }
}

/// A declaration of @decl as @base - which may itself end in a pointer, as a
/// c_rcu_array!'s does.
fn base_decl(q: &str, base: &str, decl: &str) -> String {
    let sep = if base.ends_with('*') { "" } else { " " };
    format!("{q}{base}{sep}{decl}").trim_end().to_string()
}

/// foo, if @ty is BkeyI<V> by any path, V a key value type - or
/// BkeyI<Head<V>>, an unsized one's: C's struct bkey_i_foo, V's C name being
/// struct bch_foo, by @kind - V named as Rust names it, or as C does.
fn bkey_i_arg(ty: &syn::Type, kind: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let v = arg_of(ty, "BkeyI")?;
    let v = path_names(arg_of(v, "Head").unwrap_or(v))?.pop()?;
    kind(&v).unwrap_or_else(|| format!("struct {v}")).strip_prefix("struct bch_").map(str::to_string)
}

/// c_extern!'s C: a prototype for each fn, an extern declaration for each
/// static.
fn emit_extern(ex: &CExtern, kind: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let c = |ty: &str, decl: &str, what: &str| proto_c(ty, decl, kind)
        .ok_or_else(|| format!("{what}: {ty} has no C rendering"));
    let mut t = String::new();
    for f in &ex.fns {
        let mut params = f.params.iter()
            .map(|(_, ty)| c(ty, "", &format!("fn {}", f.name)))
            .collect::<Result<Vec<_>, _>>()?;
        if f.variadic {
            params.push("...".into());
        }
        let params = if params.is_empty() { "void".to_string() } else { params.join(", ") };
        let must_check = if f.must_use { "__must_check " } else { "" };
        let decl = format!("{must_check}{}({params})", f.name);
        let proto = match f.ret.as_deref().map(str::trim) {
            None => format!("void {decl}"),
            Some("!") => format!("void __noreturn {decl}"),
            Some(r) => c(r, &decl, &format!("fn {}", f.name))?,
        };
        let printf = f.printf.as_ref().map(|p| format!("__printf({p}) ")).unwrap_or_default();
        let cold = if f.cold { "__cold " } else { "" };
        let proto = format!("{printf}{cold}{proto}");
        t.push_str(&under_cfg(&f.cfg, format!("{proto};\n"))?);
    }
    for s in &ex.statics {
        // a static that isn't mut is C's const: what it holds, an array's
        // elements - for a pointer, the pointer
        let decl = match &s.c_decl {
            Some(d) => d.clone(),
            None => type_of(&s.ty).and_then(|t| proto_ty(&t, s.name.clone(), !s.mutable, true, kind))
                .ok_or_else(|| format!("static {}: {} has no C rendering", s.name, s.ty))?,
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

/// The C base type Rust type @ty renders as - the canonical rendering, by
/// which a field needs no #[c]: integers by their plain names, endian
/// integers - Le16..Le64, zerocopy's le::/be:: - as __leN/__beN, atomics as
/// the integers they hold, c::foo as struct foo - unless foo ends in _t, a
/// typedef.
fn canon_base(ty: &syn::Type) -> Option<String> {
    let syn::Type::Path(p) = ty else { return None };
    if p.qself.is_some() || p.path.segments.iter().any(|s| !s.arguments.is_none()) {
        return None;
    }
    let path: Vec<String> = p.path.segments.iter().map(|s| s.ident.to_string()).collect();
    canon_path(&path.iter().map(String::as_str).collect::<Vec<_>>())
}

fn canon_path(path: &[&str]) -> Option<String> {
    let s = match path {
        ["u8"] => "u8", ["u16"] => "u16", ["u32"] => "u32", ["u64"] => "u64",
        ["i8"] => "s8", ["i16"] => "s16", ["i32"] => "s32", ["i64"] => "s64",
        ["bool"] => "bool", ["usize"] => "size_t", ["isize"] => "ssize_t",
        [.., "c_void"] => "void", [.., "c_char"] => "char",
        [.., "c_short"] => "short", [.., "c_ushort"] => "unsigned short",
        [.., "c_int"] => "int", [.., "c_uint"] => "unsigned",
        [.., "c_long"] => "long", [.., "c_ulong"] => "unsigned long",
        // C's atomic types, which its atomic_*() take: the signed ones - an
        // unsigned atomic is a plain integer to C
        [.., "AtomicI32"] => "atomic_t", [.., "AtomicI64"] => "atomic64_t",
        [.., "AtomicIsize"] => "atomic_long_t",
        [.., "AtomicULong"] => "unsigned long",
        [.., "AtomicU8"] => "u8", [.., "AtomicU16"] => "u16",
        [.., "AtomicU32"] => "u32", [.., "AtomicU64"] => "u64",
        [.., "AtomicUsize"] => "size_t",
        [.., "c_longlong"] => "long long", [.., "c_ulonglong"] => "unsigned long long",
        [.., "Le16"] => "__le16", [.., "Le32"] => "__le32", [.., "Le64"] => "__le64",
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

/// A field's Rust type, as #[derive(TypeInfo)] describes it.
#[allow(dead_code)] // the macros', not rust_types_gen's
pub enum FieldType {
    /// `[elem; n]`, both as Rust source
    Array { elem: String, n: String },
    /// A pointer, reference, function or generic: nothing to describe
    Opaque,
    /// Anything else: the C base type it renders as, if it has one - an
    /// integer's name, `struct foo` - and the last segment of its path.
    Named { c: Option<String>, name: String },
}

/// What @ty is, for #[derive(TypeInfo)]: an integer's width, endianness and
/// signedness are read from canon_base(), the C the generator renders it as,
/// so a field's TypeInfo and its C declaration can't disagree.
#[allow(dead_code)] // the macros', not rust_types_gen's
pub fn field_type(ty: &str) -> Result<FieldType, String> {
    Ok(match type_of(ty).ok_or_else(|| format!("{ty}: not a type"))? {
        syn::Type::Array(a) => FieldType::Array { elem: rust_text(&a.elem), n: rust_text(&a.len) },
        t => match path_names(&t) {
            Some(p) => FieldType::Named { c: canon_base(&t), name: p.last().cloned().unwrap_or_default() },
            None => FieldType::Opaque,
        },
    })
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

/// A bitfield-struct member that's padding: Rust's, where C leaves bits
/// unused.
fn is_bitfield_pad(name: &str) -> bool {
    name.starts_with("__pad")
}

/// The members of bitfield type @bf, as C declares them: least significant
/// first on little-endian bitfields, the reverse on big-endian - so each is at
/// the same bits of the same native-endian word either way. A member of an
/// enum type is C's `enum foo name:N`.
fn emit_bitfield(bf: &CBitfieldDef, kind: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let decl = |(name, ty, width): &(String, String, u32)| -> Result<String, String> {
        let ty = bf.c_type(ty);
        let d = canon_c(ty, &bitfield_c_name(name), false, kind)
            .ok_or_else(|| format!("{}.{name}: {ty} has no C type", bf.name))?;
        Ok(if Some(*width) == int_bits(ty) {
            format!("\t{d};\n")
        } else {
            format!("\t{d}:{width};\n")
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
            let base = type_of(&f.1).as_ref().and_then(canon_base)
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

/// A Locked<R, G, P>, as @ty, Rust source: R, G and P - G and P (), as Locked's
/// defaults, when they're left out.
pub fn lock_args(ty: &str) -> Result<Option<[String; 3]>, String> {
    Ok(match container_args(ty, "Locked")?.as_deref() {
        Some([r, rest @ ..]) if rest.len() <= 2 => {
            let arg = |n: usize| rest.get(n).cloned().unwrap_or_else(|| "()".into());
            Some([r.clone(), arg(0), arg(1)])
        }
        _ => None,
    })
}

/// What #[c_lock] field @f's Locked protects and publishes, G and P: structs of
/// @items, which C writes in its place, after the lock - None for ().
pub fn lock_defs<'a>(f: &CFieldDef, items: &'a [CItem]) -> Result<[Option<&'a CStructDef>; 2], String> {
    let [_, g, p] = lock_args(&f.ty)?.ok_or_else(|| format!("{}: #[c_lock] is for a Locked, not {}", f.name, f.ty))?;
    let def = |ty: &str| -> Result<_, String> {
        if ty == "()" {
            return Ok(None);
        }
        items.iter()
            .find_map(|i| match i {
                CItem::Struct(d) if d.name == ty => Some(d),
                _ => None,
            })
            .map(Some)
            .ok_or_else(|| format!("{}: #[c_lock], but {ty} isn't a struct or union here", f.name))
    };
    Ok([def(&g)?, def(&p)?])
}

/// The name a #[c_lock] declaration gives the lock: its last word.
pub fn lock_name(decl: &str) -> Option<&str> {
    let name = decl.trim_end().rsplit(|ch: char| !(ch.is_alphanumeric() || ch == '_')).next()?;
    (!name.is_empty() && !name.starts_with(|ch: char| ch.is_ascii_digit())).then_some(name)
}

/// Rust type @ty as an element type and C's array dimensions - `[[T; 2]; N]`
/// is T and [N][2] - or @ty itself and none. None if a dimension has no C
/// rendering.
fn array_of(ty: &str) -> Option<(String, String)> {
    let ty = type_of(ty)?;
    let mut elem = &ty;
    let mut dims = String::new();
    while let syn::Type::Array(a) = elem {
        dims.push_str(&format!("[{}]", c_dim(&a.len)?));
        elem = &a.elem;
    }
    Some((rust_text(elem), dims))
}

/// An array dimension as C: a number, or a constant C has by name - a usize
/// one, or another `as usize`.
fn c_dim(e: &syn::Expr) -> Option<String> {
    let c_name = |p: &syn::Path| match p.segments.iter().map(|s| ident(&s.ident)).collect::<Vec<_>>().as_slice() {
        [c, name] if is_c_ns(c) => Some(name.clone()),
        _ => None,
    };
    match e {
        syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(n), .. }) => Some(n.to_string().trim_end_matches("usize").into()),
        syn::Expr::Path(p) => c_name(&p.path),
        syn::Expr::Cast(c) if rust_text(&c.ty) == "usize" => match &*c.expr {
            syn::Expr::Path(p) => c_name(&p.path),
            _ => None,
        },
        syn::Expr::Group(g) => c_dim(&g.expr),
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
    // a LockedBy<G>, publishing nothing: G, which it holds at its start
    let ty = type_of(&f.ty).and_then(|t| arg_of(&t, "LockedBy").map(rust_text)).unwrap_or_else(|| f.ty.clone());
    let (elem, dims) = array_of(&ty)?;
    if !dims.is_empty() && !f.inline {
        return None;
    }
    items.iter().find_map(|i| match i {
        CItem::Struct(d) if d.rust_ident() == elem.trim() => Some(d),
        _ => None,
    })
}

/// The types of @items that fields declare in their place: C has no
/// definition of its own for them.
#[allow(dead_code)]
pub fn in_place_types(items: &[CItem]) -> Vec<&str> {
    let fields = items.iter()
        .filter_map(|i| match i {
            CItem::Struct(d) => Some(d),
            _ => None,
        })
        .flat_map(|d| d.fields.iter());
    let in_place = fields.clone().filter_map(|f| in_place_def(f, items));
    // what a #[c_lock]'s Locked protects and publishes, written after the lock
    let locked = fields
        .filter(|f| f.lock.is_some())
        .flat_map(|f| lock_defs(f, items).into_iter().flatten().flatten());
    // and a c_rcu_array!'s header, written in its struct
    let heads = items.iter()
        .filter_map(|i| match i {
            CItem::RcuArray(a) => rcu_array_head(a, items),
            _ => None,
        });
    in_place.chain(locked).chain(heads).map(|d| d.name.as_str()).collect()
}

/// @a's header's definition, if it has one and it's a struct of @items.
pub fn rcu_array_head<'a>(a: &CRcuArray, items: &'a [CItem]) -> Option<&'a CStructDef> {
    let head = a.head.as_deref()?;
    let name = head.rsplit("::").next()?.trim();
    items.iter().find_map(|i| match i {
        CItem::Struct(d) if d.name == name || d.rust_ident() == name => Some(d),
        _ => None,
    })
}

/// c_rcu_array!'s C: the struct of its buffer, laid out as util/rcu.rs's
/// RcuArrayBuf - the header's fields an anonymous struct, so they're laid out
/// as Rust's nested header is, and C names them as the buffer's.
fn emit_rcu_array(a: &CRcuArray, items: &[CItem],
                  opaque: &dyn Fn(&CStructDef, &CFieldDef) -> Result<String, String>,
                  kind: &dyn Fn(&str) -> Option<String>)
    -> Result<String, String>
{
    let elems = canon_c(&a.elem, &format!("{}[] __counted_by({})", a.elems, a.nr), true, kind)
        .ok_or_else(|| format!("its element type {} has no C rendering", a.elem))?;
    let head = match &a.head {
        None => String::new(),
        Some(h) => {
            let def = rcu_array_head(a, items)
                .ok_or_else(|| format!("its header {h} isn't a #[repr(C)] struct of this file"))?;
            let members: String = emit_members(def, items, opaque, kind)?
                .lines()
                .map(|l| if l.starts_with('#') { format!("{l}\n") } else { format!("\t{l}\n") })
                .collect();
            format!("\tstruct {{\n{members}\t}};\n")
        }
    };
    under_cfg(&a.cfg, format!("struct {} {{\n\tstruct rcu_head\trcu;\n\tsize_t\t\t{};\n{head}\t{elems};\n}};\n",
                              a.c_name, a.nr))
}

/// A zero-length array in a union: C can't have a flexible array member
/// there, but the kernel's __DECLARE_FLEX_ARRAY wraps one in an anonymous
/// struct, which can.
fn union_flex_array(ty: &str, name: &str) -> Option<String> {
    let syn::Type::Array(a) = type_of(ty)? else { return None };
    if !matches!(&a.len, syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(n), .. }) if n.to_string() == "0") {
        return None;
    }
    Some(format!("__DECLARE_FLEX_ARRAY({}, {name})", canon_base(&a.elem)?))
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
    // whether C has a member before this one: a flexible array, x[], needs
    // one - a struct of only x[] is an error to gcc before 15 - so with none
    // it's x[0], as bch_val's __nothing was written
    let mut named = false;
    // the members of @inner, a struct field @f's #[@attr] writes in its place
    let nested = |f: &CFieldDef, attr: &str, inner: &CStructDef| -> Result<String, String> {
        if !inner.repr_c() || inner.typedef.is_some() {
            return Err(format!("{}.{}: #[{attr}]'s type {} must be #[repr(C)], and not a typedef",
                               def.name, f.name, inner.name));
        }
        Ok(emit_members(inner, items, opaque, kind)?
            .lines()
            .map(|l| if l.starts_with('#') { format!("{l}\n") } else { format!("\t{l}\n") })
            .collect())
    };
    for (n, f) in def.fields.iter().enumerate() {
        let last = n + 1 == def.fields.len() && named;
        let text = if f.bitfield {
            let bf = items.iter().find_map(|i| match i {
                CItem::Bitfield(b) if b.name == bitfield_type(&f.ty) => Some(b),
                _ => None,
            }).ok_or_else(|| format!("{}.{}: #[c_bitfield], but {} isn't a #[bitfield] type here",
                                     def.name, f.name, f.ty))?;
            emit_bitfield(bf, kind)?
        } else if let Some(lock) = &f.lock {
            // the lock, then what it protects, then what it publishes: as
            // Locked lays them out
            let mut text = format!("\t{lock};\n");
            for inner in lock_defs(f, items).map_err(|e| format!("{}.{e}", def.name))?.into_iter().flatten() {
                let members = nested(f, "c_lock", inner)?;
                text += &format!("\t{} {{\n{members}\t}}{};\n", inner.kind.keyword(), c_attrs(inner)?);
            }
            text
        } else if in_place(f) {
            let attr = match (f.inline, f.group) {
                (true, _) => "c_inline",
                (_, true) => "c_struct_group",
                _ => "c_anon",
            };
            let inner = in_place_def(f, items).ok_or_else(|| format!(
                "{}.{}: #[{attr}], but {} isn't a struct or union here", def.name, f.name, f.ty))?;
            let members = nested(f, attr, inner)?;
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
        } else if let Some(decl) = container_c(&f.ty, &f.name, kind)
            .map_err(|e| format!("{}.{e}", def.name))? {
            format!("\t{decl};\n")
        } else if let Some(decl) = canon_c(&f.ty, &f.name, last, kind) {
            format!("\t{decl};\n")
        } else {
            format!("\t{};\n", opaque(def, f)?)
        };
        let text = match (&f.c_align, text.strip_suffix(";\n")) {
            (None, _) => text,
            (Some(a), Some(decl)) => format!("{decl} __aligned({a});\n"),
            (Some(_), None) => return Err(format!("{}.{}: after an alignment marker, but C declares it \
                                                   so __aligned() can't follow", def.name, f.name)),
        };
        named |= !text.is_empty();
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

/// @ty's type arguments, Rust source, if it's @name<...> - one of
/// types/lib.rs's containers of C's, by any path.
/// A #[c_bitfield] field's bitfield type, from its type @ty: the type, or a
/// Volatile<T>'s or Cell<T>'s T - a word read and written whole, whose
/// members C declares in its place all the same.
pub fn bitfield_type(ty: &str) -> String {
    bitfield_wrapper(ty).and_then(|w| match container_args(ty, w) {
        Ok(Some(args)) if args.len() == 1 => Some(args[0].trim().to_string()),
        _ => None,
    }).unwrap_or_else(|| ty.trim().to_string())
}

/// What a #[c_bitfield] field's bitfield is wrapped in, if anything:
/// "Volatile" or "Cell".
pub fn bitfield_wrapper(ty: &str) -> Option<&'static str> {
    ["Volatile", "Cell"].into_iter().find(|w| matches!(container_args(ty, w), Ok(Some(a)) if a.len() == 1))
}

fn container_args(ty: &str, name: &str) -> Result<Option<Vec<String>>, String> {
    let t = type_of(ty).ok_or_else(|| format!("{ty}: not a type"))?;
    Ok(generic_args(&t, name).map(|args| args.iter().map(rust_text).collect()))
}

/// A DArray<T> or DArray<T, N> - types/lib.rs's, C's DARRAY() - as @ty, Rust
/// source: T, and N if it's there.
fn darray_args(ty: &str) -> Result<Option<(String, Option<String>)>, String> {
    Ok(match container_args(ty, "DArray")?.as_deref() {
        Some([elem]) => Some((elem.clone(), None)),
        Some([elem, nr]) => Some((elem.clone(), Some(nr.clone()))),
        _ => None,
    })
}

/// @decl of container type @ty - a DArray or a GenRadix - as C declares it,
/// by the container's macro: DARRAY(T), DARRAY_PREALLOCATED(T, N),
/// GENRADIX(T). None if @ty is no container.
fn container_c(ty: &str, decl: &str, kind: &dyn Fn(&str) -> Option<String>)
    -> Result<Option<String>, String>
{
    let elem_c = |elem: &str| canon_c(elem, "", false, kind)
        .map(|e| e.trim().to_string())
        .ok_or_else(|| format!("{decl}: {ty}'s element type {elem} has no C rendering"));
    if let Some((elem, nr)) = darray_args(ty)? {
        let elem = elem_c(&elem)?;
        return Ok(Some(match nr {
            None => format!("DARRAY({elem}) {decl}"),
            Some(n) => format!("DARRAY_PREALLOCATED({elem}, {}) {decl}", darray_nr_c(&n)
                .ok_or_else(|| format!("{decl}: {ty}'s length {n} has no C rendering"))?),
        }));
    }
    if let Some([elem]) = container_args(ty, "GenRadix")?.as_deref() {
        return Ok(Some(format!("GENRADIX({}) {decl}", elem_c(elem)?)));
    }
    Ok(None)
}

/// DArray<T, N>'s N as C: a const generic, so a constant's in braces.
fn darray_nr_c(n: &str) -> Option<String> {
    match syn::parse_str::<syn::Expr>(n).ok()? {
        syn::Expr::Block(b) => match b.block.stmts.as_slice() {
            [syn::Stmt::Expr(e, None)] => c_dim(e),
            _ => None,
        },
        e => c_dim(&e),
    }
}

/// A declaration of each struct, union and enum @c names inside parentheses -
/// in a prototype: a c_extern!'s, a callback field's, a function typedef's. One
/// a prototype names before it's declared would be the prototype's own, which
/// conflicts with the real one. An enum too: incomplete until its definition,
/// as GNU C allows. Macros are left out - they declare nothing until they're
/// used - and so are comments.
fn forward_decls(c: &str) -> String {
    let mut decls = std::collections::BTreeSet::new();
    let mut depth = 0usize;
    let mut in_comment = false;
    for line in c.lines() {
        if !in_comment && line.trim_start().starts_with('#') {
            continue;
        }
        let b = line.as_bytes();
        let ident = |i: usize| b.get(i).is_some_and(|&c| c == b'_' || c.is_ascii_alphanumeric());
        let mut i = 0;
        while i < b.len() {
            if in_comment {
                if b[i..].starts_with(b"*/") {
                    in_comment = false;
                    i += 1;
                }
            } else if b[i..].starts_with(b"/*") {
                in_comment = true;
                i += 1;
            } else if b[i..].starts_with(b"//") {
                break;
            } else if b[i] == b'(' {
                depth += 1;
            } else if b[i] == b')' {
                depth = depth.saturating_sub(1);
            } else if ident(i) {
                let start = i;
                while ident(i) { i += 1; }
                let word = &line[start..i];
                if depth > 0 && matches!(word, "struct" | "union" | "enum") {
                    let rest = line[i..].trim_start();
                    let n: String = rest.chars().take_while(|&c| c == '_' || c.is_ascii_alphanumeric()).collect();
                    if !n.is_empty() {
                        decls.insert(format!("{word} {n};\n"));
                    }
                }
                continue;
            }
            i += 1;
        }
    }
    decls.into_iter().collect()
}

/// The C for @items, in order, after the declarations their prototypes need
/// (forward_decls()). @opaque as for emit_struct().
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
                        t.push_str(&format!("#define {}({})\t\\\n\t{}({})\n", m.name, m.params.join(", "),
                                            x.callback, spell_concat(&m.template, |p| p.join("##"))?));
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
                // the struct as Rust names it - if not as C does, C's, which
                // @kind has from its record
                let c_type = match kind(&b.def) {
                    Some(c) if ![format!("struct {}", b.def), format!("union {}", b.def), b.def.clone()].contains(&c) => c,
                    _ => b.c_type.clone(),
                };
                let mut t = String::new();
                for e in &b.entries {
                    t.push_str(&format!("{}({}, {}, {}, {}, {});\n",
                                        b.macro_name, e.name, c_type, b.field, e.start, e.end));
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
                                    e.c_ident().map(|n| format!(" {n}")).unwrap_or_default());
                for v in &e.variants {
                    match &v.value {
                        Some(x) => t.push_str(&format!("\t{} = {x},\n", v.name)),
                        None => t.push_str(&format!("\t{},\n", v.name)),
                    }
                }
                // a byte or two wide, where a field holds it: GCC's packed enum
                // is the smallest type its values fit - a 16 bit one whose
                // values fit a byte is one, and its size assert says so
                let end = if matches!(e.repr.as_str(), "u8" | "i8" | "u16" | "i16") { "} __packed;\n" } else { "};\n" };
                under_cfg(&e.cfg, t + end)?
            }
            CItem::Const(c) => under_cfg(&c.cfg, format!("#define {}\t{}\n", c.name, c.c_value()?))?,
            CItem::Typedef(t) => match (&t.c_decl, darray_args(&t.ty)?) {
                // data_structures/darray.h's: the typedef and its cleanup class
                (None, Some((elem, nr))) => {
                    let elem = canon_c(&elem, "", false, kind)
                        .ok_or_else(|| format!("typedef {}: {elem} has no C rendering", t.name))?;
                    under_cfg(&t.cfg, match nr {
                        None => format!("DEFINE_DARRAY_NAMED({}, {});\n", t.name, elem.trim()),
                        Some(n) => {
                            let n = darray_nr_c(&n).ok_or_else(|| format!(
                                "typedef {}: {}'s length {n} has no C rendering", t.name, t.ty))?;
                            format!("typedef DARRAY_PREALLOCATED({}, {n}) {};\nDEFINE_DARRAY_CLASS({})\n",
                                    elem.trim(), t.name, t.name)
                        }
                    })?
                }
                _ => {
                    let decl = match &t.c_decl {
                        Some(d) => d.clone(),
                        None => match container_c(&t.ty, &t.name, kind).map_err(|e| format!("typedef {e}"))? {
                            Some(d) => d,
                            None => canon_c(&t.ty, &t.name, false, kind)
                                .ok_or_else(|| format!("typedef {}: {} has no C rendering - it needs a #[c(\"...\")]",
                                                       t.name, t.ty))?,
                        },
                    };
                    under_cfg(&t.cfg, format!("typedef {decl};\n"))?
                }
            },
            // C's own definition: the generator asserts it's the Rust type's.
            CItem::Same(_) => continue,
            CItem::RcuArray(a) => emit_rcu_array(a, items, opaque, kind)
                .map_err(|e| format!("c_rcu_array! {}: {e}", a.alias))?,
        };
        out.push('\n');
        out.push_str(&text);
    }
    let decls = forward_decls(&out);
    Ok(if decls.is_empty() { out } else { format!("\n{decls}{out}") })
}

// ---- entry points ----------------------------------------------------------

/// The type names @text's item writes - its own, and a family definition's
/// family, its fields', arms' and prototypes', a bitmask's struct's,
/// c_same!'s Rust type - for its record to say which type each is: each
/// that's a plain path, under a pointer, reference, array or generic
/// argument, as the generator looks types up; not the item's generic
/// parameters. The generator looks a Rust type up by nothing else.
#[allow(dead_code)]
pub fn written_types(text: &str) -> Vec<String> {
    let Ok(items) = parse_items(text) else { return Vec::new() };
    let generics: Vec<String> = match syn::parse_str::<syn::DeriveInput>(text) {
        Ok(d) => d.generics.type_params().map(|p| ident(&p.ident))
            .chain(d.generics.const_params().map(|p| ident(&p.ident))).collect(),
        Err(_) => Vec::new(),
    };

    // c_same!'s Rust type, written whole: it's the type the record is of
    let mut out = Vec::new();
    let mut tys = Vec::new();
    for item in &items {
        match item {
            CItem::Struct(d) => {
                // by name alone, its lifetimes elided - type_name() erases them -
                // and its type parameters defaulted, which is C's struct
                if d.type_params.len() == generics.len() {
                    tys.push(d.rust_ident().to_string());
                }
                tys.extend(d.family().map(str::to_string));
                // a #[c_bitfield]'s type is the bitfield's C, which Rust doesn't name
                tys.extend(d.fields.iter().filter(|f| !f.bitfield).map(|f| f.ty.clone()));
            }
            CItem::Bitfield(b) => tys.extend(b.fields.iter().map(|(_, ty, _)| ty.clone())),
            CItem::Enum(CEnumDef { name: Some(n), .. }) => tys.push(n.clone()),
            CItem::Bitmask(b) => tys.push(b.def.clone()),
            CItem::Typedef(t) => tys.extend([t.rust_ident().to_string(), t.ty.clone()]),
            // a struct's own name: a union over another's storage is generic
            CItem::TaggedUnion(tu) => tys.extend(tu.arms.iter().map(|a| a.ty.clone()).chain(tu.pad.clone())
                .chain((!tu.union).then(|| tu.name.clone()))),
            CItem::Extern(x) => {
                for f in &x.fns {
                    tys.extend(f.params.iter().map(|(_, ty)| ty.clone()).chain(f.ret.clone()));
                }
                tys.extend(x.statics.iter().map(|s| s.ty.clone()));
            }
            CItem::Same(s) => out.push(s.rust_ty.clone()),
            _ => {}
        }
    }

    // A type as C sees one: under pointers, references, arrays, generic
    // arguments and function pointers, a plain path
    fn walk(t: &syn::Type, out: &mut Vec<String>) {
        use syn::Type::*;
        match t {
            Ptr(syn::TypePtr { elem, .. }) | Reference(syn::TypeReference { elem, .. }) |
            Array(syn::TypeArray { elem, .. }) | Slice(syn::TypeSlice { elem, .. }) |
            Paren(syn::TypeParen { elem, .. }) | Group(syn::TypeGroup { elem, .. }) => walk(elem, out),
            BareFn(f) => {
                f.inputs.iter().for_each(|a| walk(&a.ty, out));
                if let syn::ReturnType::Type(_, r) = &f.output {
                    walk(r, out);
                }
            }
            Path(p) if p.qself.is_none() && p.path.leading_colon.is_none() => {
                for a in p.path.segments.iter().filter_map(|s| match &s.arguments {
                    syn::PathArguments::AngleBracketed(a) => Some(a),
                    _ => None,
                }) {
                    for g in &a.args {
                        if let syn::GenericArgument::Type(t) = g {
                            walk(t, out);
                        }
                    }
                }
                let Some(names) = path_names(t) else { return };
                if names.iter().any(|n| RUST_KEYWORDS.contains(&n.as_str()) && n != "crate" && n != "super") {
                    return;
                }
                let path = names.join("::");
                if !out.contains(&path) {
                    out.push(path);
                }
            }
            _ => {}
        }
    }

    for t in tys.iter().filter_map(|t| type_of(t)) {
        walk(&t, &mut out);
    }
    out.retain(|p| !generics.contains(p));
    out
}

/// Parse a derive's input: exactly one struct or union.
#[allow(dead_code)]
pub fn parse_cstruct(src: &str) -> Result<CStructDef, String> {
    let expected = "#[derive(CStruct)] is for structs and unions with named fields";
    let d = syn::parse_str::<syn::DeriveInput>(src).map_err(|e| format!("{expected} - {e}"))?;
    parse_struct(&d, item_attrs_of(&d.attrs)?)?.ok_or_else(|| expected.to_string())
}

/// Every C-shared item in a source file, in order.
#[allow(dead_code)]
pub fn parse_items(src: &str) -> Result<Vec<CItem>, String> {
    // A record's macro has its input alone: no attributes, and no `;` after
    // a (..) or [..] invocation
    if let Ok(m) = syn::parse_str::<syn::Macro>(src) {
        match m.path.get_ident().map(ident).as_deref() {
            Some("c_const") => return parse_consts(&m),
            Some("c_typedef") => return Ok(vec![CItem::Typedef(parse_typedef(&m)?)]),
            Some("c_xmacro") => return Ok(vec![CItem::XMacro(parse_xmacro(&m)?)]),
            Some("c_enum") => return Ok(vec![CItem::Enum(parse_enum(&m)?)]),
            Some("tagged_union") => return Ok(vec![CItem::TaggedUnion(parse_tagged_union(&m)?)]),
            Some("c_bitmask") => return Ok(vec![CItem::Bitmask(parse_bitmask(&m)?)]),
            Some("c_ioctl") => return Ok(vec![CItem::Ioctl(parse_ioctl(&m)?)]),
            Some("c_same") => return Ok(vec![CItem::Same(parse_same(&m)?)]),
            Some("c_rcu_array") => return Ok(vec![CItem::RcuArray(parse_rcu_array(&m)?)]),
            Some("c_extern") => return Ok(vec![CItem::Extern(parse_extern(&m)?)]),
            Some("c_verbatim") => {
                let text: syn::LitStr = m.parse_body().map_err(|_| "c_verbatim!: takes one string")?;
                return Ok(vec![CItem::Verbatim(CVerbatim { cfg: None, text: text.value().trim_matches('\n').into() })]);
            }
            _ => {}
        }
    }
    if let Some(e) = syn::parse_str::<syn::Item>(src).ok().filter(is_export).as_ref().and_then(export_of) {
        return Ok(vec![CItem::Extern(e?.ex)]);
    }

    // else a struct or a union: a #[bitfield]'s, or a CStruct
    let d = syn::parse_str::<syn::DeriveInput>(src).map_err(|e| format!("{src}: {e}"))?;
    let a = item_attrs_of(&d.attrs)?;
    if let Some(storage) = a.bitfield.clone() {
        if let Some(def) = parse_bitfield(&d, a.cfg.clone(), storage, a.bitfield_ty.clone())? {
            return Ok(vec![CItem::Bitfield(def)]);
        }
    }
    Ok(match a.derives {
        true => parse_struct(&d, a)?.map(CItem::Struct).into_iter().collect(),
        false => Vec::new(),
    })
}
