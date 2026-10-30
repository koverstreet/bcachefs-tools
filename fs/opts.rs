// SPDX-License-Identifier: GPL-2.0
//! Options: mount options, the sysfs options/ files, format options, and the
//! filesystem defaults the superblock keeps - one list, bch_opts' fields
//! (opts_types.rs), and OPTS, the table #[derive(Options)] makes from them.
//!
//! Terms:
//! - An option, Opt: what its field's #[opt] says of one - its type and
//!   bounds, when it may be set (flags, OPT_*), where the superblock keeps
//!   it, if it does. OPTS is indexed by bch_opt_id.
//! - A value is a u64, whatever the option's type: a bool's 0 or 1, a string
//!   option's index into its choices, a bitfield's bits.
//! - bch_opts: a value for each option, and whether it's defined - mount
//!   options as parsed have only those given; c->opts has all of them.
//!   DEFAULTS is the #[opt]s' defaults, all defined. Each option's field
//!   has a getter, name(), a setter that defines it, set_name(), and
//!   name_or_default().
//! - Where the superblock keeps an option: a bitfield of the superblock
//!   itself, of its ext section, or of a member - its #[opt] names the
//!   field's getter, and OptHost finds the struct it's on in a superblock.
//!   The field may hold the value scaled: OPT_SB_FIELD_SECTORS (>> 9),
//!   _ILOG2, _ONE_BIAS (0 is unset).
//!
//! Changing an option on a running filesystem is an OptChange, and the
//! hooks either side of the change: opts/change.rs.
//!
//! Also here, as opts.c had them: the dirent types' names. The format's
//! enums' are theirs - util/names.rs.

use crate::c;
use crate::opt_id;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::inode_opts::InodeOpt;
use crate::sb::io::SbState;
use crate::prt;
use crate::util::vstructs::{bytes_of_mut, cast_plain_mut, plain_bytes_mut, Unvalidated};
use crate::sb::members;
use crate::util::ffi::c_char;
use crate::util::log::{log_no_fs, LogLevel};
use crate::util::names::{self, Names};
use crate::util::{HumanReadable, Printbuf};
use crate::util::sysfs::{parse_human_u64, parse_int};
use core::ffi::CStr;

pub const NR: usize = opt_id::bch2_opts_nr.0 as usize;

/// By bch_opt_id: bch_opts' #[opt]s.
pub static OPTS: [Opt; NR] = c::bch_opts::TABLE;

/// Each option's default, all defined.
pub const DEFAULTS: c::bch_opts = c::bch_opts::DEFAULTS;


// ── Names ────────────────────────────────────────────────────────────────

/// @t's name - "(invalid data type)" if it isn't one: bch2_data_type_str().
pub fn data_type_str(t: c::bch_data_type) -> &'static str {
    t.name().unwrap_or("(invalid data type)")
}

/// The dirent d_types' names, by DT_*.
pub const D_TYPES: [&str; c::BCH_DT_MAX as usize] = {
    let mut t = [""; c::BCH_DT_MAX as usize];
    t[c::DT_UNKNOWN as usize] = "unknown";
    t[c::DT_FIFO as usize]    = "fifo";
    t[c::DT_CHR as usize]     = "chr";
    t[c::DT_DIR as usize]     = "dir";
    t[c::DT_BLK as usize]     = "blk";
    t[c::DT_REG as usize]     = "reg";
    t[c::DT_LNK as usize]     = "lnk";
    t[c::DT_SOCK as usize]    = "sock";
    t[c::DT_WHT as usize]     = "whiteout";
    t[c::DT_SUBVOL as usize]  = "subvol";
    t
};

// ── The options ──────────────────────────────────────────────────────────

/// An option with a type of its own: OPT_FN().
pub struct OptFn {
    /// @val is None for an option given without one; Err(option_needs_open_fs)
    /// for one that can't be parsed without the filesystem.
    pub parse:    fn(fs: Option<&Fs>, val: Option<&[u8]>, err: Option<&mut Printbuf>) -> Result<u64, BchError>,
    /// Against @fs, or without one @sb: a target's, a label's names are
    /// theirs.
    pub to_text:  fn(out: &mut Printbuf, fs: Option<&Fs>, sb: Option<&c::bch_sb<Unvalidated>>, v: u64),
    pub validate: Option<fn(v: u64, err: Option<&mut Printbuf>) -> Result<(), BchError>>,
}

