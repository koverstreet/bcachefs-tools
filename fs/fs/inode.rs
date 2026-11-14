// SPDX-License-Identifier: GPL-2.0

//! Inodes, what was inode.c: the on-disk formats and the unpacked form
//! everything else works with; lookups and writes; validate and to_text;
//! inode number allocation; the trigger; deleting inodes, from the VFS and at
//! recovery; and starting new ones.
//!
//! An inode is a key at (0, inum, snapshot) in the inodes btree, in one of
//! three versions. v1 packs its fields in its own big-endian encoding - or,
//! with INODEv1_NEW_VARINT, as varints; v2 as varints, with a journal_seq and
//! a 64 bit flags field; v3 moves size, sectors and version into fixed
//! fields, and the mode into the flags field. pack() writes only v3, so the
//! older versions go as their inodes are rewritten.
//!
//! Unpacking doesn't fail. The unpacked inode starts zeroed; a field that
//! doesn't decode is reported, and it and the fields after it stay zero,
//! with recovery passes scheduled for the fields a zero can't be left in.
//! The report is the key and what did unpack - never the value's to_text,
//! which unpacks it again.
//!
//! The flags field is more than flags: from bit 20 it holds the str_hash
//! type and the number of packed fields, and in v3 where they start and the
//! mode. Unpacked, bi_flags is the flags and the str_hash type, where the
//! packed versions have them. BCH_INODE_has_inode_opts is
//! pack()'s to maintain, from the option fields. With CONFIG_BCACHEFS_DEBUG,
//! pack() checks what it wrote unpacks back: the inode number, hash seed,
//! mode, sizes, version and every packed field.
//!
//! C calls in through the bch2_* exports, which keep C's signatures.
//! inode_opts.c - option inheritance and propagation - is still C, called
//! from here.

#![allow(non_camel_case_types)]

use crate::btree::bkey::prelude::*;
use crate::c;
use crate::errcode::{Errcode, BchError, Found};
use crate::fs::Fs;
use crate::dirent::{self, DirentTarget};
use crate::namei;
use crate::os::kernel::random_u64;
use crate::{bch_err_ratelimited, bch_warn, trans_inconsistent};
use crate::btree::bkey::{bkey_family, pos, spos, BKEY_U64S, BkeyFamily, BkeyI, BkeyS, BkeySC, POS_MIN, SPOS_MAX,
                         BkeyOps};
use crate::init::passes::{self, RecoveryPass};
use crate::util::varint;
use core::mem::{offset_of, align_of};
use crate::btree::trigger::{Trigger, TriggerOp};
use crate::init::progress::Progress;
use crate::{bch_verbose_ratelimited, fs_inconsistent, fsck_err, fsck_err_on};
use crate::btree::iter::{
    commit_do, is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags,
    TransAttempt, TransBkey, UpdateTriggerFlags,
};
use crate::btree::trans::TransGuard;
use crate::init::error::{id, BkeyValidate};
use crate::snapshots::subvolume;
use crate::util::{Printbuf, ToText};
use crate::util::log::LogMsg;
use crate::{bch_err_fn, bch_err_msg, bkey_fsck_err_on, prt};
use core::mem::size_of;
use core::ops::ControlFlow;
use core::ffi::CStr;
use cstruct_macros::{bitfield, BitfieldMembers, MaybeZeroable, TypeInfo};
use crate::cstructs::c::BCH_INODE_FIELDS_v3;
use crate::types::{c_default, Le64};
use crate::util::projections::Volatile;
use crate::format::{Bpos, Btree, BkeyType, MetadataVersion, SbFeature};

#[allow(non_camel_case_types)]
pub type u96 = u64;

