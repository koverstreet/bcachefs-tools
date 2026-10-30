// SPDX-License-Identifier: GPL-2.0
//! `#[derive(Options)]`: bch_opts, the options - a field each, described by
//! its `#[opt(...)]` and its doc comment, the option's help:
//!
//! ```text
//! /// Filesystem block size
//! #[opt(uint(512, 1 << 15), flags = fs | format | human_readable, sb = bch_sb::block_size,
//!       default = 4 << 10, hint = "size")]
//! pub block_size: u16,
//! ```
//!
//! The type first: `bool`, `uint(min, max)`, `str(Enum)`, `str_nolimit(Enum)`,
//! `bitfield(Enum[, allowed])`, `with(OPT_FN)`, `str_member(member_field)` -
//! a string or bitfield option's choices are its enum's Names. Then, each
//! optional: `flags`, the OPT_* when it may be set, lowercase; `sb`, its
//! superblock field's getter, `struct::getter`, its setter set_getter, or
//! `struct::getter / setter`; `default`; `hint`.
//!
//! It makes, from the fields in order - each one's id, Opt_name:
//!
//! ```text
//! bch_opt_id               the ids, and bch2_opts_nr - a c_enum!, for C too
//! bch_opts::TABLE          each option's Opt (opts.rs), by id
//! bch_opts::DEFAULTS       each one's default, all defined
//! get(), set(), defined(), apply()   by id; set() sets it defined, in the
//!                          field `defined`, a bch_opts_mask
//! name(), set_name()       each option's value - a bool option's as bool, a
//!                          string option's as its enum - and setting it,
//!                          defined
//! name_or_default()        its value, or its default if it isn't defined
//! PublishedOpts            bch_opts laid out the same, each option a Volatile:
//!                          the filesystem's, c->opts, which anyone reads an
//!                          option at a time and its lock's holder writes -
//!                          the readers above, and load(), a bch_opts copy
//! PublishedOptsWriter      it, as its writer has it: the writers above
//! ```
//!
//! A field without #[opt] isn't an option: `defined`.

use proc_macro::{Delimiter, TokenStream, TokenTree};

use crate::tokens;

struct Field {
    name: String,
    ty:   String,
    opt:  Option<Vec<Vec<TokenTree>>>,
    doc:  Vec<String>,
}

/// @ts split at its top level commas.
fn split_commas(ts: TokenStream) -> Vec<Vec<TokenTree>> {
    let mut out = vec![Vec::new()];
    for t in ts {
        match &t {
            TokenTree::Punct(p) if p.as_char() == ',' => out.push(Vec::new()),
            _ => out.last_mut().unwrap().push(t),
        }
    }
    out.retain(|v| !v.is_empty());
    out
}

fn text(ts: &[TokenTree]) -> String {
    ts.iter().cloned().collect::<TokenStream>().to_string()
}

/// A doc comment's string literal, as the string it is.
fn unescape(lit: &str) -> Result<String, String> {
    let s = lit.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| format!("doc comment {lit}: not a plain string literal"))?;
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n')  => out.push('\n'),
            Some('t')  => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"')  => out.push('"'),
            Some('\'') => out.push('\''),
            other => return Err(format!("doc comment {lit}: escape \\{other:?}")),
        }
    }
    Ok(out)
}