pub enum OptType {
    Bool,
    Uint,
    /// An index into its choices.
    Str(&'static [&'static str]),
    /// A bit per choice, of those @allowed.
    Bitfield { choices: &'static [&'static str], allowed: u64 },
    Fn(&'static OptFn),
    /// A string in a member's char[] field, which it's set and shown through:
    /// not a value.
    StrMember {
        get:     fn(&c::bch_member) -> &[u8],
        get_mut: fn(&mut c::bch_member) -> &mut [u8],
        size:    usize,
    },
}

pub(crate) const fn size_of_field<T, F>(_: fn(&T) -> &F) -> usize {
    size_of::<F>()
}

pub struct Opt {
    pub id:    opt_id,
    pub name:  &'static str,
    pub flags: c::opt_flags,
    pub ty:    OptType,
    /// Bounds, inclusive: max 0 is none.
    pub min:   u64,
    pub max:   u64,
    /// Its superblock field, if it has one.
    pub sb:    Option<&'static dyn SbOpt>,
    pub hint:  Option<&'static str>,
    pub help:  Option<&'static str>,
}

/// An option's superblock field, as stored - before the OPT_SB_FIELD_*
/// scaling.
pub trait SbOpt: Sync {
    /// A member's: a device option's.
    fn member(&self) -> bool;
    /// None if the struct it's on isn't there: no ext section, no member @dev.
    fn get(&self, sb: &c::bch_sb<Unvalidated>, dev: u32) -> Option<u64>;
    /// Whether it changed: None if the struct it's on isn't there.
    fn set(&self, sb: &mut c::bch_sb_buf, dev: u32, v: u64) -> Option<bool>;
}

/// What an option's superblock field is on, found in a superblock: the
/// superblock's head, its ext section, or member @dev. Read from one that
/// may not be valid - every superblock is an Unvalidated one - and written
/// in one that is.
pub trait OptHost: 'static {
    const MEMBER: bool;
    fn with<R>(sb: &c::bch_sb<Unvalidated>, dev: u32, f: impl FnOnce(&Self) -> R) -> Option<R>;
    fn with_mut<R>(sb: &mut c::bch_sb_buf, dev: u32, f: impl FnOnce(&mut Self) -> R) -> Option<R>;
}

impl OptHost for c::bch_sb__head {
    const MEMBER: bool = false;

    fn with<R>(sb: &c::bch_sb<Unvalidated>, _: u32, f: impl FnOnce(&Self) -> R) -> Option<R> {
        Some(f(Unvalidated::head(sb)))
    }

    fn with_mut<R>(sb: &mut c::bch_sb_buf, _: u32, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        cast_plain_mut(bytes_of_mut(sb.sb_mut())).map(f)
    }
}

/// Read, a copy: a section written before the struct grew has its missing
/// fields zero, as a validated one's arm reads.
impl OptHost for c::bch_sb_field_ext {
    const MEMBER: bool = false;

    fn with<R>(sb: &c::bch_sb<Unvalidated>, _: u32, f: impl FnOnce(&Self) -> R) -> Option<R> {
        let src = Unvalidated::field(sb, c::bch_sb_field_type::BCH_SB_FIELD_ext)?.as_bytes();
        let mut ext = Self::default();
        let dst = plain_bytes_mut(&mut ext);
        let n = src.len().min(dst.len());
        dst[..n].copy_from_slice(&src[..n]);
        Some(f(&ext))
    }

    fn with_mut<R>(sb: &mut c::bch_sb_buf, _: u32, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        sb.field_mut::<Self>().map(f)
    }
}

/// A member that's alive: bch2_member_exists().
impl OptHost for c::bch_member {
    const MEMBER: bool = true;