macro_rules! __bch_inode_unpacked_0 {
    ([$($(#[$m:meta])* pub $f:ident: $ft:ty),* $(,)?] $(($_name:tt, $_bits:tt)),* $(,)?) => { ::paste::paste! {
        #[repr(C)]
        #[derive(Clone, Copy, Debug, Default, MaybeZeroable, TypeInfo, BitfieldMembers)]
        pub struct bch_inode_unpacked {
            $($(#[$m])* pub $f: $ft,)*
            $(pub $_name: [<u $_bits>],)*
        }

        /// A bch_inode_unpacked shared - a VFS inode's ei_inode: written by
        /// one writer at a time, read without a lock, field by field - and
        /// laid out as it is, so C has it as one.
        #[repr(C)]
        #[derive(TypeInfo, BitfieldMembers)]
        pub struct VolatileInodeUnpacked {
            $($(#[$m])* pub $f: Volatile<$ft>,)*
            $(pub $_name: Volatile<[<u $_bits>]>,)*
        }

        impl VolatileInodeUnpacked {
            /// Each field as it is: not all of them as of one moment, if
            /// it's written meanwhile.
            pub fn get(&self) -> bch_inode_unpacked {
                bch_inode_unpacked {
                    $($f: self.$f.get(),)*
                    $($_name: self.$_name.get(),)*
                }
            }

            /// By its one writer.
            pub fn set(&self, v: &bch_inode_unpacked) {
                $(self.$f.set(v.$f);)*
                $(self.$_name.set(v.$_name);)*
            }
        }

        const _: () = {
            assert!(size_of::<VolatileInodeUnpacked>() == size_of::<bch_inode_unpacked>());
            assert!(align_of::<VolatileInodeUnpacked>() == align_of::<bch_inode_unpacked>());
            $(assert!(offset_of!(VolatileInodeUnpacked, $f) == offset_of!(bch_inode_unpacked, $f));)*
            $(assert!(offset_of!(VolatileInodeUnpacked, $_name) == offset_of!(bch_inode_unpacked, $_name));)*
        };
    } };
}
BCH_INODE_FIELDS_v3!(__bch_inode_unpacked_0 [
    pub bi_inum: u64,
    pub bi_snapshot: u32,
    pub bi_journal_seq: u64,
    pub bi_hash_seed: Le64,
    pub bi_size: u64,
    pub bi_sectors: u64,
    pub bi_version: u64,
    #[members]
    pub bi_flags: InodeUnpackedBiFlags,
    pub bi_mode: u16,
]);

#[bitfield(u32)]
pub struct InodeUnpackedBiFlags {
    #[bits(20, from = c::InodeFlags::from_bits_retain)]
    flags:    c::InodeFlags,
    #[bits(4)]
    str_hash: c::StrHashKind,
    __:       u8,
}
crate::one_word!(InodeUnpackedBiFlags);

macro_rules! __BKEY_INODE_BUF_XSUM_0 {
    ($(($_name:tt, $_bits:tt)),* $(,)?) => {
        #[allow(non_upper_case_globals)]
        const BKEY_INODE_BUF_XSUM_0: usize = 0 + (0 $(+ 8 + (($_bits) as usize) / 8)*);
    };
}
BCH_INODE_FIELDS_v3!(__BKEY_INODE_BUF_XSUM_0);

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct bkey_inode_buf {
    /* C's typed key: a v3 inode's header, its fields in _pad */
    pub inode: BkeyI<c::InodeV3>,
    pub _pad: [u8; BKEY_INODE_BUF_XSUM_0],
}
c_default!(bkey_inode_buf);

// The inode x-macros as m! { name = arg, ... }, the way C uses them: each
// user defines its own m, as C code defines its own x().
macro_rules! name_eq_arg {
    ([$m:ident] $(($name:tt, $arg:tt)),* $(,)?) => { $m! { $($name = $arg,)* } };
}
#[allow(unused_macros)]
macro_rules! inode_fields_v2 { ($m:ident) => { c::BCH_INODE_FIELDS_v2!(name_eq_arg [$m]) }; }
#[allow(unused_macros)]
macro_rules! inode_fields_v3 { ($m:ident) => { c::BCH_INODE_FIELDS_v3!(name_eq_arg [$m]) }; }

// ── Packing ──────────────────────────────────────────────────────────────

/// Where an inode_v3's packed fields start, in its value - past the fixed
/// fields. Unpack reads them from here, not from INODEv3_FIELDS_START, as C
/// always has: the validate check on that field is all it's used for.
const V3_FIELDS_OFFSET: usize = offset_of!(c::InodeV3, repr.fields);

/// The packed fields an inode's value ends with, decoded into @u: the first
/// @nr_fields, in inode_fields_v3!() order if @v3, else inode_fields_v2!(),
/// the rest left as they are. @decode reads one field, of so many bits, from
/// the start of a slice: its value and length. Err(the field number) at a
/// field that doesn't decode, or is too big for its field in @u.
#[allow(unused_assignments)]
fn unpack_fields(
    u:         &mut c::bch_inode_unpacked,
    v3:        bool,
    mut in_:   &[u8],
    nr_fields: usize,
    decode:    impl Fn(&[u8], u32) -> Option<(u128, usize)>,
) -> Result<(), usize> {
    let mut fieldnr = 0;

    macro_rules! fields {
        ($($name:ident = $bits:literal,)*) => {$(
            if fieldnr == nr_fields {
                return Ok(());
            }
            let (v, len) = decode(in_, $bits).ok_or(fieldnr)?;
            u.$name = v.try_into().map_err(|_| fieldnr)?;
            in_ = &in_[len..];
            fieldnr += 1;
        )*};
    }

    if v3 {
        inode_fields_v3!(fields);
    } else {
        inode_fields_v2!(fields);
    }

    // XXX: signal if there were more fields than expected?
    Ok(())
}

/// One field as v2 and v3 pack them: a varint - two, low half first, for a
/// field wider than 64 bits.
fn varint_field(in_: &[u8], bits: u32) -> Option<(u128, usize)> {
    let (lo, mut len) = varint::decode(in_).ok()?;
    let mut hi = 0;

    if bits > 64 {
        let (v, n) = varint::decode(&in_[len..]).ok()?;
        hi = v;
        len += n;
    }

    Some(((hi as u128) << 64 | lo as u128, len))
}

/// The original packing's field lengths, indexed by the first byte's leading
/// zeros.
const V1_FIELD_BYTES: [usize; 8] = [1, 2, 3, 4, 6, 8, 10, 13];

/// One field as the original packing has it: big endian, its length given by
/// the first byte's highest set bit - which isn't part of the value.
fn v1_field(in_: &[u8], _bits: u32) -> Option<(u128, usize)> {
    let first = *in_.first()?;
    if first == 0 {
        return None;
    }

    let lz = first.leading_zeros() as usize;
    let len = V1_FIELD_BYTES[lz];

    let mut be = [0u8; 16];
    be[16 - len..].copy_from_slice(in_.get(..len)?);
    be[16 - len] ^= 0x80 >> lz;

    Some((u128::from_be_bytes(be), len))
}

/// Unpack @k, reporting nothing: Err(what did unpack, and the field it
/// stopped at) if a field doesn't - it, and the fields after it, are left
/// zero.
// result_large_err: Ok is the same bch_inode_unpacked, so boxing Err would
// shrink nothing
#[allow(clippy::type_complexity, clippy::result_large_err)]
fn try_unpack(k: BkeySC<'_, Inodes>)
    -> Result<c::bch_inode_unpacked, (c::bch_inode_unpacked, usize)>
{
    let mut u = c::bch_inode_unpacked {
        bi_inum:     k.k.p.offset,
        bi_snapshot: k.k.p.snapshot,
        ..Default::default()
    };

    let val = k.val_bytes();
    let fields_at = |offset: usize| val.get(offset..).unwrap_or_default();

    let ret = match k.typed() {
        InodesTyped::inode_v3(i) => {
            let v = i.v();
            u.bi_journal_seq = v.bi_journal_seq.get();
            u.bi_hash_seed   = v.bi_hash_seed;
            u.set_flags(v.flags());
            u.set_str_hash(v.str_hash());
            u.bi_sectors     = v.bi_sectors.get();
            u.bi_size        = v.bi_size.get();
            u.bi_version     = v.bi_version.get();
            u.bi_mode        = v.mode();

            unpack_fields(&mut u, true, fields_at(V3_FIELDS_OFFSET), v.nr_fields() as usize, varint_field)
        }
        InodesTyped::inode_v2(i) => {
            let v = i.v();
            u.bi_journal_seq = v.bi_journal_seq.get();
            u.bi_hash_seed   = v.bi_hash_seed;
            u.set_flags(v.flags());
            u.set_str_hash(v.str_hash());
            u.bi_mode        = v.bi_mode.get();

            unpack_fields(&mut u, false, fields_at(offset_of!(c::InodeV2, repr.fields)),
                          v.nr_fields() as usize, varint_field)
        }
        InodesTyped::inode(i) => {
            let v = i.v();
            u.bi_hash_seed   = v.bi_hash_seed;
            u.set_flags(v.flags());
            u.set_str_hash(v.str_hash());
            u.bi_mode        = v.bi_mode.get();

            let in_ = fields_at(offset_of!(c::Inode, repr.fields));
            let nr_fields = v.nr_fields() as usize;

            if v.new_varint() {
                unpack_fields(&mut u, false, in_, nr_fields, varint_field)
            } else {
                unpack_fields(&mut u, false, in_, nr_fields, v1_field)
            }
        }
    };

    match ret {
        Ok(())       => Ok(u),
        Err(fieldnr) => Err((u, fieldnr)),
    }
}

/// Report that @k's packed fields stopped decoding at @fieldnr - @u has what
/// did - and schedule the recovery passes that check fields that can't be
/// left zero. Most can: zero is legal (bi_dir, bi_depth), or loses only
/// behaviour (timestamps, options). Printed only if a pass is newly
/// scheduled.
///
/// Not the value's to_text: an inode's unpacks it, and would fail here again.
#[cold]
fn unpack_error(fs: &Fs, k: BkeySC<'_, Inodes>, u: &c::bch_inode_unpacked, fieldnr: usize) {
    macro_rules! names {
        ($($name:ident = $bits:literal,)*) => { &[$(stringify!($name),)*] };
    }
    let names: &[&str] = if matches!(k.typed(), InodesTyped::inode_v3(_)) {
        inode_fields_v3!(names)
    } else {
        inode_fields_v2!(names)
    };

    let passes = [
        ("bi_nlink",         RecoveryPass::check_nlinks),
        ("bi_subvol",        RecoveryPass::check_subvols),
        ("bi_parent_subvol", RecoveryPass::check_subvolume_structure),
        // Dirent hashes depend on it - v2 doesn't have it:
        ("bi_casefold",      RecoveryPass::check_dirents),
    ];

    let mut msg = LogMsg::new(fs);
    msg.set_suppressed(true);
    prt!(&mut msg, "inode unpack error at field {fieldnr} ({}) in {}\n", names[fieldnr], k.k);
    u.to_text(&mut msg);

    crate::init::error::count_fsck_err(fs, id::inode_unpack_error, &mut msg);

    for (field, pass) in passes {
        if names.iter().position(|n| *n == field).is_some_and(|i| fieldnr <= i) {
            // As C: unpack has no error to return.
            let _ = passes::run_explicit(fs, &mut msg, pass, c::RunRecoveryPassFlags::from_bits_retain(0));
        }
    }
}

/// Unpack @k. A field that doesn't decode is reported, and left zero with the
/// fields after it.
pub fn unpack(fs: &Fs, k: BkeySC<'_, Inodes>) -> c::bch_inode_unpacked {
    try_unpack(k).unwrap_or_else(|(u, fieldnr)| {
        unpack_error(fs, k, &u, fieldnr);
        u
    })
}

impl<'a> From<&'a c::bkey_inode_buf> for BkeySC<'a, Inodes> {
    fn from(b: &'a c::bkey_inode_buf) -> Self {
        BkeySC::from(b.as_ref()).arm::<Inodes>().expect("pack() makes an inode")
    }
}

/// Its key, all of it: the typed key's header, then the fields packed past
/// it, in _pad - which the typed key, inode, doesn't reach.
impl AsRef<BkeyI> for c::bkey_inode_buf {
    fn as_ref(&self) -> &BkeyI {
        // SAFETY: the key at the start of the buffer, its u64s within it, as
        // pack() sizes them; from a pointer to all of the buffer
        unsafe { &*BkeyI::from_raw((&raw const self.inode).cast()) }
    }
}

impl AsMut<BkeyI> for c::bkey_inode_buf {
    fn as_mut(&mut self) -> &mut BkeyI {
        // SAFETY: as as_ref()'s
        unsafe { &mut *BkeyI::from_raw_mut((&raw mut self.inode).cast()) }
    }
}

/// @inode packed, as an inode_v3 key at snapshot 0 - the caller's to set.
/// The key ends at the last nonzero field, and has_inode_opts is recomputed -
/// pack is what maintains it.
pub fn pack(inode: &c::bch_inode_unpacked) -> c::bkey_inode_buf {

    let mut out = c::bkey_inode_buf::default();

    let mut flags = inode.flags();
    flags.set(c::InodeFlags::has_inode_opts, inode.has_opts());

    let k = &mut out.inode;
    *k = BkeyI { k: c::bkey::new(), v: Default::default() };
    k.k.set_key_type(BkeyType::inode_v3);
    k.k.set_val_bytes(size_of::<c::InodeV3>());
    k.k.p.offset = inode.bi_inum;
    k.v.bi_journal_seq = inode.bi_journal_seq.into();
    k.v.bi_hash_seed   = inode.bi_hash_seed;
    k.v.set_flags(flags);
    k.v.set_str_hash(inode.str_hash());
    k.v.bi_sectors     = inode.bi_sectors.into();
    k.v.bi_size        = inode.bi_size.into();
    k.v.bi_version     = inode.bi_version.into();
    k.v.set_mode(inode.bi_mode);
    k.v.set_fields_start((V3_FIELDS_OFFSET / 8) as u8);

    // The fields go after the value's fixed part - in _pad, the room the
    // buffer has for every one of them at its biggest:
    const _: () = assert!(offset_of!(c::bkey_inode_buf, _pad) ==
                          offset_of!(c::bkey_inode_buf, inode) +
                          offset_of!(BkeyI<c::InodeV3>, v) +
                          V3_FIELDS_OFFSET);
    let fields = &mut out._pad;

    let (mut len, mut nr) = (0, 0);
    let (mut used_len, mut used_nr) = (0, 0);

    macro_rules! fields {
        ($($name:ident = $bits:literal,)*) => {$(
            let v = inode.$name as u64;
            len += varint::encode(&mut fields[len..], v);
            if $bits > 64 {
                len += varint::encode(&mut fields[len..], 0);
            }
            nr += 1;

            if v != 0 {
                used_len = len;
                used_nr  = nr;
            }
        )*};
    }
    inode_fields_v3!(fields);

    // Trailing zero fields are left off; what they encoded to is zeroes,
    // so the value's last u64 is zero-padded:
    let k = &mut out.inode;
    k.k.u64s = (BKEY_U64S + (V3_FIELDS_OFFSET + used_len).div_ceil(8)) as u8;
    k.v.set_nr_fields(used_nr);

    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    {
        let u = try_unpack(BkeySC::from(&out))
            .unwrap_or_else(|(_, fieldnr)| panic!("packed inode doesn't unpack, at field {fieldnr}"));

        macro_rules! check {
            ($($name:ident),*) => {$(
                assert_eq!(u.$name, inode.$name, concat!("packed inode's ", stringify!($name)));
            )*};
        }
        check!(bi_inum, bi_hash_seed, bi_sectors, bi_size, bi_version, bi_mode);

        macro_rules! fields {
            ($($name:ident = $bits:literal,)*) => { check!($($name),*); };
        }
        inode_fields_v3!(fields);
    }

    out
}

// ── Reading keys, without unpacking ──────────────────────────────────────

bkey_family!(
    /// An inode, of any version, BkeyTypeFlags::inode: a key armed to it -
    /// BkeySC<'_, Inodes>, BkeyS<'_, Inodes>, &BkeyI<Inodes> - is read
    /// through typed(), or without unpacking through BkeyVInode.
    Inodes: inode);

/// What every inode version has, read and written without unpacking it, in
/// whichever form it's held: any BkeyV armed to Inodes.
pub trait BkeyVInode: BkeyV<V = Inodes> {
    fn mode(&self) -> u32 {
        match self.as_sc().typed() {
            InodesTyped::inode(i)    => i.v().bi_mode.get() as u32,
            InodesTyped::inode_v2(i) => i.v().bi_mode.get() as u32,
            InodesTyped::inode_v3(i) => i.v().mode() as u32,
        }
    }

    /// As bkey_inode_flags().
    fn flags(&self) -> c::InodeFlags {
        match self.as_sc().typed() {
            InodesTyped::inode(i)    => i.v().flags(),
            InodesTyped::inode_v2(i) => i.v().flags(),
            InodesTyped::inode_v3(i) => i.v().flags(),
        }
    }

    /// As bkey_inode_flags_set().
    fn set_flags(&mut self, f: c::InodeFlags) where Self: BkeyVMut {
        match self.as_s().typed() {
            InodesTyped::inode(mut i)    => i.v_mut().set_flags(f),
            InodesTyped::inode_v2(mut i) => i.v_mut().set_flags(f),
            InodesTyped::inode_v3(mut i) => i.v_mut().set_flags(f),
        }
    }

    /// Stamp it with the journal sequence number it's committed in - an inode
    /// v1 has nowhere to: as bkey_inode_journal_seq_set().
    fn set_journal_seq(&mut self, seq: u64) where Self: BkeyVMut {
        match self.as_s().typed() {
            InodesTyped::inode(_)        => {}
            InodesTyped::inode_v2(mut i) => i.v_mut().bi_journal_seq = seq.into(),
            InodesTyped::inode_v3(mut i) => i.v_mut().bi_journal_seq = seq.into(),
        }
    }
}

impl<K: BkeyV<V = Inodes>> BkeyVInode for K {}

impl c::bkey {
    /// Whether it's an inode, of any version: as C's bkey_is_inode().
    pub fn is_inode(&self) -> bool {
        <Inodes as BkeyFamily>::contains(self)
    }
}

// ── Lookups and writes ───────────────────────────────────────────────────

/// The inode at @iter's position: ENOENT_inode if there isn't one.
fn peek_slot<'t>(t: &TransAttempt<'_, 't>, iter: &mut BtreeIter<'t>)
    -> Result<c::bch_inode_unpacked, BchError>
{
    let k = iter.peek_slot(t)?.expect("a slot always has a key");
    let Some(k) = k.arm::<Inodes>() else {
        return Err(t.fs().err(Errcode::ENOENT_inode));
    };
    Ok(unpack(t.fs(), k))
}

/// Peek inode @inum as seen in @snapshot, through @iter: ENOENT_inode if it
/// isn't there. With @warn - C passes its __func__ - an error is logged,
/// naming it. As __bch2_inode_peek_snapshot().
fn __peek_snapshot<'a>(
    t:        &TransAttempt<'a, '_>,
    iter:     &mut BtreeIter<'a>,
    inum:     c::subvol_inum,
    snapshot: u32,
    flags:    BtreeIterFlags,
    warn:     Option<&CStr>,
) -> Result<c::bch_inode_unpacked, BchError> {
    *iter = BtreeIter::new(t, Btree::inodes, spos(0, inum.inum, snapshot),
                           flags | BtreeIterFlags::CACHED);
    let ret = peek_slot(t, iter);

    match warn {
        Some(warn) => bch_err_msg!(t.fs(), ret, "{}: looking up inum {}:{}:",
                                   warn.to_str().unwrap_or("?"), inum.subvol, inum.inum),
        None       => ret,
    }
}

/// As __peek_snapshot(), in @inum's subvolume's snapshot: as
/// __bch2_inode_peek().
pub(crate) fn __peek<'a>(
    t:     &TransAttempt<'a, '_>,
    iter:  &mut BtreeIter<'a>,
    inum:  c::subvol_inum,
    flags: BtreeIterFlags,
    warn:  Option<&CStr>,
) -> Result<c::bch_inode_unpacked, BchError> {
    // C passes @warn on as the bool saying whether to warn:
    let snapshot = match warn {
        Some(_) => subvolume::get_snapshot(t, inum.subvol as u32)?,
        None    => subvolume::get_snapshot_nowarn(t, inum.subvol as u32)?,
    };
    __peek_snapshot(t, iter, inum, snapshot, flags, warn)
}

/// Peek inode @inum through @iter - for updating it through @iter: as
/// bch2_inode_peek_nowarn(). ENOENT_inode if it isn't there.
pub fn peek<'a>(
    t:     &TransAttempt<'a, '_>,
    iter:  &mut BtreeIter<'a>,
    inum:  c::subvol_inum,
    flags: BtreeIterFlags,
) -> Result<c::bch_inode_unpacked, BchError> {
    __peek(t, iter, inum, flags, None)
}

/// As peek(), as seen in @snapshot: as bch2_inode_peek_snapshot(), without
/// the warning.
pub fn peek_snapshot<'a>(
    t:        &TransAttempt<'a, '_>,
    iter:     &mut BtreeIter<'a>,
    inum:     c::subvol_inum,
    snapshot: u32,
    flags:    BtreeIterFlags,
) -> Result<c::bch_inode_unpacked, BchError> {
    __peek_snapshot(t, iter, inum, snapshot, flags, None)
}

/// Inode @inum as seen in @snapshot: ENOENT_inode if there's no inode there.
/// In the caller's transaction attempt.
pub fn find_by_inum_snapshot(
    t:        &TransAttempt<'_, '_>,
    inum:     u64,
    snapshot: u32,
    flags:    BtreeIterFlags,
) -> Result<c::bch_inode_unpacked, BchError> {
    let mut iter = BtreeIter::new(t, Btree::inodes, spos(0, inum, snapshot), flags);
    peek_slot(t, &mut iter)
}

/// Inode @inum, in its subvolume, as seen in @snapshot: an error logged,
/// naming @warn, if it isn't there - bch2_inode_find_by_inum_snapshot2(). In
/// the caller's transaction attempt.
pub fn find_by_inum_in_snapshot(
    t:        &TransAttempt<'_, '_>,
    inum:     c::subvol_inum,
    snapshot: u32,
    warn:     Option<&CStr>,
) -> Result<c::bch_inode_unpacked, BchError> {
    let mut iter = BtreeIter::uninit();
    __peek_snapshot(t, &mut iter, inum, snapshot, BtreeIterFlags::empty(), warn)
}

/// The inode @inum, in a transaction: as bch2_inode_find_by_inum_trans(). An
/// error is logged, naming @warn - pass here!(), as C passes
/// __func__. In the caller's transaction attempt.
pub fn find_by_inum_trans(
    t:    &TransAttempt<'_, '_>,
    inum: c::subvol_inum,
    warn: &CStr,
) -> Result<c::bch_inode_unpacked, BchError> {
    let mut iter = BtreeIter::uninit();
    __peek(t, &mut iter, inum, BtreeIterFlags::empty(), Some(warn))
}

/// find_by_inum_trans(), logging nothing: as
/// bch2_inode_find_by_inum_nowarn_trans().
pub fn find_by_inum_nowarn_trans(t: &TransAttempt<'_, '_>, inum: c::subvol_inum)
    -> Result<c::bch_inode_unpacked, BchError>
{
    let mut iter = BtreeIter::uninit();
    __peek(t, &mut iter, inum, BtreeIterFlags::empty(), None)
}

/// The inode @inum, in its own transaction: as bch2_inode_find_by_inum().
pub fn find_by_inum(fs: &Fs, inum: c::subvol_inum) -> Result<c::bch_inode_unpacked, BchError> {
    let trans = crate::btree_trans!(fs);
    lockrestart_do(&trans, |t| find_by_inum_trans(t, inum, c"bch2_inode_find_by_inum"))
}

/// The oldest version of @inum that a key in @snapshot sees - the version
/// all the others take their hash info from: as
/// bch2_inode_find_oldest_snapshot(). In the caller's transaction attempt.
///
/// Ancestors have higher snapshot IDs: walking up from @snapshot, the last
/// version that's an ancestor is the oldest. The first is the nearest -
/// @snapshot's own, if it has one.
pub fn find_oldest_snapshot(
    t:        &TransAttempt<'_, '_>,
    inum:     u64,
    snapshot: u32,
) -> Result<c::bch_inode_unpacked, BchError> {
    let fs = t.fs();
    let mut root = None;

    let mut iter = BtreeIter::new(t, Btree::inodes, spos(0, inum, snapshot),
                                  BtreeIterFlags::ALL_SNAPSHOTS);
    iter.for_each_norestart(t, |_, k| Ok(
        if k.k.p.offset != inum {
            ControlFlow::Break(())
        } else {
            if let Some(k) = k.arm::<Inodes>()
                .filter(|k| crate::snapshots::snapshot::is_ancestor(t, snapshot, k.k.p.snapshot)) {
                root = Some(unpack(fs, k));
            }
            ControlFlow::Continue(())
        }))?;

    root.ok_or_else(|| fs.err(Errcode::ENOENT_inode))
}

/// Any surviving version of @inum, in any snapshot - ancestor, descendant or
/// sibling: as bch2_inode_find_any_snapshot(). In the caller's transaction
/// attempt.
///
/// Only for recovering the fields that are snapshot-invariant by
/// construction, i.e. hash info: every version of an inode must agree on
/// bi_hash_seed and the str_hash type, which is what
/// str_hash::repair_inode_hash_info() enforces. Anything that needs the
/// authoritative version wants find_oldest_snapshot() instead - a
/// descendant's other fields are not a valid stand-in for an ancestor's.
pub fn find_any_snapshot(t: &TransAttempt<'_, '_>, inum: u64)
    -> Result<c::bch_inode_unpacked, BchError>
{
    let fs = t.fs();

    let mut iter = BtreeIter::new(t, Btree::inodes, pos(0, inum),
                                  BtreeIterFlags::ALL_SNAPSHOTS);
    iter.for_each_norestart(t, |_, k| Ok(
        if k.k.p.offset != inum {
            ControlFlow::Break(None)
        } else if let Some(k) = k.arm::<Inodes>() {
            ControlFlow::Break(Some(unpack(fs, k)))
        } else {
            ControlFlow::Continue(())
        }))?
        .ok_or_else(|| fs.err(Errcode::ENOENT_inode))
}

/// @inode packed, in transaction memory - at no snapshot, which is for the
/// caller to set: as pack().
fn pack_trans<'a, 't>(t: &TransAttempt<'a, 't>, inode: &c::bch_inode_unpacked)
    -> Result<TransBkey<'a, 't>, BchError>
{
    let buf = t.kmalloc(size_of::<c::bkey_inode_buf>())?.as_mut_ptr() as *mut c::bkey_inode_buf;
    unsafe {
        buf.write(pack(inode));
        TransBkey::from_raw(t, (&raw mut (*buf).inode).cast())
    }
}

/// Queue writing @inode back through @iter, which peeked it, with @flags: as
/// bch2_inode_write_flags().
pub fn write_flags<'t>(
    t:     &TransAttempt<'_, 't>,
    iter:  &mut BtreeIter<'t>,
    inode: &mut c::bch_inode_unpacked,
    flags: UpdateTriggerFlags,
) -> Result<(), BchError> {
    let mut k = pack_trans(t, inode)?;
    k.k_mut().p.snapshot = iter.snapshot();
    t.update(iter, &k, flags)
}