fn parse_fields(input: TokenStream) -> Result<(String, Vec<Field>), String> {
    let mut trees = input.into_iter().peekable();
    let name = loop {
        match trees.next() {
            Some(TokenTree::Ident(i)) if i.to_string() == "struct" => match trees.next() {
                Some(TokenTree::Ident(n)) => break n.to_string(),
                _ => return Err("expected the struct's name".into()),
            },
            Some(_) => {}
            None => return Err("for a struct".into()),
        }
    };
    let body = trees.find_map(|t| match t {
        TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => Some(g.stream()),
        _ => None,
    }).ok_or("a struct with named fields")?;

    let mut fields = Vec::new();
    for f in split_commas(body) {
        let mut opt = None;
        let mut doc = Vec::new();
        let mut i = 0;
        while let (Some(TokenTree::Punct(p)), Some(TokenTree::Group(g))) = (f.get(i), f.get(i + 1)) {
            if p.as_char() != '#' || g.delimiter() != Delimiter::Bracket {
                break;
            }
            let attr: Vec<TokenTree> = g.stream().into_iter().collect();
            match attr.as_slice() {
                [TokenTree::Ident(n), TokenTree::Group(args)] if n.to_string() == "opt" =>
                    opt = Some(split_commas(args.stream())),
                [TokenTree::Ident(n), TokenTree::Punct(eq), TokenTree::Literal(l)]
                    if n.to_string() == "doc" && eq.as_char() == '=' =>
                    doc.push(unescape(&l.to_string())?),
                _ => {}
            }
            i += 2;
        }
        // past the visibility, to `name: type`
        let rest = &f[i..];
        let colon = rest.iter().position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ':'))
            .ok_or("a field without a type")?;
        let Some(TokenTree::Ident(n)) = rest.get(colon - 1) else {
            return Err("a field without a name".into());
        };
        fields.push(Field { name: n.to_string(), ty: text(&rest[colon + 1..]), opt, doc });
    }
    Ok((name, fields))
}

/// An option's Opt, and what its set and get are.
fn option(f: &Field, args: &[Vec<TokenTree>]) -> Result<String, String> {
    let name = &f.name;
    let err = |e: &str| format!("{name}: #[opt]: {e}");
    let o = "crate::opts";

    // the type: kind(args), or a bare kind
    let (kind, kargs) = match args.first().map(Vec::as_slice) {
        Some([TokenTree::Ident(k)]) => (k.to_string(), Vec::new()),
        Some([TokenTree::Ident(k), TokenTree::Group(g)]) if g.delimiter() == Delimiter::Parenthesis =>
            (k.to_string(), split_commas(g.stream()).iter().map(|a| text(a)).collect()),
        _ => return Err(err("the type first: bool, uint(min, max), str(Enum), ...")),
    };
    let names = |t: &str| format!("crate::names_by_value!(c::{t})");
    let (ty, min, max) = match (kind.as_str(), kargs.as_slice()) {
        ("bool", []) => (format!("{o}::OptType::Bool"), "0".into(), "1".into()),
        ("uint", [min, max]) => (format!("{o}::OptType::Uint"), format!("({min}) as u64"), format!("({max}) as u64")),
        ("str", [t]) => (format!("{o}::OptType::Str({})", names(t)), "0".into(),
                         format!("({}.len() - 1) as u64", names(t))),
        ("str_nolimit", [t]) => (format!("{o}::OptType::Str({})", names(t)), "0".into(), "u64::MAX".into()),
        ("bitfield", [t]) => (format!("{o}::OptType::Bitfield {{ choices: {}, allowed: u64::MAX }}", names(t)),
                              "0".into(), "0".into()),
        ("bitfield", [t, allowed]) => (format!("{o}::OptType::Bitfield {{ choices: {}, allowed: ({allowed}) as u64 }}",
                                               names(t)), "0".into(), "0".into()),
        ("with", [f]) => (format!("{o}::OptType::Fn(&{f})"), "0".into(), "0".into()),
        ("str_member", [m]) => (format!("{o}::OptType::StrMember {{\n\
                                            get: |m| &m.{m},\n\
                                            get_mut: |m| &mut m.{m},\n\
                                            size: {o}::size_of_field::<c::bch_member, _>(|m| &m.{m}),\n\
                                         }}"), "0".into(), "0".into()),
        _ => return Err(err(&format!("{kind}({}): not an option type", kargs.join(", ")))),
    };

    let mut flags = "0".to_string();
    let mut sb = "None".to_string();
    let mut hint = "None".to_string();
    for a in &args[1..] {
        let [TokenTree::Ident(k), TokenTree::Punct(eq), v @ ..] = a.as_slice() else {
            return Err(err(&format!("{}: expected key = value", text(a))));
        };
        if eq.as_char() != '=' || v.is_empty() {
            return Err(err(&format!("{}: expected key = value", text(a))));
        }
        match k.to_string().as_str() {
            "flags" => {
                let fl: Result<Vec<String>, String> = v.split(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == '|'))
                    .map(|f| match f {
                        [TokenTree::Ident(f)] => Ok(format!("OPT_{}", f.to_string().to_uppercase())),
                        f => Err(err(&format!("flags: {}: not a flag", text(f)))),
                    })
                    .collect();
                flags = fl?.join(" | ");
            }
            "sb" => {
                // host::getter [/ setter]
                let (get, set) = match v.iter().position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == '/')) {
                    Some(i) => (&v[..i], Some(text(&v[i + 1..]))),
                    None    => (v, None),
                };
                let [TokenTree::Ident(host), TokenTree::Punct(_), TokenTree::Punct(_), TokenTree::Ident(getter)] = get else {
                    return Err(err(&format!("sb = {}: expected struct::getter", text(v))));
                };
                let host = match host.to_string().as_str() {
                    // the head: what either state of a superblock has its fields on
                    "bch_sb" => "bch_sb__head".to_string(),
                    h        => h.to_string(),
                };
                let set = set.unwrap_or_else(|| format!("set_{getter}"));
                sb = format!("Some(&{o}::Field::<c::{host}, _> {{ get: |t| t.{getter}(), set: |t, v| t.{set}(v) }} \
                              as &dyn {o}::SbOpt)");
            }
            "default" => {}
            "hint" => hint = format!("Some({})", text(v)),
            k => return Err(err(&format!("{k}: not an #[opt] key"))),
        }
    }

    let help = match f.doc.is_empty() {
        true  => "None".to_string(),
        false => {
            let lines: Vec<&str> = f.doc.iter().map(|l| l.strip_prefix(' ').unwrap_or(l)).collect();
            format!("Some({:?})", lines.join("\n"))
        }
    };

    Ok(format!(
        "{o}::Opt {{\n\
             id:    bch_opt_id::Opt_{name},\n\
             name:  {name:?},\n\
             flags: c::opt_flags::from_bits_retain({flags}),\n\
             ty:    {ty},\n\
             min:   {min},\n\
             max:   {max},\n\
             sb:    {sb},\n\
             hint:  {hint},\n\
             help:  {help},\n\
         }}"))
}