    fn with<R>(sb: &c::bch_sb<Unvalidated>, dev: u32, f: impl FnOnce(&Self) -> R) -> Option<R> {
        members::sb_member(sb, dev).filter(|m| m.alive()).map(|m| f(&m))
    }

    fn with_mut<R>(sb: &mut c::bch_sb_buf, dev: u32, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        members::sb_member(sb.sb(), dev).filter(|m| m.alive())?;
        sb.member_mut(dev).map(f)
    }
}

/// A one bit field's accessors take a bool, the rest a u64.
pub trait FieldValue: Copy + 'static {
    fn to_u64(self) -> u64;
    /// As a setter's mask truncates it.
    fn from_u64(v: u64) -> Self;
}

impl FieldValue for u64 {
    fn to_u64(self) -> u64 { self }
    fn from_u64(v: u64) -> Self { v }
}

impl FieldValue for bool {
    fn to_u64(self) -> u64 { self as u64 }
    fn from_u64(v: u64) -> Self { v & 1 != 0 }
}

/// A superblock field, by its accessors - c_bitmask!'s.
pub struct Field<T: ?Sized, V> {
    pub get: fn(&T) -> V,
    pub set: fn(&mut T, V),
}

impl<T: OptHost + ?Sized, V: FieldValue> SbOpt for Field<T, V> {
    fn member(&self) -> bool {
        T::MEMBER
    }

    fn get(&self, sb: &c::bch_sb<Unvalidated>, dev: u32) -> Option<u64> {
        T::with(sb, dev, |t| (self.get)(t).to_u64())
    }

    fn set(&self, sb: &mut c::bch_sb_buf, dev: u32, v: u64) -> Option<bool> {
        T::with_mut(sb, dev, |t| {
            let changed = v != (self.get)(t).to_u64();
            (self.set)(t, V::from_u64(v));
            changed
        })
    }
}

/// fix_errors: given without a value, yes.
pub(crate) const FIX_ERRORS: OptFn = OptFn {
    parse: |_, val, err| {
        let Some(val) = val else {
            return Ok(c::FSCK_FIX_yes.into());
        };
        let choices: &[&str] = crate::names_by_value!(c::fsck_err_opts);
        choices.iter().position(|n| n.as_bytes() == val).map(|v| v as u64).ok_or_else(|| {
            if let Some(err) = err {
                write!(err, "fix_errors: invalid selection");
            }
            c::EINVAL.into()
        })
    },
    to_text: |out, _, _, v| names::prt_name(out, c::fsck_err_opts::ENTRIES, v),
    validate: None,
};

impl opt_id {
    pub fn opt(self) -> &'static Opt {
        &OPTS[self.0 as usize]
    }

    pub fn inode_opt(self) -> Option<InodeOpt> {
        InodeOpt::from_opt_id(self)
    }
}

impl Opt {
    pub fn has(&self, flag: c::opt_flags) -> bool {
        self.flags.intersects(flag)
    }

    /// Whether the superblock has it: a field, or a member's string.
    pub fn in_sb(&self) -> bool {
        self.sb.is_some() || matches!(self.ty, OptType::StrMember { .. })
    }