/// Queue writing @inode back through @iter, which peeked it: as
/// bch2_inode_write().
pub fn write<'t>(
    t:     &TransAttempt<'_, 't>,
    iter:  &mut BtreeIter<'t>,
    inode: &mut c::bch_inode_unpacked,
) -> Result<(), BchError> {
    write_flags(t, iter, inode, UpdateTriggerFlags::empty())
}

/// Queue writing back @inode, at its own snapshot, for fsck repair - the
/// caller commits: as __bch2_fsck_write_inode().
pub fn fsck_write(t: &TransAttempt<'_, '_>, inode: &mut c::bch_inode_unpacked)
    -> Result<(), BchError>
{
    let mut k = pack_trans(t, inode)?;
    k.k_mut().p.snapshot = inode.bi_snapshot;
    t.insert_with(Btree::inodes, k, BtreeIterFlags::CACHED,
                  UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

/// As fsck_write(), committed: as bch2_fsck_write_inode().
pub fn fsck_write_inode(trans: &BtreeTrans<'_>, inode: &mut c::bch_inode_unpacked)
    -> Result<(), BchError>
{
    let fs = trans.fs();
    bch_err_fn!(fs, commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| fsck_write(t, inode)))
}

/// Write @inode, in its own commit, through the key cache, at snapshot
/// U32_MAX - the root snapshot a new filesystem starts with: for copying a
/// tree in at format time.
pub fn write_cached(fs: &Fs, inode: &c::bch_inode_unpacked) -> Result<(), BchError> {
    let mut packed = pack(inode);
    packed.inode.k.p.snapshot = u32::MAX;
    fs.btree_insert(
        Btree::inodes,
        packed.as_mut(),
        None,
        CommitFlags::empty(),
        BtreeIterFlags::CACHED,
    )
}

/// @k, an inode of any version, as an inode_v3 - at no snapshot, as packing
/// leaves it - in transaction memory: as bch2_inode_to_v3(). ENOENT if it
/// isn't an inode.
pub fn to_v3<'a, 't>(t: &TransAttempt<'a, 't>, k: &BkeyI) -> Result<TransBkey<'a, 't>, BchError> {
    let Some(k) = BkeySC::from(k).arm::<Inodes>() else {
        return Err(BchError::from(c::ENOENT));
    };
    pack_trans(t, &unpack(t.fs(), k))
}

// ── Validate, to_text ────────────────────────────────────────────────────

/// What every inode version is validated for: its position - inode 0, and
/// above the block device range.
fn validate_pos(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    bkey_fsck_err_on!(v, v.k.k.p.inode != 0, id::inode_pos_inode_nonzero,
                      "nonzero k.p.inode")?;

    bkey_fsck_err_on!(v, v.k.k.p.offset < c::BLOCKDEV_INODE_MAX as u64,
                      id::inode_pos_blockdev_range, "fs inode in blockdev range")
}

/// A str_hash type that isn't one.
fn validate_str_hash(v: &BkeyValidate<'_, '_>, str_hash: c::StrHashKind) -> Result<(), BchError> {
    let (str_hash, nr) = (str_hash.0, c::StrHashKind::NR);
    bkey_fsck_err_on!(v, str_hash >= nr, id::inode_str_hash_invalid,
                      "invalid str hash type ({str_hash} >= {nr})")
}

#[cold]
fn inode_to_text(out: &mut Printbuf, fs: Option<&Fs>, k: BkeySC<'_>) {
    let k = k.arm::<Inodes>().expect("an inode type's to_text");
    // with no filesystem - show-super's sb_clean - nothing to report a bad one to
    let inode = match fs {
        Some(fs) => unpack(fs, k),
        None     => try_unpack(k).unwrap_or_else(|(u, _)| u),
    };
    fields_to_text(out, &inode)
}

impl BkeyOps for c::Inode {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        validate_str_hash(v, v.k.arm::<c::Inode>().expect("an inode").v().str_hash())?;
        validate_pos(v)
    }

    fn to_text(out: &mut Printbuf, fs: Option<&Fs>, k: BkeySC<'_>) {
        inode_to_text(out, fs, k)
    }

    const TRIGGER: Option<&'static Trigger> = Some(&INODE);
}

impl BkeyOps for c::InodeV2 {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        validate_str_hash(v, v.k.arm::<c::InodeV2>().expect("an inode_v2").v().str_hash())?;
        validate_pos(v)
    }

    fn to_text(out: &mut Printbuf, fs: Option<&Fs>, k: BkeySC<'_>) {
        inode_to_text(out, fs, k)
    }

    const TRIGGER: Option<&'static Trigger> = Some(&INODE);
}