enum Accessor {
    Bool,
    /// A string option's choices' enum, through c_enum!'s from_bits() and
    /// into_bits(): a closed enum's from_bits() panics on a value that isn't
    /// one, and a string option's value is an index into its choices - so
    /// one, unless something set it around the parser.
    Enum(String),
    Field,
}

/// What an option's accessors take and give: a bool option's a bool, a
/// string option's its enum, the rest the field's own type.
fn accessor_type(args: &[Vec<TokenTree>]) -> Accessor {
    match args.first().map(Vec::as_slice) {
        Some([TokenTree::Ident(k)]) if k.to_string() == "bool" => Accessor::Bool,
        Some([TokenTree::Ident(k), TokenTree::Group(g)])
            if matches!(k.to_string().as_str(), "str" | "str_nolimit") =>
            Accessor::Enum(text(&g.stream().into_iter().collect::<Vec<_>>())),
        _ => Accessor::Field,
    }
}

/// What option @f's accessors take and give; that from the field's value, an
/// expression @x; and the field's value from one of it, `v`.
fn accessor(f: &Field, args: &[Vec<TokenTree>]) -> (String, Box<dyn Fn(&str) -> String>, String) {
    let field_ty = f.ty.clone();
    match accessor_type(args) {
        Accessor::Bool    => ("bool".into(), Box::new(|x| format!("{x} != 0")), format!("v as {field_ty}")),
        Accessor::Enum(e) => (format!("c::{e}"), Box::new(move |x| format!("c::{e}::from_bits({x} as _)")),
                              format!("v.into_bits() as {field_ty}")),
        Accessor::Field   => (field_ty, Box::new(|x| x.to_string()), "v".into()),
    }
}