    /// Its superblock field, if it's the superblock's or the ext section's:
    /// a filesystem default.
    pub fn fs_field(&self) -> Option<&'static dyn SbOpt> {
        self.sb.filter(|f| !f.member())
    }

    /// Its superblock field, if it's a member's.
    pub fn member_field(&self) -> Option<&'static dyn SbOpt> {
        self.sb.filter(|f| f.member())
    }

    /// The choices of a string or bitfield option.
    pub fn choices(&self) -> &'static [&'static str] {
        match self.ty {
            OptType::Str(c) | OptType::Bitfield { choices: c, .. } => c,
            _ => &[],
        }
    }

    /// Its value from its superblock field's: the field may hold it scaled.
    pub fn from_sb_field(&self, v: u64) -> u64 {
        let mut v = v;
        if self.has(c::opt_flags::OPT_SB_FIELD_ONE_BIAS) {
            v = v.wrapping_sub(1);
        }
        if self.has(c::opt_flags::OPT_SB_FIELD_ILOG2) {
            v = 1u64.checked_shl(v as u32).unwrap_or(0);
        }
        if self.has(c::opt_flags::OPT_SB_FIELD_SECTORS) {
            v <<= 9;
        }
        v
    }

    /// Its superblock field's value for @v: from_sb_field()'s inverse.
    pub fn to_sb_field(&self, v: u64) -> u64 {
        let mut v = v;
        if self.has(c::opt_flags::OPT_SB_FIELD_SECTORS) {
            v >>= 9;
        }
        if self.has(c::opt_flags::OPT_SB_FIELD_ILOG2) {
            v = v.checked_ilog2().unwrap_or(0).into();
        }
        if self.has(c::opt_flags::OPT_SB_FIELD_ONE_BIAS) {
            v = v.wrapping_add(1);
        }
        v
    }

    /// A BCH_OPT_STR_MEMBER option's value: the string in member @m's field,
    /// up to a NUL.
    pub fn member_str<'a>(&self, m: &'a c::bch_member) -> &'a [u8] {
        let OptType::StrMember { get, .. } = self.ty else {
            panic!("{}: not a member string option", self.name);
        };
        get(m).split(|&b| b == 0).next().unwrap_or_default()
    }
}

// ── bch_opts ─────────────────────────────────────────────────────────────

impl c::bch_opts_mask {
    pub const fn test(&self, id: opt_id) -> bool {
        let (i, bits) = (id.0 as usize, core::ffi::c_ulong::BITS as usize);
        self.d[i / bits] >> (i % bits) & 1 != 0
    }

    pub const fn set(&mut self, id: opt_id) {
        let (i, bits) = (id.0 as usize, core::ffi::c_ulong::BITS as usize);
        self.d[i / bits] |= 1 << (i % bits);
    }

    /// A copy of @mask, a word at a time.
    pub fn load(mask: &c::AtomicOptsMask) -> Self {
        let words = mask.load();
        Self { d: core::array::from_fn(|i| words.words()[i] as _) }
    }
}

// ── Parsing ──────────────────────────────────────────────────────────────

pub fn opt_lookup(name: &str) -> Option<&'static Opt> {
    OPTS.iter().find(|o| o.name == name)
}

/// As a mount option names it: "quota" is usrquota.
fn mount_opt_lookup(name: &str) -> Option<&'static Opt> {
    opt_lookup(match name {
        "quota" => "usrquota",
        n       => n,
    })
}

/// Values mount options spell differently: degraded's bools.
fn opt_val_synonym<'a>(name: &str, val: &'a [u8]) -> &'a [u8] {
    match (name, val) {
        ("degraded", b"true" | b"1")  => b"yes",
        ("degraded", b"false" | b"0") => b"no",
        _ => val,
    }
}

/// @opt's name and @msg, into @err if it's given.
fn complain(err: Option<&mut Printbuf>, opt: &Opt, msg: core::fmt::Arguments<'_>) {
    if let Some(err) = err {
        write!(err, "{}: {msg}", opt.name);
    }
}

/// Whether @v is within @opt's bounds, and what its flags and type require.
pub fn opt_validate(opt: &Opt, v: u64, mut err: Option<&mut Printbuf>) -> Result<(), BchError> {
    macro_rules! fail {
        ($e:expr, $($msg:tt)*) => {{
            complain(err.as_deref_mut(), opt, format_args!($($msg)*));
            return Err($e.into());
        }};
    }

    if v < opt.min {
        fail!(bch_errcode::BCH_ERR_ERANGE_option_too_small, "too small (min {})", opt.min);
    }
    if opt.max != 0 && v > opt.max {
        fail!(bch_errcode::BCH_ERR_ERANGE_option_too_big, "too big (max {})", opt.max);
    }
    if opt.has(c::opt_flags::OPT_SB_FIELD_SECTORS) && v & 511 != 0 {
        fail!(bch_errcode::BCH_ERR_opt_parse_error, "not a multiple of 512");
    }
    if opt.has(c::opt_flags::OPT_MUST_BE_POW_2) && !v.is_power_of_two() {
        fail!(bch_errcode::BCH_ERR_opt_parse_error, "must be a power of two");
    }
    match opt.ty {
        OptType::Fn(OptFn { validate: Some(validate), .. }) => validate(v, err),
        _ => Ok(()),
    }
}