/// The fields start past the fixed ones, within the value.
impl BkeyOps for c::InodeV3 {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        let i = v.k.arm::<c::InodeV3>().expect("an inode_v3").v();
        let start = i.fields_start() as u64;
        let min = c::INODEv3_FIELDS_START_INITIAL as u64;
        let max = v.k.k.u64s as u64 - crate::btree::bkey::BKEY_U64S as u64;

        bkey_fsck_err_on!(v, start < min || start > max, id::inode_v3_fields_start_bad,
                          "invalid fields_start (got {start}, min {min} max {max})")?;

        validate_str_hash(v, i.str_hash())?;
        validate_pos(v)
    }

    fn to_text(out: &mut Printbuf, fs: Option<&Fs>, k: BkeySC<'_>) {
        inode_to_text(out, fs, k)
    }

    const TRIGGER: Option<&'static Trigger> = Some(&INODE);
}

impl BkeyOps for c::InodeGeneration {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        bkey_fsck_err_on!(v, v.k.k.p.inode != 0, id::inode_pos_inode_nonzero,
                          "nonzero k.p.inode")
    }

    #[cold]
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, k: BkeySC<'_>) {
        let generation = (k.arm::<c::InodeGeneration>().expect("an inode_generation").v().bi_generation).get();
        write!(out, "generation: {generation}");
    }
}

/// @inode's fields, a line each, after a newline.
fn fields_to_text(out: &mut Printbuf, inode: &c::bch_inode_unpacked) {
    out.newline();

    let d_type = (inode.bi_mode as u32 >> 12) & 15;
    let d_type_name = if d_type < c::BCH_DT_MAX {
        dirent::d_type_str(d_type as u8)
    } else {
        "unknown"
    };
    writeln!(out, "mode={:o} ({d_type_name})", inode.bi_mode);

    write!(out, "flags=");
    inode.flags().to_text(out);
    writeln!(out, "({:x})", inode.bi_flags.into_bits());

    writeln!(out, "journal_seq={}", inode.bi_journal_seq);
    writeln!(out, "hash_seed={:x}", inode.bi_hash_seed);
    prt!(out, "hash_type={}\n", inode.str_hash());
    writeln!(out, "bi_size={}", inode.bi_size);
    writeln!(out, "bi_sectors={}", inode.bi_sectors);
    writeln!(out, "bi_version={}", inode.bi_version);

    macro_rules! field {
        ($($name:ident = $bits:literal,)*) => {
            $(writeln!(out, concat!(stringify!($name), "={}"), inode.$name as u64);)*
        };
    }
    inode_fields_v3!(field);
}

impl ToText for c::bch_inode_unpacked {
    /// By number and snapshot, then its fields: as
    /// bch2_inode_unpacked_to_text().
    fn to_text(&self, out: &mut Printbuf) {
        write!(out, "inum: {}:{} ", self.bi_inum, self.bi_snapshot);
        let mut out = out.indent(2);
        fields_to_text(&mut out, self);
    }
}

// ── Inode number allocation ──────────────────────────────────────────────
//
// Inode numbers are handed out from cursors, in the logged_ops btree at
// LOGGED_OPS_INUM_inode_cursors: cursor 0 for inodes below 2^31 - for
// inodes_32bit - and with shard_inode_numbers_bits, one per shard above
// that, so allocators in different threads don't contend on one cursor. A
// cursor that reaches the end of its range wraps, bumping its generation,
// which new inodes take - so a reused inode number gets a new generation.

/// Give each inode number shard a CPU, spread across the online CPUs, NUMA
/// nodes first - for the scheduler to keep a shard's allocations together:
/// as bch2_fs_inode_shard_cpu_init(). Kernel only.
pub fn shard_cpu_init(fs: &Fs) {
    #[cfg(kernel)]
    {
        use crate::os::kernel::{cpumask_local_spread, nr_cpu_ids};
        use core::sync::atomic::Ordering::Relaxed;

        let bits = fs.opts().shard_inode_numbers_bits.get() as u32;
        if bits == 0 {
            return;
        }

        let shard_cpu = &fs.inode_shard_cpu;
        let nr_shards = 1usize << bits;
        // The option's range, up to 8 bits, keeps it within inode_shard_cpu[]:
        assert!(nr_shards <= shard_cpu.len(), "{nr_shards} inode number shards");

        for (shard, cpu) in shard_cpu[..nr_shards].iter().enumerate() {
            let c = cpumask_local_spread(shard as u32);
            cpu.store(if c < nr_cpu_ids() { c as u16 } else { 0 }, Relaxed);
        }
    }
    #[cfg(not(kernel))]
    let _ = fs;
}

/// The default for shard_inode_numbers_bits, when it isn't set.
///
/// Scales with @nr_cpus - doubled, for oversubscribed threads - capped so the
/// shard boundaries' btree nodes (four sharded btrees, a node per shard) stay
/// under 1% of the filesystem, and at 8, what the option and
/// inode_shard_cpu[] allow. One function for both the format-time default
/// and the kernel's rewrite of a legacy 0.
pub fn shard_bits_default(nr_cpus: u32, fs_size: u64, btree_node_bytes: u64) -> u32 {
    let cpu_bits = (nr_cpus.max(1) as u64 * 2).next_power_of_two().ilog2();

    // 2^bits * 4 * btree_node_bytes <= fs_size / 100
    //   =>  2^bits <= fs_size / (400 * btree_node_bytes)
    let denom = btree_node_bytes.wrapping_mul(400);
    let size_bits = if denom != 0 && fs_size >= denom { (fs_size / denom).ilog2() } else { 0 };

    cpu_bits.min(size_bits).min(8)
}

/// The inode numbers cursor @idx hands out: [min, max).
fn cursor_range(fs: &Fs, idx: u64) -> (u64, u64) {
    if idx == 0 {
        (c::BLOCKDEV_INODE_MAX as u64, i32::MAX as u64)
    } else {
        let shard = idx - 1;
        let bits = 63 - fs.opts().shard_inode_numbers_bits.get() as u32;

        ((shard << bits).max(i32::MAX as u64 + 1),
         (shard << bits) | !(u64::MAX << bits))
    }
}

/// The cursor to allocate from - below 2^31 with @is_32bit, otherwise this
/// thread's shard's - updated to its current shard bits, and to its range:
/// wrapped, a new generation, if it's at the end. Queued as an update, so
/// what the caller does to it is committed. Its range, too.
fn alloc_cursor_get<'a, 't>(t: &TransAttempt<'a, 't>, is_32bit: bool)
    -> Result<(TransBkey<'a, 't, c::InodeAllocCursor>, u64, u64), BchError>
{
    let fs = t.fs();
    let idx = if is_32bit { 0 } else { 1 + fs.inode_shard_idx() };
    let (min, max) = cursor_range(fs, idx);

    let cursor_pos = pos(c::LoggedOpsInum::LOGGED_OPS_INUM_inode_cursors.0 as u64, idx);

    let cursor = t.bkey_get_mut::<c::InodeAllocCursor>(Btree::logged_ops, cursor_pos,
                                                       BtreeIterFlags::CACHED,
                                                       UpdateTriggerFlags::empty());
    let mut cursor = match cursor.found()? {
        Some(cursor) => cursor,
        None => {
            // No cursor there yet:
            let mut iter = BtreeIter::new(t, Btree::logged_ops, cursor_pos,
                                          BtreeIterFlags::INTENT | BtreeIterFlags::CACHED);
            iter.traverse(t)?;

            let mut k = t.bkey_alloc_typed::<c::InodeAllocCursor>()?;
            k.k_mut().p = cursor_pos;
            t.update(&iter, &k, UpdateTriggerFlags::empty())?;
            k
        }
    };

    let v = cursor.v_mut();
    v.bits = fs.opts().shard_inode_numbers_bits.get();

    if v.idx.get() < min {
        v.idx = min.into();
    }

    if v.idx.get() >= max {
        v.idx = min.into();
        v.generation = v.generation.get().wrapping_add(1).into();
    }

    Ok((cursor, min, max))
}