/// The option's default, from its #[opt]: 0 without one.
fn default(f: &Field, args: &[Vec<TokenTree>]) -> String {
    args[1..].iter()
        .find_map(|a| match a.as_slice() {
            [TokenTree::Ident(k), TokenTree::Punct(_), v @ ..] if k.to_string() == "default" => Some(text(v)),
            _ => None,
        })
        .map_or_else(|| "0".to_string(), |d| format!("({d}) as {}", f.ty))
}

pub(crate) fn derive(input: TokenStream) -> Result<TokenStream, String> {
    let (name, fields) = parse_fields(input)?;
    let opts: Vec<(&Field, &Vec<Vec<TokenTree>>)> = fields.iter()
        .filter_map(|f| f.opt.as_ref().map(|o| (f, o)))
        .collect();

    let ids: String = opts.iter().map(|(f, _)| format!("#[label(\"{0}\")] Opt_{0},\n", f.name)).collect();
    let table: Result<Vec<String>, String> = opts.iter().map(|(f, a)| option(f, a)).collect();
    let table = table?.join(",\n");
    let defaults: String = opts.iter().map(|(f, a)| format!("opts.{} = {};\n", f.name, default(f, a))).collect();

    let arms = |body: &dyn Fn(&Field) -> String| -> String {
        opts.iter().map(|(f, _)| format!("bch_opt_id::Opt_{} => {},\n", f.name, body(f))).collect()
    };

    // What reads and writes a value of each option, generic over how a
    // field's loaded and stored: the plain struct's fields, or its published
    // mirror's Volatiles. @load(n) is field n's value; @store(n, v) sets it;
    // @defined(id) and @define(id) test and set the defined bit of option id,
    // an expression.
    struct Place<'a> {
        load:    &'a dyn Fn(&str) -> String,
        store:   &'a dyn Fn(&str, &str) -> String,
        defined: &'a dyn Fn(&str) -> String,
        define:  &'a dyn Fn(&str) -> String,
        recv:    &'a str,
    }

    let readers = |p: &Place| -> String {
        let get = arms(&|f| format!("{} as u64", (p.load)(&f.name)));
        let accessors: String = opts.iter().map(|(f, a)| {
            let (ty, conv, _) = accessor(f, a);
            let n = &f.name;
            format!("pub fn {n}(&self) -> {ty} {{ {} }}\n\
                     pub fn {n}_or_default(&self) -> {ty} {{\n\
                         if self.defined(bch_opt_id::Opt_{n}) {{ self.{n}() }} else {{ {} }}\n\
                     }}\n", conv(&(p.load)(n)), conv(&format!("{name}::DEFAULTS.{n}")))
        }).collect();
        format!("/// Its value for @id - defined or not.\n\
                 pub fn get(&self, id: bch_opt_id) -> u64 {{\n\
                     match id {{\n\
                         {get}\
                         _ => panic!(\"not an option: {{}}\", id.0),\n\
                     }}\n\
                 }}\n\
                 \n\
                 pub fn defined(&self, id: bch_opt_id) -> bool {{\n\
                     {}\n\
                 }}\n\
                 \n\
                 {accessors}", (p.defined)("id"))
    };

    let writers = |p: &Place| -> String {
        let set = arms(&|f| (p.store)(&f.name, &format!("v as {}", f.ty)));
        let apply: String = opts.iter().map(|(f, _)| format!(
            "if src.defined.test(bch_opt_id::Opt_{0}) {{\n\
                 {1};\n\
                 {2};\n\
             }}\n", f.name, (p.store)(&f.name, &format!("src.{}", f.name)),
            (p.define)(&format!("bch_opt_id::Opt_{}", f.name)))).collect();
        let setters: String = opts.iter().map(|(f, a)| {
            let (ty, _, back) = accessor(f, a);
            let n = &f.name;
            format!("pub fn set_{n}({}, v: {ty}) {{\n\
                         {};\n\
                         {};\n\
                     }}\n", p.recv, (p.store)(n, &back),
                     (p.define)(&format!("bch_opt_id::Opt_{n}")))
        }).collect();
        format!("/// Set, and defined.\n\
                 pub fn set({}, id: bch_opt_id, v: u64) {{\n\
                     match id {{\n\
                         {set}\
                         _ => panic!(\"not an option: {{}}\", id.0),\n\
                     }}\n\
                     {};\n\
                 }}\n\
                 \n\
                 /// Those @src defines, set: bch2_opts_apply().\n\
                 pub fn apply({}, src: &{name}) {{\n\
                     {apply}\
                 }}\n\
                 \n\
                 {setters}", p.recv, (p.define)("id"), p.recv)
    };

    let plain = Place {
        load:    &|n| format!("self.{n}"),
        store:   &|n, v| format!("self.{n} = {v}"),
        defined: &|id| format!("self.defined.test({id})"),
        define:  &|id| format!("self.defined.set({id})"),
        recv:    "&mut self",
    };
    let published = Place {
        load:    &|n| format!("self.{n}.get()"),
        store:   &|n, v| format!("self.{n}.set({v})"),
        defined: &|id| format!("self.defined.test({id}.0 as usize)"),
        define:  &|id| format!("self.defined.set({id}.0 as usize)"),
        recv:    "&self",
    };
    let (plain_readers, plain_writers) = (readers(&plain), writers(&plain));
    let (pub_readers, pub_writers) = (readers(&published), writers(&published));

    let mirror: String = opts.iter()
        .map(|(f, _)| format!("pub {}: crate::util::projections::Volatile<{}>,\n", f.name, f.ty))
        .collect();
    let load_fields: String = opts.iter().map(|(f, _)| format!("{0}: self.{0}.get(),\n", f.name)).collect();
    let nr = opts.len();

    Ok(tokens(&format!(
        "c_enum! {{\n\
             #[open]\n\
             pub enum bch_opt_id: u32 {{\n\
                 {ids}\
                 bch2_opts_nr,\n\
             }}\n\
         }}\n\
         \n\
         const _: () = assert!({nr} <= BCH_OPTS_MAX as usize, \"more options than BCH_OPTS_MAX\");\n\
         \n\
         impl {name} {{\n\
             /// Each option, by bch_opt_id.\n\
             pub const TABLE: [crate::opts::Opt; {nr}] = [{table}];\n\
         \n\
             /// Each option's default, all defined.\n\
             pub const DEFAULTS: Self = {{\n\
                 // SAFETY: integers, and the defined bits'\n\
                 let mut opts: Self = unsafe {{ core::mem::zeroed() }};\n\
                 {defaults}\
                 let mut i = 0;\n\
                 while i < {nr} {{\n\
                     opts.defined.set(bch_opt_id(i as u32));\n\
                     i += 1;\n\
                 }}\n\
                 opts\n\
             }};\n\
         \n\
             {plain_readers}\
             {plain_writers}\
         }}\n\
         \n\
         crate::published! {{\n\
             /// {name}, read an option at a time while its lock's holder writes it,\n\
             /// through PublishedOptsWriter. Laid out as {name} is: to C, one.\n\
             pub struct PublishedOpts {{\n\
                 {mirror}\
                 pub defined: AtomicOptsMask,\n\
             }}\n\
         }}\n\
         \n\
         // SAFETY: Volatile words: zeroes are options, none defined\n\
         unsafe impl ::pin_init::Zeroable for PublishedOpts {{}}\n\
         \n\
         // Field for field as {name}, each Volatile its type's size: checked\n\
         const _: () = {{\n\
             assert!(core::mem::size_of::<{name}>() == core::mem::size_of::<PublishedOpts>());\n\
             assert!(core::mem::align_of::<{name}>() == core::mem::align_of::<PublishedOpts>());\n\
         }};\n\
         \n\
         impl PublishedOpts {{\n\
             /// A copy: each option as it was when it was read, not all as of one\n\
             /// moment.\n\
             pub fn load(&self) -> {name} {{\n\
                 {name} {{\n\
                     {load_fields}\
                     defined: bch_opts_mask::load(&self.defined),\n\
                 }}\n\
             }}\n\
         \n\
             {pub_readers}\
         }}\n\
         \n\
         impl PublishedOptsWriter {{\n\
             {pub_writers}\
         }}\n")))
}