/// A bitfield option's value: the choices named, comma or semicolon
/// separated - or with a leading '-', those allowed but not named.
/// Err(a name that isn't one of the choices, or isn't allowed).
pub fn read_flag_list<'a>(val: &'a [u8], choices: &[&str], allowed: u64) -> Result<u64, &'a [u8]> {
    let s = val.trim_ascii();
    let (invert, s) = match s.strip_prefix(b"-") {
        Some(s) => (true, s),
        None    => (false, s),
    };

    let mut v = 0;
    for name in s.split(|&b| b == b',' || b == b';') {
        let bit = choices.iter().position(|c| c.as_bytes() == name).ok_or(name)?;
        if allowed & 1 << bit == 0 {
            return Err(name);
        }
        v |= 1 << bit;
    }

    if invert {
        let all = if choices.len() >= 64 { !0 } else { (1 << choices.len()) - 1 };
        v = !v & all & allowed;
    }
    Ok(v)
}

/// @val as an @opt value: @fs enables options that need one.
pub fn opt_parse(fs: Option<&Fs>, opt: &Opt, val: Option<&[u8]>, mut err: Option<&mut Printbuf>)
    -> Result<u64, BchError>
{
    if let Some(err) = err.as_deref_mut() {
        err.indent_add_nextline(2);
    }
    macro_rules! fail {
        ($e:expr, $($msg:tt)*) => {{
            complain(err.as_deref_mut(), opt, format_args!($($msg)*));
            return Err($e.into());
        }};
    }

    let v = match opt.ty {
        OptType::Bool => match val.unwrap_or(b"1") {
            b"1" | b"true" | b"yes" => 1,
            b"0" | b"false" | b"no" => 0,
            _ => fail!(bch_errcode::BCH_ERR_option_not_bool, "must be bool"),
        },
        OptType::Uint => {
            let Some(val) = val else {
                fail!(bch_errcode::BCH_ERR_EINVAL_opt_parse_uint_required, "required value");
            };
            if val.starts_with(b"-") {
                fail!(bch_errcode::BCH_ERR_option_negative, "must be a non-negative number");
            }
            let v = core::str::from_utf8(val)
                .map_err(|_| BchError::from(c::EINVAL))
                .and_then(|s| if opt.has(c::opt_flags::OPT_HUMAN_READABLE) {
                    parse_human_u64(s)
                } else {
                    parse_int(s)
                });
            match v {
                Ok(v)  => v,
                Err(e) => fail!(e, "must be a number"),
            }
        }
        OptType::Str(choices) => {
            let Some(val) = val else {
                fail!(bch_errcode::BCH_ERR_EINVAL_opt_parse_str_required, "required value");
            };
            match choices.iter().position(|c| c.as_bytes() == val) {
                Some(i) => i as u64,
                None    => fail!(c::EINVAL, "invalid selection"),
            }
        }
        OptType::Bitfield { choices, allowed } => {
            let Some(val) = val else {
                fail!(bch_errcode::BCH_ERR_EINVAL_opt_parse_str_required, "required value");
            };
            match read_flag_list(val, choices, allowed) {
                Ok(v)    => v,
                Err(bad) => fail!(bch_errcode::BCH_ERR_opt_parse_error, "invalid flag {}",
                                  core::str::from_utf8(bad).unwrap_or("(not UTF-8)")),
            }
        }
        OptType::StrMember { size, .. } => {
            let Some(val) = val else {
                fail!(bch_errcode::BCH_ERR_EINVAL_opt_parse_str_required, "required value");
            };
            if val.len() > size {
                fail!(c::ENAMETOOLONG, "too long (max {size} bytes)");
            }
            // Set in the member directly, by opt_set_sb()
            0
        }
        OptType::Fn(f) => match (f.parse)(fs, val, err.as_deref_mut()) {
            Ok(v) => v,
            Err(e) if e == bch_errcode::BCH_ERR_option_needs_open_fs.into() => return Err(e),
            Err(e) => fail!(e, "parse error"),
        },
    };

    opt_validate(opt, v, err)?;
    Ok(v)
}