/// Give @inode a free inode number in @snapshot, below 2^31 with @is_32bit,
/// and its generation, and leave @iter there, for creating it: as
/// bch2_inode_create().
///
/// Free is no key, or an inode_generation key - left by a deleted inode -
/// visible in @snapshot; the new inode takes the higher of its generation
/// and the cursor's. From the cursor to the end of its range, then from the
/// start of it, once, in a new generation; ENOSPC_inode_create if it's full.
pub fn create<'a>(
    t:        &TransAttempt<'a, '_>,
    iter:     &mut BtreeIter<'a>,
    inode:    &mut c::bch_inode_unpacked,
    snapshot: u32,
    is_32bit: bool,
) -> Result<(), BchError> {
    let fs = t.fs();
    let (mut cursor, min, max) = alloc_cursor_get(t, is_32bit)?;
    let cursor = cursor.v_mut();

    let mut start = cursor.idx.get();
    let mut inum = start;

    *iter = BtreeIter::new(t, Btree::inodes, pos(0, inum),
                           BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::INTENT);
    loop {
        while inum < max {
            // Free, and if so the generation a deleted inode left:
            let free = match iter.peek_max(t, spos(0, inum, u32::MAX))? {
                None => Some(0),
                Some(k) if k.k.key_type() == BkeyType::inode_generation &&
                           crate::snapshots::snapshot::is_ancestor(t, snapshot, k.k.p.snapshot) =>
                    Some((k.arm::<c::InodeGeneration>().expect("a generation").v().bi_generation).get()),
                Some(_) => None,
            };

            if let Some(generation) = free {
                inode.bi_inum       = inum;
                inode.bi_generation = cursor.generation.get().max(generation);
                cursor.idx          = (inum + 1).into();

                iter.set_pos(spos(0, inum, snapshot));
                return iter.traverse(t);
            }

            inum += 1;
            iter.set_pos(pos(0, inum));
        }

        if start == min {
            return Err(fs.err(Errcode::ENOSPC_inode_create));
        }

        // Retry from start
        start = min;
        inum = min;
        iter.set_pos(pos(0, inum));
        cursor.generation = cursor.generation.get().wrapping_add(1).into();
    }
}

impl BkeyOps for c::InodeAllocCursor {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        bkey_fsck_err_on!(v, v.k.k.p.inode != c::LoggedOpsInum::LOGGED_OPS_INUM_inode_cursors.0 as u64,
                          id::inode_alloc_cursor_inode_bad, "k.p.inode bad")
    }

    /// The cursor, and the range it hands out.
    #[cold]
    fn to_text(out: &mut Printbuf, fs: Option<&Fs>, k: BkeySC<'_>) {
        let v = k.arm::<c::InodeAllocCursor>().expect("an inode alloc cursor").v();
        let idx = v.idx.get();

        // the range is the filesystem's options'
        if let Some(fs) = fs {
            let (min, max) = cursor_range(fs, k.k.p.offset);
            write!(out, "min {min} max {max} consumed {} ", idx.wrapping_sub(min));
        }
        write!(out, "idx {idx} generation {}", v.generation.get());
    }
}

// ── Versions in other snapshots, and the trigger ─────────────────────────
//
// An inode has a version in each snapshot it was changed in. The oldest
// version a snapshot sees is its "parent"; BCH_INODE_has_child_snapshot,
// on a version, says some descendant snapshot has its own. The trigger
// keeps that flag, the inode count, and the deleted_inodes btree - an
// entry for every unlinked inode that no descendant still has - up to date.

/// An unlinked inode that no descendant snapshot has a version of: one to
/// delete. As bkey_is_unlinked_inode().
fn is_unlinked(flags: c::InodeFlags) -> bool {
    flags.contains(c::InodeFlags::unlinked) && !flags.contains(c::InodeFlags::has_child_snapshot)
}

/// The version of the inode at @p that @p's snapshot inherited - the oldest
/// ancestor's - through @iter, left at it; None if there's none, as
/// bch2_inode_get_iter_snapshot_parent(). Skips non-inode keys, going on up
/// from each.
fn snapshot_parent<'i, 'a>(
    t:     &'i TransAttempt<'a, '_>,
    iter:  &'i mut BtreeIter<'a>,
    mut p: Bpos,
) -> Result<Option<BkeySC<'i, Inodes>>, BchError> {
    loop {
        *iter = BtreeIter::new(t, Btree::inodes, p.successor(),
                               BtreeIterFlags::ALL_SNAPSHOTS);

        let snapshot = p.snapshot;
        let Some(k) = iter.find_max_norestart(t, spos(p.inode, p.offset, u32::MAX), |_, k| {
            Ok(crate::snapshots::snapshot::is_ancestor(t, snapshot, k.k.p.snapshot))
        })? else {
            return Ok(None);
        };

        p = k.k.p;
        if let Some(k) = k.arm::<Inodes>() {
            // Polonius: @iter isn't touched again.
            return Ok(Some(unsafe { crate::btree::iter::polonius_key(k) }));
        }
    }
}

/// Whether the inode at @pos has versions in descendant snapshots - only
/// looked for if @pos's snapshot has any: as
/// bch2_inode_has_child_snapshots(). In the caller's transaction attempt.
pub fn has_child_snapshots(t: &TransAttempt<'_, '_>, p: Bpos) -> Result<bool, BchError> {
    // As C: if leafness can't be told, look.
    if let Ok(true) = t.fs().snapshots.is_leaf(p.snapshot) {
        return Ok(false);
    }

    let mut iter = BtreeIter::new(t, Btree::inodes, pos(0, p.offset),
                                  BtreeIterFlags::ALL_SNAPSHOTS);
    let found = iter.find_max_norestart(t, p.predecessor(), |_, k| {
        Ok(crate::snapshots::snapshot::is_ancestor(t, k.k.p.snapshot, p.snapshot) &&
           k.k.is_inode())
    })?;
    Ok(found.is_some())
}

/// The inode @k is first being written in this snapshot: if it has no
/// versions in descendants after all, its has_child_snapshot goes - and with
/// @have_child, it's set. As update_inode_has_children().
fn update_has_children(t: &TransAttempt<'_, '_>, mut k: BkeyS<'_, Inodes>, have_child: bool)
    -> Result<(), BchError>
{
    if !have_child && has_child_snapshots(t, k.k.p)? {
        return Ok(());
    }

    let mut f = k.flags();
    if have_child != f.contains(c::InodeFlags::has_child_snapshot) {
        f.set(c::InodeFlags::has_child_snapshot, have_child);
        k.set_flags(f);
    }
    Ok(())
}

/// The inode at @p was made or deleted: the version it inherited, in an
/// ancestor, has a child snapshot version - or with @have_child false, may
/// no longer. As update_parent_inode_has_children().
fn update_parent_has_children(t: &TransAttempt<'_, '_>, p: Bpos, have_child: bool)
    -> Result<(), BchError>
{
    let mut iter = BtreeIter::uninit();
    let Some(k) = snapshot_parent(t, &mut iter, p)? else { return Ok(()) };

    if !have_child && has_child_snapshots(t, k.k.p)? {
        return Ok(());
    }

    let mut f = k.flags();
    if have_child != f.contains(c::InodeFlags::has_child_snapshot) {
        // As bch2_bkey_make_mut(): a copy, queued as the update.
        let mut u = t.bkey_make_mut_noupdate(k)?;
        t.update(&iter, &u, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
        f.set(c::InodeFlags::has_child_snapshot, have_child);
        BkeyS::from(&mut u).set_flags(f);
    }
    Ok(())
}

/// The inode keys' trigger: as bch2_trigger_inode().
///
///  - Atomic, on insert: the key going in is stamped with the journal
///    sequence number it's committed in.
///  - Transactional or gc: the inode count goes up or down.
///  - Transactional: an inode becoming unlinked - and not kept by a
///    descendant - goes on the deleted_inodes list, and comes off it when
///    it's no longer; an inode made or deleted in a snapshot with a parent
///    updates has_child_snapshot on the version it inherited, and one first
///    written here, its own.
pub fn trigger(t: &TransAttempt<'_, '_>, op: &mut TriggerOp<'_>) -> Result<(), BchError> {
    let fs = t.fs();

    if op.flags.contains(UpdateTriggerFlags::ATOMIC) &&
       op.flags.contains(UpdateTriggerFlags::INSERT) {
        let seq = t.journal_res_seq();
        assert!(seq != 0, "atomic trigger without a journal reservation");
        op.new.as_s().arm::<Inodes>().expect("inserting an inode").set_journal_seq(seq);
    }

    let nr = op.new.k.is_inode() as i64 - op.old.k.is_inode() as i64;
    if op.flags.intersects(UpdateTriggerFlags::TRANSACTIONAL | UpdateTriggerFlags::GC) && nr != 0 {
        crate::accounting::add(t, crate::accounting::NrInodes {}, crate::accounting::NrInodesCounters { nr },
                               op.flags.contains(UpdateTriggerFlags::GC))?;
    }

    if op.flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        let unlinked = |k: BkeySC<'_>| k.arm::<Inodes>().is_some_and(|k| is_unlinked(k.flags()));
        let unlinked_delta = unlinked(op.new.as_sc()) as i32 - unlinked(op.old) as i32;
        if unlinked_delta != 0 {
            t.bit_mod_buffered(Btree::deleted_inodes, op.new.k.p, unlinked_delta > 0)?;
        }

        // Creating or deleting an inode at this snapshot ID, with maybe an
        // inode in a parent snapshot ID: we might need to set or clear the
        // has_child_snapshot flag on the parent.
        let deleted_delta = nr;
        if deleted_delta != 0 &&
           fs.snapshots.parent(op.new.k.p.snapshot).is_some() {
            update_parent_has_children(t, op.new.k.p, deleted_delta > 0)?;
        }

        // When an inode is first updated in a new snapshot, we may need to
        // clear has_child_snapshot
        if deleted_delta > 0 {
            update_has_children(t, op.new.as_s().arm::<Inodes>().expect("an inode made"), false)?;
        }
    }

    Ok(())
}

static INODE: Trigger = Trigger(trigger);

// ── Deleting inodes ──────────────────────────────────────────────────────
//
// The VFS deletes an inode it's done with through rm(); delete_dead_inodes()
// deletes those on the deleted_inodes list, at recovery. Either way,
// versions in ancestor snapshots go too, once nothing else sees them.

/// Whether inode @p may be deleted, unpacking it into @inode: an error, as
/// to why not, if not. As may_delete_deleted_inode().
///
/// From the deleted_inodes sweep - @from_deleted_inodes - a list entry for
/// an inode that shouldn't be on it is an fsck error, and comes off; false
/// then, and for an inode left on a clean filesystem that fsck says to leave.
/// A has_child_snapshot flag found missing is set: from rm(), that's
/// committed, and an error.
fn may_delete_deleted_inode(
    t:                   &TransAttempt<'_, '_>,
    p:                   Bpos,
    inode:               &mut c::bch_inode_unpacked,
    from_deleted_inodes: bool,
) -> Result<bool, BchError> {

    let fs = t.fs();
    let delete = || t.bit_mod_buffered(Btree::deleted_inodes, p, false).map(|_| false);

    let mut iter = BtreeIter::new(t, Btree::inodes, p, BtreeIterFlags::CACHED);
    let k = iter.peek_slot(t)?.expect("a slot always has a key");
    let k_pos = k.k.p;

    let k = k.arm::<Inodes>();
    if fsck_err_on!(t, from_deleted_inodes && k.is_none(), id::deleted_inode_missing,
                    "nonexistent inode {}:{} in deleted_inodes btree", { p.offset }, { p.snapshot })? {
        return delete();
    }
    let Some(k) = k else {
        return Err(fs.err(Errcode::ENOENT_inode));
    };

    *inode = unpack(fs, k);

    // Subvolume roots are deleted by the subvolume deletion path, never the
    // inode reaper (see is_subvolume_root()). A deleted_inodes entry for one
    // is expected, not damage - the trigger enrolls any inode transitioning
    // to unlinked - but consuming it here would race the snapshot sweep;
    // crash recovery for subvolume deletion is check_subvols(), keyed off
    // SUBVOLUME_STATE_unlinked. Just drop the entry:
    if inode.is_subvolume_root() {
        if from_deleted_inodes {
            return delete();
        }
        return Err(fs.err(Errcode::inode_is_subvolume_root));
    }

    if inode.is_dir() {
        let ret = dirent::empty_dir_snapshot(t, p.offset, 0, p.snapshot);
        let not_empty = matches!(&ret, Err(e) if e.matches(c::ENOTEMPTY));
        if fsck_err_on!(t, from_deleted_inodes && not_empty, id::deleted_inode_is_dir,
                        "non empty directory {}:{} in deleted_inodes btree",
                        { p.offset }, { p.snapshot })? {
            return delete();
        }
        ret?;
    }

    let not_unlinked = (!inode.flag(c::InodeFlags::unlinked))
        .then(|| fs.err(Errcode::inode_not_unlinked));
    if fsck_err_on!(t, from_deleted_inodes && not_unlinked.is_some(), id::deleted_inode_not_unlinked,
                    "non-deleted inode {}:{} in deleted_inodes btree", { p.offset }, { p.snapshot })? {
        return delete();
    }
    if let Some(e) = not_unlinked {
        return Err(e);
    }

    let has_child = inode.flag(c::InodeFlags::has_child_snapshot)
        .then(|| fs.err(Errcode::inode_has_child_snapshot));
    if fsck_err_on!(t, from_deleted_inodes && has_child.is_some(),
                    id::deleted_inode_has_child_snapshots,
                    "inode with child snapshots {}:{} in deleted_inodes btree",
                    { p.offset }, { p.snapshot })? {
        return delete();
    }
    if let Some(e) = has_child {
        return Err(e);
    }

    if has_child_snapshots(t, k_pos)? {
        if fsck_err!(t, id::inode_has_child_snapshots_wrong,
                     "inode has_child_snapshots flag wrong (should be set)\n{}", inode)? {
            inode.set_flag(c::InodeFlags::has_child_snapshot, true);
            fsck_write(t, inode)?;
        }

        if !from_deleted_inodes {
            t.commit(None, CommitFlags::NO_ENOSPC)?;
            return Err(fs.err(Errcode::inode_has_child_snapshot));
        }

        return delete();
    }

    if from_deleted_inodes {
        if fs.flags.get(c::FsFlag::clean_recovery) &&
           !fsck_err!(t, id::deleted_inode_but_clean,
                      "filesystem marked as clean but have deleted inode {}:{}",
                      { p.offset }, { p.snapshot })? {
            return Ok(false);
        }

        return Ok(true);
    }

    Ok(false)
}

/// may_delete_deleted_inode(), for inode @inum: as may_delete_deleted_inum().
fn may_delete_deleted_inum(t: &TransAttempt<'_, '_>, inum: c::subvol_inum,
                           inode: &mut c::bch_inode_unpacked) -> Result<bool, BchError> {
    let snapshot = subvolume::get_snapshot(t, inum.subvol as u32)?;
    may_delete_deleted_inode(t, spos(0, inum.inum, snapshot), inode, false)
}

/// Delete inode @inum's keys in @btree, a commit each: as
/// bch2_inode_delete_keys(). An extent is deleted whole.
fn delete_keys(trans: &TransGuard<'_>, inum: c::subvol_inum, btree: Btree)
    -> Result<(), BchError>
{
    let fs = trans.fs();

    // Deleting a compressed extent that straddles a snapshot boundary splits
    // it, and the commit charges the split's extra_disk_res to our disk
    // reservation - so pass one in, or that charge dereferences a NULL
    // disk_reservation.
    let res = crate::alloc::capacity::DiskReservation::new(fs);

    let mut iter = BtreeIter::new(trans, btree, pos(inum.inum, 0),
                                  BtreeIterFlags::INTENT | BtreeIterFlags::TRIM);

    subvolume::for_each_in_subvolume_max_commit(trans, &mut iter, pos(inum.inum, u64::MAX), inum.subvol as u32,
                                                BtreeIterFlags::empty(), Some(&res), CommitFlags::NO_ENOSPC,
                                                |t, iter, k| {
        let mut delete = t.bkey_alloc_init::<c::Deleted>(0, iter.pos())?;
        if iter.is_extents() {
            delete.k_mut().resize(k.k.size);
        }
        t.update(iter, &delete, UpdateTriggerFlags::empty())
    })
}

/// Delete the inode @inum's key, and its damage record; its snapshot. As
/// bch2_inode_rm_trans(). It not being there is an inconsistency.
fn rm_trans(t: &TransAttempt<'_, '_>, inum: c::subvol_inum) -> Result<u32, BchError> {
    let fs = t.fs();
    let snapshot = subvolume::get_snapshot(t, inum.subvol as u32)?;

    let mut iter = BtreeIter::new(t, Btree::inodes, spos(0, inum.inum, snapshot),
                                  BtreeIterFlags::INTENT | BtreeIterFlags::CACHED);
    let is_inode = iter.peek_slot(t)?.expect("a slot always has a key").k.is_inode();

    if !is_inode {
        fs_inconsistent!(fs, "inode {}:{} not found when deleting", inum.inum, snapshot);
        return Err(fs.err(Errcode::ENOENT_inode));
    }

    t.delete_at(&iter, UpdateTriggerFlags::empty())?;
    crate::init::damage::delete(t, inum.inum, snapshot)?;
    Ok(snapshot)
}

/// Delete inode @inum, which the VFS is done with - its keys, then it, then
/// the versions in ancestor snapshots nothing else sees: as bch2_inode_rm().
/// The VFS asking for an inode that may not be deleted is a bug, logged
/// with the inode, and counted.
pub fn rm(fs: &Fs, inum: c::subvol_inum) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);

    let mut inode = c::bch_inode_unpacked::default();
    let mut ret = lockrestart_do(&trans, |t| may_delete_deleted_inum(t, inum, &mut inode));

    if matches!(&ret, Err(e) if !e.matches(c::EIO) && !e.matches(c::EROFS)) {
        let mut buf = Printbuf::new();
        writeln!(buf, "VFS incorrectly tried to delete inode");
        {
            let mut buf = buf.indent(2);
            // For the message: not finding the path isn't an error.
            let _ = lockrestart_do(&trans, |t| crate::namei::inum_to_path(t, inum, &mut buf));
            buf.newline();
            inode.to_text(&mut buf);
        }

        ret = bch_err_msg!(fs, ret, "{}", buf);
        crate::init::error::sb_error_count(fs, id::vfs_bad_inode_rm);
    }
    ret?;

    // If this was a directory, there shouldn't be any real dirents left -
    // but there could be whiteouts (from hash collisions) that we should
    // delete:
    //
    // XXX: the dirent code ideally would delete whiteouts when they're no
    // longer needed
    delete_keys(&trans, inum, if !inode.is_dir() {
        Btree::extents
    } else {
        Btree::dirents
    })?;
    delete_keys(&trans, inum, Btree::xattrs)?;

    let mut snapshot = 0;
    commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| {
        snapshot = rm_trans(t, inum)?;
        Ok(())
    })?;

    delete_ancestor_snapshot_inodes(&trans, spos(0, inum.inum, snapshot))
}