/// One mount option, @name=@val, into @opts if it's given: an unknown
/// option is skipped, and one that can't be parsed before the filesystem is
/// open goes into @parse_later, if it's given, to parse then.
pub fn parse_one_mount_opt(fs: Option<&Fs>, opts: Option<&mut c::bch_opts>,
                           parse_later: Option<&mut Printbuf>, name: &[u8], val: Option<&[u8]>,
                           err: &mut Printbuf) -> Result<(), BchError> {
    let name_str = core::str::from_utf8(name).ok();
    let mut opt = name_str.and_then(mount_opt_lookup);
    let mut val = val;

    // "noopt", a bool's negation
    if opt.is_none() && val.is_none() {
        if let Some(n) = name_str.and_then(|n| n.strip_prefix("no")) {
            opt = mount_opt_lookup(n);
            val = Some(b"0");
        }
    }

    let Some(opt) = opt else {
        return Ok(());
    };

    // a value for the synonyms - but OPT_FN is weird
    if val.is_none() && !matches!(opt.ty, OptType::Fn(_)) {
        val = Some(b"1");
    }
    let val = val.map(|v| opt_val_synonym(name_str.unwrap_or_default(), v));

    if !opt.has(c::opt_flags::OPT_MOUNT | c::opt_flags::OPT_MOUNT_OLD) {
        return Err(bch_errcode::BCH_ERR_option_name.into());
    }

    if (opt.id == opt_id::Opt_usrquota || opt.id == opt_id::Opt_grpquota) && !cfg!(CONFIG_BCACHEFS_QUOTA) {
        return Err(bch_errcode::BCH_ERR_option_name.into());
    }

    let v = match opt_parse(fs, opt, val, Some(err)) {
        Err(e) if e == bch_errcode::BCH_ERR_option_needs_open_fs.into() => {
            if let Some(later) = parse_later {
                later.write_bytes(name);
                write!(later, "=");
                later.write_bytes(val.unwrap_or_default());
                write!(later, ",");
                if later.allocation_failure() {
                    return Err(c::ENOMEM.into());
                }
            }
            return Ok(());
        }
        r => r?,
    };

    if opt.has(c::opt_flags::OPT_MOUNT_OLD) {
        log_no_fs(LogLevel::ERR, crate::message!(
            "option {} may no longer be specified at mount time; set via sysfs opts dir", opt.name));
        return Ok(());
    }

    if let Some(opts) = opts {
        opts.set(opt.id, v);
    }
    Ok(())
}

/// A mount option string, comma separated, into @opts: what can only be
/// parsed with the filesystem open into @parse_later, as
/// parse_one_mount_opt().
pub fn parse_mount_opts_into(fs: Option<&Fs>, opts: &mut c::bch_opts, mut parse_later: Option<&mut Printbuf>,
                             options: &[u8], ignore_unknown: bool) -> Result<(), BchError> {
    // sys_fsconfig() occasionally hands us a list starting with a comma
    let options = options.strip_prefix(b",").unwrap_or(options);

    for opt in options.split(|&b| b == b',').filter(|o| !o.is_empty()) {
        let (name, val) = match opt.iter().position(|&b| b == b'=') {
            Some(i) => (&opt[..i], Some(&opt[i + 1..])),
            None    => (opt, None),
        };

        let mut err = Printbuf::new();
        match parse_one_mount_opt(fs, Some(opts), parse_later.as_deref_mut(), name, val, &mut err) {
            Err(e) if e == bch_errcode::BCH_ERR_option_name.into() && ignore_unknown => {}
            Err(e) => {
                log_no_fs(LogLevel::ERR, crate::message!("Error parsing option {}", err.as_str()));
                return Err(e);
            }
            Ok(()) => {}
        }
    }
    Ok(())
}

/// @optstr parsed into fresh options: what can only be parsed on a running
/// filesystem too, if @fs is given.
pub fn parse_mount_opts_cstr(fs: Option<&Fs>, optstr: &CStr, ignore_unknown: bool)
        -> Result<c::bch_opts, BchError> {
    let mut opts = Default::default();
    parse_mount_opts_into(fs, &mut opts, None, optstr.to_bytes(), ignore_unknown)?;
    Ok(opts)
}

pub fn parse_mount_opts(fs: Option<&Fs>, optstr: Option<&str>, ignore_unknown: bool)
        -> Result<c::bch_opts, BchError> {
    let mut opts = Default::default();
    if let Some(s) = optstr {
        parse_mount_opts_into(fs, &mut opts, None, s.as_bytes(), ignore_unknown)?;
    }
    Ok(opts)
}

#[cfg(feature = "std")]
pub fn parse_mount_opts_vec(opts: &[String], ignore_unknown: bool) -> Result<c::bch_opts, BchError> {
    parse_mount_opts(None, Some(&opts.join(",")), ignore_unknown)
}

// ── Printing ─────────────────────────────────────────────────────────────

/// @flags: OPT_SHOW_*. Against @fs, or without one @sb: a target's, a
/// label's names are theirs.
#[cold]
pub fn opt_to_text(out: &mut Printbuf, fs: Option<&Fs>, sb: Option<&c::bch_sb<Unvalidated>>, opt: &Opt, v: u64,
                   flags: u32) {
    if flags & c::OPT_SHOW_MOUNT_STYLE != 0 {
        if let OptType::Bool = opt.ty {
            write!(out, "{}{}", if v != 0 { "" } else { "no" }, opt.name);
            return;
        }
        write!(out, "{}=", opt.name);
    }

    match opt.ty {
        OptType::Bool | OptType::Uint if opt.has(c::opt_flags::OPT_HUMAN_READABLE) => prt!(out, "{}", HumanReadable(v)),
        OptType::Bool | OptType::Uint => write!(out, "{}", v as i64),
        OptType::Str(choices) => match choices.get(v as usize) {
            Some(_) if v < opt.min || v > opt.max => write!(out, "(invalid option {})", v as i64),
            Some(_) if flags & c::OPT_SHOW_FULL_LIST != 0 => {
                for (i, c) in choices.iter().enumerate() {
                    if i as u64 == v {
                        write!(out, "[{c}] ");
                    } else {
                        write!(out, "{c} ");
                    }
                }
            }
            Some(c) => write!(out, "{c}"),
            None    => write!(out, "(invalid option {})", v as i64),
        },
        OptType::Bitfield { choices, .. } => out.bitflags(choices, v),
        OptType::Fn(f) => (f.to_text)(out, fs, sb, v),
        // Shown per device, from the member: bch2_member_to_text()
        OptType::StrMember { .. } => {}
    }
}

/// The options in @opts that aren't the defaults, comma separated: those
/// flagged with any of @show_mask and none of @hide_mask, and of @mask if
/// it's given.
#[cold]
pub fn opts_to_text(out: &mut Printbuf, opts: &c::bch_opts, fs: Option<&Fs>, sb: Option<&c::bch_sb<Unvalidated>>,
                    mask: Option<&c::bch_opts_mask>, show_mask: u32, hide_mask: u32, flags: u32) {
    let mut first = true;

    for opt in &OPTS {
        let f = opt.flags.bits();
        if f & hide_mask != 0 || f & show_mask == 0 {
            continue;
        }
        if mask.is_some_and(|m| !m.test(opt.id)) {
            continue;
        }

        let v = opts.get(opt.id);
        if v == DEFAULTS.get(opt.id) {
            continue;
        }

        if !first {
            write!(out, ",");
        }
        first = false;

        opt_to_text(out, fs, sb, opt, v, flags);
    }
}

// ── The superblock ───────────────────────────────────────────────────────

fn warn_no_member(opt: &Opt, dev: u32) {
    log_no_fs(LogLevel::WARN, crate::message!(
        "tried to set device option {} on nonexistent device {dev}", opt.name));
    crate::warn_on!(true);
}