/// Delete inode @inum in @snapshot and its contents there - each content
/// btree's in its own loop - then the inode, and its damage record: as
/// __bch2_inode_rm_snapshot(). If content couldn't all be deleted, the inode
/// is whited out instead - so the ancestor's version doesn't resurface
/// here, and deletion or fsck can still find this position - and the error
/// returned.
fn __rm_snapshot(trans: &BtreeTrans<'_>, inum: u64, snapshot: u32) -> Result<(), BchError> {
    let fs = trans.fs();
    let mut ret = Ok(());

    for btree in [Btree::extents, Btree::dirents, Btree::xattrs] {
        let r = match trans.delete_range(btree, spos(inum, 0, snapshot), spos(inum, u64::MAX, snapshot),
                                         UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE) {
            // handled restarts are an FYI - the range was fully deleted:
            Err(e) if is_restart(&e) => Ok(()),
            r => r,
        };
        if ret.is_ok() {
            ret = r;
        }
    }

    if ret.is_ok() {
        return commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| {
            t.delete(Btree::inodes, spos(0, inum, snapshot),
                     UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
            crate::init::damage::delete(t, inum, snapshot)
        });
    }

    // Content left behind: whiteout the inode key instead of deleting it, so
    // the ancestor's version doesn't resurface here and the deletion
    // scan/fsck can still find this position:
    let ret = bch_err_msg!(fs, ret, "deleting content of inode {inum}:{snapshot}, leaving whiteout");

    commit_do(trans, None, CommitFlags::NO_ENOSPC, |t| {
        let whiteout = t.bkey_alloc_init::<c::Whiteout>(0,
                                         spos(0, inum, snapshot))?;
        t.insert(Btree::inodes, whiteout, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
        crate::init::damage::delete(t, inum, snapshot)
    })?;
    ret
}

/// Whether the inode at @pos, or a version of it in a descendant snapshot,
/// is open in the VFS: the inode cache's answer. Never, in userspace.
#[cfg(kernel)]
pub fn or_descendents_is_open(t: &TransAttempt<'_, '_>, pos: Bpos) -> Result<bool, BchError> {
    crate::vfs::inode_cache::or_descendents_is_open(t, pos)
}

#[cfg(not(kernel))]
pub fn or_descendents_is_open(_t: &TransAttempt<'_, '_>, _pos: Bpos) -> Result<bool, BchError> {
    Ok(false)
}

/// After deleting an inode, there may be versions in older snapshots that
/// should also be deleted - if they're not referenced by sibling snapshots
/// and not open in other subvolumes: as delete_ancestor_snapshot_inodes().
fn delete_ancestor_snapshot_inodes(trans: &BtreeTrans<'_>, mut p: Bpos) -> Result<(), BchError> {
    loop {
        let parent = lockrestart_do(trans, |t| {
            let mut iter = BtreeIter::uninit();
            Ok(snapshot_parent(t, &mut iter, p)?.map(|k| (k.k.p, is_unlinked(k.flags()))))
        })?;

        let Some((parent, true)) = parent else { return Ok(()) };

        p = parent;
        if lockrestart_do(trans, |t| or_descendents_is_open(t, p))? {
            return Ok(());
        }

        __rm_snapshot(trans, p.offset, p.snapshot)?;
    }
}

/// Delete inode @inum in @snapshot, its contents, and the ancestors' versions
/// nothing else sees - committing as it goes, in iterations of its own: as
/// bch2_inode_rm_snapshot(). From inside an attempt, through
/// TransAttempt::close(), which returns the transaction_restart_nested C's
/// returned itself.
pub fn rm_snapshot(trans: &BtreeTrans<'_>, inum: u64, snapshot: u32) -> Result<(), BchError> {
    __rm_snapshot(trans, inum, snapshot)?;
    delete_ancestor_snapshot_inodes(trans, spos(0, inum, snapshot))
}

/// Delete the inodes on the deleted_inodes list that may be deleted, a
/// recovery pass: as bch2_delete_dead_inodes().
///
/// If check_inodes ran, unlinked inodes will have already been cleaned up
/// but the write buffer will be out of sync; therefore we always need a
/// write buffer flush.
///
/// Each entry is decided on, and what that queued committed, in an attempt of
/// its own; a deletion - rm_snapshot(), attempts of its own - runs after it,
/// and the walk goes on, not back: the list entry is still there until the
/// write buffer is flushed, so going back would find it again. C's loop said
/// this with a continue in the body of for_each_btree_key_commit(), which
/// skips the commit and goes on to the next key.
fn delete_dead_inodes(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    trans.write_buffer_flush_sync()?;

    let flags = BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS;
    let mut iter = BtreeIter::new(&trans, Btree::deleted_inodes, POS_MIN, flags);

    loop {
        let next = lockrestart_do(&trans, |t| {
            let Some(k) = iter.peek_max_type(t, SPOS_MAX, flags)? else { return Ok(None) };
            let p = k.k.p;

            let mut inode = c::bch_inode_unpacked::default();
            let delete = may_delete_deleted_inode(t, p, &mut inode, true)?;

            // may_delete_deleted_inode() can have queued updates - an fsck
            // error's log entry - which rm_snapshot()'s begins would drop
            t.commit(None, CommitFlags::NO_ENOSPC)?;
            Ok(Some((p, delete)))
        })?;

        let Some((p, delete)) = next else { return Ok(()) };
        if delete {
            bch_verbose_ratelimited!(fs, "deleting unlinked inode {}:{}", { p.offset }, { p.snapshot });
            match rm_snapshot(&trans, p.offset, p.snapshot) {
                Err(e) if is_restart(&e) => continue,
                r => r?,
            }
        }

        if !iter.advance() {
            return Ok(());
        }
    }
}

crate::recovery_pass!(delete_dead_inodes => delete_dead_inodes);

/// Delete the inode_generation keys in the inodes btree, a recovery pass: as
/// bch2_kill_i_generation_keys().
fn kill_i_generation_keys(fs: &Fs) -> Result<(), BchError> {
    let progress = Progress::recovery(fs, c"bch2_kill_i_generation_keys",
                                      &[Btree::inodes], &[]);
    let trans = crate::btree_trans!(fs);

    let mut iter = BtreeIter::new(&trans, Btree::inodes, POS_MIN,
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
        progress.update(t, iter)?;
        if k.k.key_type() == BkeyType::inode_generation {
            t.delete_at(iter, UpdateTriggerFlags::empty())?;
        }
        Ok(())
    })
}

crate::recovery_pass!(kill_i_generation_keys => kill_i_generation_keys);

// ── New inodes ───────────────────────────────────────────────────────────
//
// In two steps: init_early(), what doesn't depend on the parent directory -
// the VFS's create does it before its transaction - then init_late(), what
// does, once the transaction has read the parent (namei::create_trans()).

/// Start a new inode: zeroed, but for its hash type - per the str_hash
/// option - and a random hash seed. As bch2_inode_init_early().
pub fn init_early(fs: &Fs, inode: &mut c::bch_inode_unpacked) {
    *inode = c::bch_inode_unpacked::default();
    inode.set_str_hash(crate::str_hash::new_inode_type(fs));
    inode.bi_hash_seed = random_u64().into();
}

/// Finish a new inode, made at @now, in directory @parent: it inherits
/// @parent's inode options, and from a setgid @parent its group - and
/// setgid itself, for a directory. Only a directory is casefolded; if it is,
/// it has a case insensitive directory below it - itself. As
/// bch2_inode_init_late().
#[allow(clippy::too_many_arguments)]
pub fn init_late(
    fs:     &Fs,
    inode:  &mut c::bch_inode_unpacked,
    now:    u64,
    uid:    c::uid_t,
    gid:    c::gid_t,
    mode:   c::umode_t,
    rdev:   c::dev_t,
    parent: Option<&c::bch_inode_unpacked>,
) {
    inode.bi_mode  = mode;
    inode.bi_uid   = uid;
    inode.bi_gid   = gid;
    inode.bi_dev   = rdev as _; // as C: bi_dev is 32 bits, dev_t 64 in userspace
    inode.bi_atime = now;
    inode.bi_mtime = now;
    inode.bi_ctime = now;
    inode.bi_otime = now;

    if let Some(parent) = parent {
        if parent.bi_mode as u32 & c::S_ISGID != 0 {
            inode.bi_gid = parent.bi_gid;
            if inode.is_dir() {
                inode.bi_mode |= c::S_ISGID as c::umode_t;
            }
        }

        macro_rules! inherit {
            ($(($name:ident, $($rest:tt)*)),* $(,)?) => { crate::paste! {
                $(inode.[<bi_ $name>] = parent.[<bi_ $name>];)*
            } };
        }
        c::InodeOptId_arms!(inherit);
    }

    if !inode.is_dir() {
        inode.bi_casefold = 0;
    }

    if inode.casefold(fs) {
        inode.set_flag(c::InodeFlags::has_case_insensitive, true);
    }
}

/// A new inode, made now: init_early(), then init_late().
pub fn init(
    fs:     &Fs,
    uid:    c::uid_t,
    gid:    c::gid_t,
    mode:   c::umode_t,
    rdev:   c::dev_t,
    parent: Option<&c::bch_inode_unpacked>,
) -> c::bch_inode_unpacked {
    let mut inode = c::bch_inode_unpacked::default();
    init_early(fs, &mut inode);
    init_late(fs, &mut inode, fs.current_time(), uid, gid, mode, rdev, parent);
    inode
}

// ── Options and casefolding ──────────────────────────────────────────────

/// The unicode map directory @inode's names are casefolded with, or NULL if
/// it isn't casefolded: for its bch_hash_info.
pub fn cf_encoding(fs: &Fs, inode: &c::bch_inode_unpacked) -> *mut c::unicode_map {
    if inode.casefold(fs) {
        fs.cf_encoding.load(core::sync::atomic::Ordering::Relaxed)
    } else {
        core::ptr::null_mut()
    }
}

/// Set casefolding on directory @inode, @inum, to @v: it has to be empty -
/// its dirents would need rehashing - and the filesystem able to casefold,
/// which marks the filesystem as using it.
pub fn set_casefold(
    t:     &TransAttempt<'_, '_>,
    inum:  c::subvol_inum,
    inode: &mut c::bch_inode_unpacked,
    v:     u32,
) -> Result<(), BchError> {
    let fs = t.fs();

    if let Err(e) = fs.casefold_enabled() {
        bch_err_ratelimited!(fs, "Cannot enable casefolding: {e}");
        return Err(e);
    }

    // Not supported on individual files.
    if !inode.is_dir() {
        return Err(fs.err(Errcode::casefold_opt_is_dir_only));
    }

    dirent::empty_dir_trans(t, inum)?;
    fs.request_incompat_feature(
        MetadataVersion::casefolding)?;

    fs.check_set_feature(SbFeature::casefolding);

    inode.bi_casefold = v.wrapping_add(1) as _; // stored +1, 0 for unset
    crate::inode_opts::InodeOpt::of(c::InodeOptId::casefold).set_own(inode, true);

    namei::maybe_propagate_has_case_insensitive(t, inum, inode)
}

/// @inode's reconcile options - the ones reconcile moves its data to match,
/// and for each whether @inode set it or inherited it. An option too big for
/// its field in bch_extent_reconcile - only a corrupt inode's - is warned
/// about and truncated, as C's bitfield assignments truncate: the write it's
/// read for goes ahead, the one that corrects it too.
pub fn reconcile_opts_get(fs: &Fs, inode: &c::bch_inode_unpacked) -> c::bch_extent_reconcile {
    let opts = crate::inode_opts::get_inode(fs, inode);

    for (name, v) in c::bch_extent_reconcile::too_big(&opts) {
        bch_warn!(fs, "inode {}: io option {name} = {v} doesn't fit in bch_extent_reconcile, truncated",
                  inode.bi_inum);
    }
    c::bch_extent_reconcile::from_io_opts(&opts)
}

// ── The unpacked inode ───────────────────────────────────────────────────

impl c::bch_inode_unpacked {
    /// The link count as the VFS counts it - bi_nlink leaves out the links
    /// every inode has, two for a directory - and 0 if it's unlinked: as
    /// bch2_inode_nlink_get().
    pub fn nlink(&self) -> u32 {
        if self.flag(c::InodeFlags::unlinked) {
            0
        } else {
            self.bi_nlink.wrapping_add(self.nlink_bias())
        }
    }

    /// Set the link count, as nlink() counts it; 0 marks the inode unlinked.
    pub fn set_nlink(&mut self, nlink: u32) {
        if nlink != 0 {
            self.bi_nlink = nlink.wrapping_sub(self.nlink_bias());
            self.set_flag(c::InodeFlags::unlinked, false);
        } else {
            self.bi_nlink = 0;
            self.set_flag(c::InodeFlags::unlinked, true);
        }
    }

    /// Whether it's casefolded - set on it, or the filesystem's option: as
    /// bch2_inode_casefold(). Options are stored +1, 0 for unset.
    pub fn casefold(&self, fs: &Fs) -> bool {
        match self.bi_casefold {
            0 => fs.opts().casefold.get() != 0,
            v => v - 1 != 0,
        }
    }

    /// The links bi_nlink leaves out: as nlink_bias().
    fn nlink_bias(&self) -> u32 {
        if self.is_dir() { 2 } else { 1 }
    }

    /// Whether flag @f (BCH_INODE_*) is set.
    pub fn flag(&self, f: c::InodeFlags) -> bool {
        self.flags().contains(f)
    }

    pub fn set_flag(&mut self, f: c::InodeFlags, v: bool) {
        self.update_flags(|mut flags| { flags.set(f, v); flags })
    }

    /// Whether it records the dirent naming it (bi_dir, bi_dir_offset).
    pub fn has_backpointer(&self) -> bool {
        self.bi_dir != 0 || self.bi_dir_offset != 0
    }

    /// Forget the dirent naming this inode: the backpointer is the pair
    /// (bi_dir, bi_dir_offset), cleared together or not at all.
    pub fn clear_backpointer(&mut self) {
        self.bi_dir        = 0;
        self.bi_dir_offset = 0;
    }

    pub fn is_dir(&self) -> bool {
        self.bi_mode as u32 & c::S_IFMT == c::S_IFDIR
    }

    /// Whether it's a subvolume's root: deleting it is subvolume deletion's
    /// job - see bch2_inode_is_subvolume_root().
    pub fn is_subvolume_root(&self) -> bool {
        self.bi_subvol != 0
    }

    /// The dirent type naming it: as inode_d_type().
    pub fn d_type(&self) -> u8 {
        if self.bi_subvol != 0 {
            c::DT_SUBVOL as u8
        } else {
            ((self.bi_mode >> 12) & 15) as u8
        }
    }

    /// What a dirent naming it points at: a subvolume root by subvolume,
    /// linked from its parent subvolume; anything else by inode number.
    pub fn dirent_target(&self) -> DirentTarget {
        if self.bi_subvol != 0 {
            DirentTarget::Subvol { child: self.bi_subvol, parent: self.bi_parent_subvol }
        } else {
            DirentTarget::Inode(self.bi_inum)
        }
    }

    /// Whether it counts towards its parent's link count: a directory, but not
    /// a subvolume root, which is named by a DT_SUBVOL dirent: as
    /// is_subdir_for_nlink().
    pub fn is_subdir_for_nlink(&self) -> bool {
        self.is_dir() && self.bi_subvol == 0
    }

    /// Whether any per-inode option is set - what BCH_INODE_has_inode_opts
    /// records.
    pub fn has_opts(&self) -> bool {
        macro_rules! any_set {
            ($(($name:ident, $($rest:tt)*)),* $(,)?) => { crate::paste! {
                false $(|| self.[<bi_ $name>] != 0)*
            } };
        }
        c::InodeOptId_arms!(any_set)
    }
}

/// The most links an inode can have, as the VFS counts them.
const BCH_LINK_MAX: u32 = u32::MAX;

/// One more link to @inode - or none less, for an unlinked inode coming
/// back: as bch2_inode_nlink_inc(). too_many_links at BCH_LINK_MAX.
pub fn nlink_inc(t: &TransAttempt<'_, '_>, inode: &mut c::bch_inode_unpacked)
    -> Result<(), BchError>
{
    if inode.flag(c::InodeFlags::unlinked) {
        inode.set_flag(c::InodeFlags::unlinked, false);
    } else {
        if inode.bi_nlink == BCH_LINK_MAX - inode.nlink_bias() {
            return Err(BchError::from(Errcode::too_many_links));
        }
        nlink_add(t, inode, 1)?;
    }
    Ok(())
}

/// @inode's raw link count moved by @delta - a directory's subdirectory count,
/// or a link the caller accounts for itself - held at 0 or the maximum instead
/// of wrapping. A count that would wrap is already wrong, read that way off
/// disk, and the arithmetic must not panic on it: report it, and schedule the
/// pass that recounts it - check_dirents for a directory, check_nlinks
/// otherwise. In fsck, if that pass has already run, scheduling it returns the
/// error that rewinds recovery to it, for the caller to pass up.
pub fn nlink_add(t: &TransAttempt<'_, '_>, inode: &mut c::bch_inode_unpacked, delta: i32)
    -> Result<(), BchError>
{
    match inode.bi_nlink.checked_add_signed(delta) {
        Some(n) => {
            inode.bi_nlink = n;
            Ok(())
        }
        None => nlink_out_of_range(t, inode, delta),
    }
}

#[cold]
fn nlink_out_of_range(t: &TransAttempt<'_, '_>, inode: &mut c::bch_inode_unpacked, delta: i32)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let old = inode.bi_nlink;
    inode.bi_nlink = if delta < 0 { 0 } else { u32::MAX };

    let (err, pass) = if inode.is_dir() {
        (id::inode_dir_wrong_nlink, RecoveryPass::check_dirents)
    } else {
        (id::inode_wrong_nlink, RecoveryPass::check_nlinks)
    };

    let mut msg = LogMsg::new(fs);
    writeln!(msg, "inode {}:{} link count {old} {delta:+} out of range, held at {}",
             inode.bi_inum, inode.bi_snapshot, inode.bi_nlink);
    let log = crate::init::error::count_fsck_err(fs, err, &mut msg);
    msg.set_suppressed(!log);

    passes::run_explicit(fs, &mut msg, pass, c::RunRecoveryPassFlags::from_bits_retain(0))
}

/// One link fewer to @inode - unlinked, when it had none left: as
/// bch2_inode_nlink_dec(). An unlinked inode losing a link is a transaction
/// inconsistency, and changes nothing.
pub fn nlink_dec(t: &TransAttempt<'_, '_>, inode: &mut c::bch_inode_unpacked) {
    let unlinked = inode.flag(c::InodeFlags::unlinked);

    if unlinked && inode.bi_nlink != 0 {
        trans_inconsistent!(t, "inode {} unlinked but link count nonzero", inode.bi_inum);
        return;
    }

    if unlinked {
        trans_inconsistent!(t, "inode {} link count underflow", inode.bi_inum);
        return;
    }

    if inode.bi_nlink != 0 {
        inode.bi_nlink -= 1;
    } else {
        inode.set_flag(c::InodeFlags::unlinked, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> c::bch_inode_unpacked {
        c::bch_inode_unpacked {
            bi_inum:      4096,
            bi_hash_seed: 0x0123_4567_89ab_cdef.into(),
            bi_size:      12345,
            bi_mode:      0o100644,
            bi_atime:     0x1234_5678_9abc,
            bi_ctime:     7,
            bi_uid:       1000,
            bi_nlink:     3,
            bi_dir:       4097,
            bi_casefold:  1,
            ..Default::default()
        }
    }

    /// Set the key's value to @fields_bytes of packed fields, @nr_fields of
    /// them.
    fn set_fields_len(k: &mut c::bkey_inode_buf, fields_bytes: usize, nr_fields: u8) {
        k.inode.k.u64s = (BKEY_U64S + (V3_FIELDS_OFFSET + fields_bytes).div_ceil(8)) as u8;
        k.inode.v.set_nr_fields(nr_fields);
    }

    #[test]
    fn pack_unpack_round_trip() {
        let u = sample();
        let packed = pack(&u);
        let v = try_unpack(BkeySC::from(&packed)).expect("unpacks");

        assert_eq!(v.bi_inum, u.bi_inum);
        assert_eq!(v.bi_hash_seed, u.bi_hash_seed);
        assert_eq!(v.bi_size, u.bi_size);
        assert_eq!(v.bi_mode, u.bi_mode);
        assert_eq!(v.bi_atime, u.bi_atime);
        assert_eq!(v.bi_uid, u.bi_uid);
        assert_eq!(v.bi_nlink, u.bi_nlink);
        assert_eq!(v.bi_dir, u.bi_dir);
        assert_eq!(v.bi_casefold, u.bi_casefold);
        // An option field is set - bi_casefold - so pack sets has_inode_opts:
        assert!(v.flag(c::InodeFlags::has_inode_opts));
    }

    /// A value cut off at a field: Err at that field, the fields before it
    /// decoded, it and the rest zero, the fixed fields intact.
    #[test]
    fn unpack_truncated_field() {
        let u = sample();
        let mut packed = pack(&u);

        // bi_atime: 45 bits, a 7 byte varint, then the high half's 1 byte -
        // so the first u64 of fields holds exactly field 0:
        let nr_fields = packed.inode.v.nr_fields();
        set_fields_len(&mut packed, 8, nr_fields);

        let (v, fieldnr) = try_unpack(BkeySC::from(&packed))
            .expect_err("a truncated value doesn't unpack");

        assert_eq!(fieldnr, 1);
        assert_eq!(v.bi_atime, u.bi_atime);
        assert_eq!(v.bi_ctime, 0);
        assert_eq!(v.bi_uid, 0);
        assert_eq!(v.bi_nlink, 0);
        assert_eq!(v.bi_inum, u.bi_inum);
        assert_eq!(v.bi_size, u.bi_size);
        assert_eq!(v.bi_mode, u.bi_mode);
    }

    /// A field whose value doesn't fit it: Err at that field.
    #[test]
    fn unpack_field_too_big() {
        let mut packed = pack(&c::bch_inode_unpacked { bi_inum: 4096, ..Default::default() });

        // The four 96 bit timestamps, zero: two bytes each. Then bi_uid,
        // 32 bits, given 2^40:
        let mut len = 8;
        packed._pad[..len].fill(0);
        len += varint::encode(&mut packed._pad[len..], 1 << 40);
        set_fields_len(&mut packed, len, 5);

        let (v, fieldnr) = try_unpack(BkeySC::from(&packed))
            .expect_err("an oversized field doesn't unpack");
        assert_eq!(fieldnr, 4);
        assert_eq!(v.bi_uid, 0);
    }

    #[test]
    fn v1_field_encodings() {
        // One byte: high bit the marker, the rest the value.
        assert_eq!(v1_field(&[0x85], 32), Some((5, 1)));
        // Two bytes: the second highest bit is the marker.
        assert_eq!(v1_field(&[0x41, 0x02], 32), Some((0x0102, 2)));
        // Zero first byte: no marker.
        assert_eq!(v1_field(&[0x00, 0x01], 32), None);
        // A two byte field with one byte left.
        assert_eq!(v1_field(&[0x41], 32), None);
        assert_eq!(v1_field(&[], 32), None);
    }
}