/// Option @id as the superblock has it - a device option, as member @dev's:
/// 0 if it doesn't.
pub fn opt_from_sb(sb: &c::bch_sb<Unvalidated>, id: opt_id, dev: Option<u32>) -> u64 {
    let opt = id.opt();
    let v = match dev {
        None => opt.fs_field().and_then(|f| f.get(sb, 0)).unwrap_or(0),
        Some(dev) => match opt.member_field().and_then(|f| f.get(sb, dev)) {
            Some(v) => v,
            None => {
                warn_no_member(opt, dev);
                return 0;
            }
        },
    };
    opt.from_sb_field(v)
}

/// The filesystem defaults @sb has, into @opts - every one it has,
/// defined.
pub fn opts_from_sb(opts: &mut c::bch_opts, sb: &c::bch_sb<Unvalidated>) {
    for opt in OPTS.iter().filter(|o| o.fs_field().is_some()) {
        opts.set(opt.id, opt_from_sb(sb, opt.id, None));
    }
}

/// Whether the filesystem defaults @sb has are valid, as options: an
/// invalid one said in @out.
pub fn opts_validate_sb(sb: &c::bch_sb<Unvalidated>, out: &mut Printbuf) -> Result<(), BchError> {
    for opt in OPTS.iter().filter(|o| o.fs_field().is_some()) {
        let mut err = Printbuf::new();
        if let Err(e) = opt_validate(opt, opt_from_sb(sb, opt.id, None), Some(&mut err)) {
            writeln!(out, "Invalid superblock option {}", err.as_str());
            return Err(e);
        }
    }
    Ok(())
}

/// Set @opt in @sb, to @v - for a device option in member @dev's - @val its
/// string for a member string option: whether it changed.
pub fn opt_set_sb(sb: &mut c::bch_sb_buf, dev: Option<u32>, opt: &Opt, v: u64, val: Option<&[u8]>) -> bool {
    let v = opt.to_sb_field(v);
    let mut changed = false;

    if let (true, None, Some(f)) = (opt.has(c::opt_flags::OPT_FS), dev, opt.fs_field()) {
        changed = f.set(sb, 0, v).unwrap_or(false);
    }

    if let (true, Some(dev), Some(f)) = (opt.has(c::opt_flags::OPT_DEVICE), dev, opt.member_field()) {
        let Some(c) = f.set(sb, dev, v) else {
            warn_no_member(opt, dev);
            return false;
        };
        changed = c;

        if opt.id == opt_id::Opt_rotational {
            let m = sb.member_mut(dev).expect("set above");
            changed |= !m.rotational_set();
            m.set_rotational_set(true);
        }
    }

    if let (OptType::StrMember { get_mut, .. }, true, Some(dev)) = (&opt.ty, opt.has(c::opt_flags::OPT_DEVICE), dev) {
        let m = members::sb_member(sb.sb(), dev).filter(|m| m.alive()).and_then(|_| sb.member_mut(dev));
        let Some(m) = m else {
            warn_no_member(opt, dev);
            return false;
        };
        let val = val.unwrap_or_default();
        // zero padded, so equal strings compare equal in the interning
        for (i, b) in get_mut(m).iter_mut().enumerate() {
            let want = val.get(i).copied().unwrap_or(0);
            changed |= *b != want;
            *b = want;
        }
    }

    changed
}

impl Fs {
    /// Set @opt in the superblock - in device @ca's member for a device
    /// option - @val its string for a member string option, and write it if
    /// that changed it: whether it did.
    pub fn opt_set_sb(&self, ca: Option<&c::bch_dev>, opt: &Opt, v: u64, val: Option<&[u8]>) -> bool {
        let mut sb = self.sb_lock();
        let changed = opt_set_sb(&mut sb, ca.map(|ca| ca.dev_idx.into()), opt, v, val);
        if changed {
            self.write_super(&mut sb);
        }
        changed
    }
}

mod change;
pub use change::OptChange;

#[path = "opts_c_export.rs"]
pub mod c_export;
