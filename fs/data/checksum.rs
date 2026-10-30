// SPDX-License-Identifier: GPL-2.0

//! Checksums and encryption - data/checksum.c, in Rust - and the key they're
//! under.
//!
//! A checksum is computed by a Csum, over its data's pieces in order - a
//! buffer, or a bio's chunks: a crc's seed, xxhash's state, or a Poly1305
//! MAC's, an encryption type's checksum. Encryption is ChaCha20, a Chacha,
//! under the filesystem's key: its own inverse. The MAC is keyed by ChaCha20
//! under the same key, from the nonce with BCH_NONCE_POLY set. The primitives
//! are the kernel's lib/crypto, xxhash and crcs - in userspace include/'s, the
//! crypto over libsodium.
//!
//! A nonce is four little-endian words, on-disk format: decrypting takes the
//! nonce the data was encrypted with. The top bits of the last are its
//! domain, what's encrypted with it, so that btree nodes, journal entries and
//! extents never share one; the first counts ChaCha20 blocks, which add()
//! advances. A domain's nonce is made where its structure is: bset::nonce()
//! in btree/read.rs, jset::nonce() in journal/read.rs, extent_nonce() in
//! data/ptrs.rs. C has its own in data/checksum.h: BCH_NONCE_*, nonce_add().
//!
//! The key is in the superblock's crypt section, encrypted under the user's
//! key unless no passphrase was ever set - the user's derived from the
//! passphrase by scrypt, its parameters in the section. At mount it's
//! decrypted with the user's key: handed to us at mount, or found in
//! the keyring under "bcachefs:<uuid>", or, in userspace, from a passphrase
//! prompted for.

use crate::sb_err_on;
use crate::c;
use crate::util::vstructs::{bytes_of, Unsized, Unvalidated};
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::sb::io::{SbFieldOps, SbResize};
use crate::types::{Le32, Le64};
use crate::util::bio::{Bio, BioRange};
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint, c_void};
use cstruct_macros::c_export;

// ── Nonces ───────────────────────────────────────────────────────────────

/// ChaCha20's block, in bytes: what a nonce's first word counts.
pub const CHACHA_BLOCK_SIZE: u32 = 64;

/// The domains, in a nonce's last word.
pub const BCH_NONCE_EXTENT:	u32 = 1 << 28;
pub const BCH_NONCE_BTREE:	u32 = 2 << 28;
pub const BCH_NONCE_JOURNAL:	u32 = 3 << 28;
pub const BCH_NONCE_PRIO:	u32 = 4 << 28;
pub const BCH_NONCE_POLY:	u32 = 1 << 31;

/// The nonce @offset bytes further into its stream - a whole number of
/// ChaCha20 blocks: C's nonce_add().
impl core::ops::Add<u32> for c::nonce {
    type Output = Self;

    fn add(mut self, offset: u32) -> Self {
        debug_assert!(offset % CHACHA_BLOCK_SIZE == 0,
                      "nonce offset {offset}: not a whole number of ChaCha20 blocks");
        self.d[0] = Le32::new(self.d[0].get().wrapping_add(offset / CHACHA_BLOCK_SIZE));
        self
    }
}

// ── Checksums ────────────────────────────────────────────────────────────

/// crypto/poly1305.h's, in bytes.
const POLY1305_KEY_SIZE: usize = 32;
const POLY1305_DIGEST_SIZE: usize = 16;

/// A checksum being computed, over its data's pieces in order: C's
/// bch2_checksum_state, and Poly1305's alongside. A crc is its seed so far
/// and what that's XORed with at the end: all ones for the _nonzero types,
/// which start there too.
enum Csum {
    /// none, and a type this version doesn't know: zero, or a merge's seed
    None(u64),
    Crc32c { seed: u64, xor: u64 },
    Crc64 { seed: u64, xor: u64 },
    Xxhash(c::xxh64_state),
    /// and how many bytes of its digest the type keeps
    Poly1305(c::poly1305_desc_ctx, usize),
}

impl Csum {
    /// An encryption type's is keyed by @fs's key: panics without one.
    fn new(fs: Option<&Fs>, ty: c::bch_csum_type, mut nonce: c::nonce) -> Self {
        use c::bch_csum_type as t;
        match ty {
            t::BCH_CSUM_crc32c          => Csum::Crc32c { seed: 0, xor: 0 },
            t::BCH_CSUM_crc32c_nonzero  => Csum::Crc32c { seed: u32::MAX.into(), xor: u32::MAX.into() },
            t::BCH_CSUM_crc64           => Csum::Crc64 { seed: 0, xor: 0 },
            t::BCH_CSUM_crc64_nonzero   => Csum::Crc64 { seed: u64::MAX, xor: u64::MAX },
            t::BCH_CSUM_xxhash => {
                let mut s = c::xxh64_state::default();
                // SAFETY: a state, to set up
                unsafe { c::xxh64_reset(&mut s, 0) };
                Csum::Xxhash(s)
            }
            t::BCH_CSUM_chacha20_poly1305_80 | t::BCH_CSUM_chacha20_poly1305_128 => {
                let fs = fs.expect("an encryption type's checksum, with no filesystem for its key");
                let mut key = [0u8; POLY1305_KEY_SIZE];
                nonce.d[3] = Le32::new(nonce.d[3].get() ^ BCH_NONCE_POLY);
                // as C's, with a key it hasn't - all zeroes - though callers
                // check valid() first
                let unset = c::bch_key::default();
                chacha20(fs.encryption_key().unwrap_or(&unset), nonce, &mut key);

                let mut ctx = c::poly1305_desc_ctx::default();
                // SAFETY: a state, to set up with a key of its size
                unsafe { c::poly1305_init(&mut ctx, key.as_ptr()) };
                Csum::Poly1305(ctx, ty.bytes().expect("an encryption type's size") as usize)
            }
            _ => Csum::None(0),
        }
    }

    /// A merge's: the checksum merged onto.
    fn seed(&mut self, v: u64) {
        match self {
            Csum::None(seed) | Csum::Crc32c { seed, .. } | Csum::Crc64 { seed, .. } => *seed = v,
            _ => unreachable!("seeding a checksum that doesn't merge"),
        }
    }

    fn update(&mut self, data: &[u8]) {
        // SAFETY: @data's
        unsafe { self.update_raw(data.as_ptr(), data.len()) }
    }

    /// # Safety
    /// @p is @len bytes.
    unsafe fn update_raw(&mut self, p: *const u8, len: usize) {
        // SAFETY: the caller's; the states set up
        unsafe {
            match self {
                Csum::None(_) => {}
                Csum::Crc32c { seed, .. } => *seed = c::crc32c(*seed as u32, p.cast(), len).into(),
                Csum::Crc64 { seed, .. } => *seed = c::crc64_be(*seed, p.cast(), len),
                Csum::Xxhash(s) => { c::xxh64_update(s, p.cast(), len); }
                Csum::Poly1305(ctx, _) => c::poly1305_update(ctx, p, len as c_uint),
            }
        }
    }

    fn finish(self) -> c::bch_csum {
        let lo = match self {
            Csum::None(seed) => seed,
            Csum::Crc32c { seed, xor } | Csum::Crc64 { seed, xor } => seed ^ xor,
            // SAFETY: set up
            Csum::Xxhash(s) => unsafe { c::xxh64_digest(&s) },
            Csum::Poly1305(mut ctx, bytes) => {
                let mut digest = [0u8; POLY1305_DIGEST_SIZE];
                // SAFETY: set up, and room for its digest
                unsafe { c::poly1305_final(&mut ctx, digest.as_mut_ptr()) };
                let mut ret = c::bch_csum::default();
                crate::util::vstructs::plain_bytes_mut(&mut ret)[..bytes].copy_from_slice(&digest[..bytes]);
                return ret;
            }
        };
        c::bch_csum { lo: Le64::new(lo), hi: Le64::new(0) }
    }
}

/// The checksum of vstruct @v whose first field is its checksum: of all of it
/// after that field - C's csum_vstruct(). @fs for an encryption type's key.
pub fn csum_vstruct<T: Unsized + ?Sized>(fs: Option<&Fs>, ty: c::bch_csum_type, nonce: c::nonce,
                                         v: &T) -> c::bch_csum {
    checksum(fs, ty, nonce, &bytes_of(v)[size_of::<c::bch_csum>()..])
}

/// @data's checksum: Fs::checksum()'s, and checksum_unkeyed()'s.
fn checksum(fs: Option<&Fs>, ty: c::bch_csum_type, nonce: c::nonce, data: &[u8]) -> c::bch_csum {
    let mut csum = Csum::new(fs, ty, nonce);
    csum.update(data);
    csum.finish()
}

impl c::bch_csum_type {
    /// Whether it's an encryption type: bch2_csum_type_is_encryption().
    pub fn is_encryption(self) -> bool {
        matches!(self, c::bch_csum_type::BCH_CSUM_chacha20_poly1305_80 |
                       c::bch_csum_type::BCH_CSUM_chacha20_poly1305_128)
    }

    /// Its checksum's size in bytes - None for a type this version doesn't
    /// know: bch_crc_bytes[].
    pub fn bytes(self) -> Option<u32> {
        Some(match self {
            c::bch_csum_type::BCH_CSUM_none                     => 0,
            c::bch_csum_type::BCH_CSUM_crc32c_nonzero |
            c::bch_csum_type::BCH_CSUM_crc32c                   => 4,
            c::bch_csum_type::BCH_CSUM_crc64_nonzero |
            c::bch_csum_type::BCH_CSUM_crc64 |
            c::bch_csum_type::BCH_CSUM_xxhash                   => 8,
            c::bch_csum_type::BCH_CSUM_chacha20_poly1305_80     => 10,
            c::bch_csum_type::BCH_CSUM_chacha20_poly1305_128    => 16,
            _ => return None,
        })
    }

    /// Whether checksums of two adjacent ranges combine into the whole's -
    /// bch2_checksum_merge() - so an extent with it can merge:
    /// bch2_checksum_mergeable().
    pub fn mergeable(self) -> bool {
        matches!(self, c::bch_csum_type::BCH_CSUM_none |
                       c::bch_csum_type::BCH_CSUM_crc32c |
                       c::bch_csum_type::BCH_CSUM_crc64)
    }

    /// Whether @fs can use it - a type this version knows, and an encryption
    /// type only with the key loaded: bch2_checksum_type_valid().
    pub fn valid(self, fs: &Fs) -> bool {
        self.0 < c::bch_csum_type::BCH_CSUM_NR.0 && (!self.is_encryption() || fs.encryption_key().is_some())
    }

    /// @data's checksum, of a type that takes no key - nor so a nonce, or a
    /// filesystem. Not an encryption type's.
    pub fn checksum_unkeyed(self, data: &[u8]) -> c::bch_csum {
        assert!(!self.is_encryption(), "an unkeyed checksum, of encryption type {}", self.0);
        checksum(None, self, c::nonce::default(), data)
    }

    /// The checksum of a range whose two halves checksum to @a and @b, @b
    /// @b_len bytes: a crc of b_len zeroes on top of @a, XOR @b. Panics on a
    /// type that isn't mergeable().
    pub fn merge(self, a: c::bch_csum, b: c::bch_csum, b_len: usize) -> c::bch_csum {
        assert!(self.mergeable(), "merging checksums of type {self}, which don't merge");
        static ZEROES: [u8; 4096] = [0; 4096];

        let mut csum = Csum::new(None, self, c::nonce::default());
        csum.seed(a.lo.get());
        let mut left = b_len;
        while left != 0 {
            let n = left.min(ZEROES.len());
            csum.update(&ZEROES[..n]);
            left -= n;
        }
        let lo = csum.finish().lo.get();
        c::bch_csum { lo: Le64::new(lo ^ b.lo.get()), hi: Le64::new(a.hi.get() ^ b.hi.get()) }
    }
}

impl c::bch_csum {
    /// As many of its bytes as a @ty checksum has, in hex - all 16, for a
    /// type this version doesn't know: bch2_csum_to_text().
    pub fn to_text(&self, out: &mut Printbuf, ty: c::bch_csum_type) {
        let bytes = crate::util::vstructs::plain_bytes(self);
        for b in &bytes[..ty.bytes().unwrap_or(16) as usize] {
            write!(out, "{b:02x}");
        }
    }
}

pub fn csum_err_msg(out: &mut Printbuf, ty: c::bch_csum_type, expected: c::bch_csum, got: c::bch_csum) {
    write!(out, "checksum error, type {ty}: got ");
    got.to_text(out, ty);
    write!(out, " should be ");
    expected.to_text(out, ty);
}

impl c::bch_csum_opt {
    /// The checksum type it writes, for data or metadata - metadata's crcs
    /// are the nonzero variants: bch2_csum_opt_to_type(). An unknown opt is
    /// an incompat feature, so warned about, and crc32c.
    pub fn to_type(self, fs: &Fs, data: bool) -> c::bch_csum_type {
        use c::bch_csum_opt as o;
        use c::bch_csum_type as t;
        match self {
            o::BCH_CSUM_OPT_none   => t::BCH_CSUM_none,
            o::BCH_CSUM_OPT_crc32c => if data { t::BCH_CSUM_crc32c } else { t::BCH_CSUM_crc32c_nonzero },
            o::BCH_CSUM_OPT_crc64  => if data { t::BCH_CSUM_crc64 } else { t::BCH_CSUM_crc64_nonzero },
            o::BCH_CSUM_OPT_xxhash => t::BCH_CSUM_xxhash,
            _ => {
                crate::warn_once!(true, fs, "unknown checksum opt {}", self.0);
                if data { t::BCH_CSUM_crc32c } else { t::BCH_CSUM_crc32c_nonzero }
            }
        }
    }
}

impl Fs {
    /// The checksum type data written with @opts gets - none for nocow, which
    /// can't checksum what it overwrites in place, and encrypted on an
    /// encrypted filesystem.
    pub fn data_checksum_type(&self, opts: &c::bch_inode_opts) -> c::bch_csum_type {
        if opts.nocow != 0 {
            return c::bch_csum_type::BCH_CSUM_none;
        }
        if self.sb.encryption_type.get() != 0 {
            return if self.opts().wide_macs.get() != 0 {
                c::bch_csum_type::BCH_CSUM_chacha20_poly1305_128
            } else {
                c::bch_csum_type::BCH_CSUM_chacha20_poly1305_80
            };
        }
        c::bch_csum_opt(opts.data_checksum.into()).to_type(self, true)
    }

    /// The checksum type data reconciled to @r is written with - encrypted,
    /// whatever @r says, on an encrypted filesystem: bch2_data_checksum_type_rb().
    pub fn data_checksum_type_rb(&self, r: &c::bch_extent_reconcile) -> c::bch_csum_type {
        if self.sb.encryption_type.get() != 0 {
            return if self.opts().wide_macs.get() != 0 {
                c::bch_csum_type::BCH_CSUM_chacha20_poly1305_128
            } else {
                c::bch_csum_type::BCH_CSUM_chacha20_poly1305_80
            };
        }
        c::bch_csum_opt(r.data_checksum().into()).to_type(self, true)
    }

    /// @data's checksum of type @ty, with @nonce - an encryption type's under
    /// the filesystem's key. A type this version doesn't know checksums to
    /// zero.
    pub fn checksum(&self, ty: c::bch_csum_type, nonce: c::nonce, data: &[u8]) -> c::bch_csum {
        checksum(Some(self), ty, nonce, data)
    }

    /// The checksum of @r's data. As checksum().
    pub fn checksum_range(&self, ty: c::bch_csum_type, nonce: c::nonce, r: BioRange<'_>) -> c::bch_csum {
        let mut csum = Csum::new(Some(self), ty, nonce);
        for chunk in r {
            // SAFETY: the chunk's bytes, mapped
            chunk.with_mapped(|p, len| unsafe { csum.update_raw(p, len) });
        }
        csum.finish()
    }
}

// ── Encryption ───────────────────────────────────────────────────────────

/// ChaCha20 under a key from a nonce, as far as it's got: zeroed on drop.
struct Chacha(c::chacha_state);

impl Chacha {
    fn new(key: &c::bch_key, nonce: c::nonce) -> Self {
        let mut words: [u32; 8] = core::array::from_fn(|i| (key.key[i / 2].get() >> (32 * (i % 2))) as u32);
        let mut s = Chacha(c::chacha_state::default());
        // SAFETY: a state to set up, from 8 words of key and 16 bytes of iv
        unsafe { c::chacha_init(&mut s.0, words.as_ptr(), (&raw const nonce).cast()) };
        c::wipe(&mut words);
        s
    }

    /// Each call but the last whole ChaCha20 blocks.
    ///
    /// # Safety
    /// @p is @len bytes, ours to write.
    unsafe fn crypt(&mut self, p: *mut u8, len: usize) {
        // SAFETY: the caller's
        unsafe { c::chacha20_crypt(&mut self.0, p, p, len as c_uint) }
    }
}

impl Drop for Chacha {
    fn drop(&mut self) {
        // SAFETY: ours
        unsafe { c::chacha_zeroize_state(&mut self.0) }
    }
}

/// @data, ChaCha20'd under @key from @nonce: encrypted, or decrypted.
pub fn chacha20(key: &c::bch_key, nonce: c::nonce, data: &mut [u8]) {
    // SAFETY: @data's
    unsafe { Chacha::new(key, nonce).crypt(data.as_mut_ptr(), data.len()) }
}

impl c::bch_encrypted_key {
    /// Encrypted under @key, from the superblock's @nonce - or decrypted: the
    /// right key leaves its magic as it should be.
    pub fn chacha20(&mut self, key: &c::bch_key, nonce: c::nonce) {
        chacha20(key, nonce, crate::util::vstructs::plain_bytes_mut(self))
    }
}

impl Fs {
    /// The key to encrypt with: an encryption type without one is an
    /// inconsistency.
    fn chacha20_key(&self) -> Result<&c::bch_key, BchError> {
        let Some(key) = self.encryption_key() else {
            crate::fs_inconsistent!(self, "attempting to encrypt without encryption key");
            return self.throw(bch_errcode::BCH_ERR_no_encryption_key);
        };
        Ok(key)
    }

    /// @data encrypted with @ty under the filesystem's key - or decrypted,
    /// ChaCha20 being its own inverse. Nothing, for a type that isn't
    /// encryption.
    pub fn encrypt(&self, ty: c::bch_csum_type, nonce: c::nonce, data: &mut [u8]) -> Result<(), BchError> {
        if ty.is_encryption() {
            chacha20(self.chacha20_key()?, nonce, data);
        }
        Ok(())
    }

    /// @r's data, encrypted in place: as encrypt(), for an encryption type.
    /// Each chunk but the last must be whole ChaCha20 blocks.
    pub fn encrypt_range(&self, nonce: c::nonce, r: BioRange<'_>) -> Result<(), BchError> {
        let mut chacha = Chacha::new(self.chacha20_key()?, nonce);
        for chunk in r {
            if chunk.len() % CHACHA_BLOCK_SIZE != 0 {
                crate::bch_err_ratelimited!(self, "bio not aligned for encryption");
                return Err(BchError::from(c::EIO));
            }
            // SAFETY: the chunk's bytes, mapped
            chunk.with_mapped(|p, len| unsafe { chacha.crypt(p, len) });
        }
        Ok(())
    }
}

// ── For C ────────────────────────────────────────────────────────────────

/// @c may be NULL, for a type that isn't encryption.
///
/// # Safety
/// @data is @len bytes.
#[c_export]
pub unsafe fn bch2_checksum(c: Option<&Opaque<c::bch_fs>>, type_: c_uint, nonce: c::nonce,
                            data: *const c_void, len: usize) -> c::bch_csum {
    let mut csum = Csum::new(c.map(Fs::from_c), c::bch_csum_type(type_), nonce);
    // SAFETY: the caller's
    unsafe { csum.update_raw(data.cast(), len) };
    csum.finish()
}

/// # Safety
/// @data is @len bytes.
#[c_export]
pub unsafe fn bch2_encrypt(c: &Opaque<c::bch_fs>, type_: c_uint, nonce: c::nonce,
                           data: *mut c_void, len: usize) -> c_int {
    if !c::bch_csum_type(type_).is_encryption() {
        return 0;
    }
    // SAFETY: the caller's
    ret_to_c(Fs::from_c(c).chacha20_key().map(|key| unsafe { Chacha::new(key, nonce).crypt(data.cast(), len) }))
}

/// # Safety
/// @data is @len bytes.
#[c_export]
pub unsafe fn bch2_chacha20(key: &c::bch_key, nonce: c::nonce, data: *mut c_void, len: usize) {
    // SAFETY: the caller's
    unsafe { Chacha::new(key, nonce).crypt(data.cast(), len) }
}

/// # Safety
/// @bio is a bio, not in flight.
#[c_export]
pub unsafe fn bch2_checksum_bio(c: &Opaque<c::bch_fs>, type_: c_uint, nonce: c::nonce,
                                bio: *mut c::bio) -> c::bch_csum {
    // SAFETY: the caller's
    Fs::from_c(c).checksum_range(c::bch_csum_type(type_), nonce, unsafe { Bio::from_raw(bio) }.range())
}

/// bch2_encrypt_bio()'s, for an encryption type.
///
/// # Safety
/// @bio is a bio, not in flight.
#[c_export]
pub unsafe fn __bch2_encrypt_bio(c: &Opaque<c::bch_fs>, _type: c_uint, nonce: c::nonce, bio: *mut c::bio) -> c_int {
    // SAFETY: the caller's
    ret_to_c(Fs::from_c(c).encrypt_range(nonce, unsafe { Bio::from_raw(bio) }.range()))
}

/// @bio's data checksummed afresh, as @new_csum_type, in three: @len_a
/// sectors into @crc_a, @len_b into @crc_b, and the rest - having checked it
/// against @crc_old, what it was read with: the new checksums merged, if they
/// merge, else it checksummed again whole. Can't encrypt or decrypt, so both
/// types are encryption, or neither.
///
/// # Safety
/// @bio is a bio, not in flight.
#[c_export]
pub unsafe fn bch2_rechecksum_bio(c: &Opaque<c::bch_fs>, bio: *mut c::bio, version: c::bversion,
                                  crc_old: c::bch_extent_crc_unpacked,
                                  crc_a: Option<&mut c::bch_extent_crc_unpacked>,
                                  crc_b: Option<&mut c::bch_extent_crc_unpacked>,
                                  len_a: c_uint, len_b: c_uint, new_csum_type: c_uint) -> c_int {
    use crate::data::ptrs::{crc_is_compressed, extent_nonce};

    let fs = Fs::from_c(c);
    // SAFETY: the caller's
    let bio = unsafe { Bio::from_raw(bio) };
    let (old_ty, new_ty) = (c::bch_csum_type(crc_old.csum_type.into()), c::bch_csum_type(new_csum_type));
    let sectors = bio.raw().bi_iter.bi_size >> 9;

    assert!(len_a + len_b <= sectors, "rechecksumming {len_a} + {len_b} sectors of a bio of {sectors}");
    assert_eq!(crc_old.uncompressed_size, sectors, "rechecksumming a bio that isn't its crc's extent");
    assert!(!crc_is_compressed(&crc_old), "rechecksumming compressed data");
    assert_eq!(old_ty.is_encryption(), new_ty.is_encryption(),
               "rechecksumming {old_ty} as {new_ty}: that would encrypt or decrypt");

    let mergeable = old_ty == new_ty && new_ty.mergeable();
    let mut splits = [(crc_a, len_a), (crc_b, len_b), (None, sectors - len_a - len_b)]
        .map(|(crc, len)| (crc, len, c::bch_csum::default()));

    let (mut r, mut nonce) = (bio.range(), extent_nonce(version, &crc_old));
    for (crc, len, csum) in &mut splits {
        if mergeable || crc.is_some() {
            *csum = fs.checksum_range(new_ty, nonce, r.take(*len << 9));
        }
        r.advance(*len << 9);
        nonce = nonce + (*len << 9);
    }

    let merged = if mergeable {
        splits.iter().fold(c::bch_csum::default(), |m, (_, len, csum)| new_ty.merge(m, *csum, (*len << 9) as usize))
    } else {
        fs.checksum_range(old_ty, extent_nonce(version, &crc_old), bio.range())
    };

    // in constant time, as C's bch2_crc_cmp() means to be
    let differ = ((merged.lo.get() ^ crc_old.csum.lo.get()) | (merged.hi.get() ^ crc_old.csum.hi.get())) != 0;
    if differ && fs.opts().no_data_io.get() == 0 {
        static RATELIMIT: crate::util::log::Ratelimit = crate::util::log::Ratelimit::new();
        if !RATELIMIT.suppress(fs, crate::c_function_name!()) {
            crate::bch_warn!(fs, "checksum error in bch2_rechecksum_bio() (memory corruption or bug?)\n  \
                                  expected {:x}:{:x} got {:x}:{:x} (old type {old_ty} new type {new_ty})",
                             crc_old.csum.hi.get(), crc_old.csum.lo.get(), merged.hi.get(), merged.lo.get());
            crate::warn_on!(true);
        }
        return ret_to_c(fs.throw::<()>(bch_errcode::BCH_ERR_recompute_checksum));
    }

    let mut crc_nonce = u32::from(crc_old.nonce);
    for (crc, len, csum) in splits {
        if let Some(crc) = crc {
            *crc = c::bch_extent_crc_unpacked {
                csum_type:          new_csum_type as u8,
                compression_type:   crc_old.compression_type,
                compressed_size:    len,
                uncompressed_size:  len,
                offset:             0,
                live_size:          len,
                nonce:              crc_nonce as u16,
                csum,
            };
        }
        if new_ty.is_encryption() {
            crc_nonce += len;
        }
    }
    0
}

// ── The superblock's crypt section ───────────────────────────────────────

/// A section shorter than the struct is refused before validate(), as too
/// short to be one.
impl SbFieldOps for c::bch_sb_field_crypt {
    fn validate(_sb: &c::bch_sb<Unvalidated>, crypt: &c::bch_sb_field_crypt<Unvalidated>,
                _flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
        let kdf = crypt.bch_crypt_kdf_type();
        sb_err_on!(err, kdf != u64::from(c::bch_kdf_types::BCH_KDF_SCRYPT.0), BCH_ERR_invalid_sb_crypt,
                   "bad kdf type {kdf}")?;
        Ok(())
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb<Unvalidated>,
                crypt: &c::bch_sb_field_crypt<Unvalidated>) {
        write!(out, "KFD:               {}\n", crypt.bch_crypt_kdf_type());
        write!(out, "scrypt n:          {}\n", crypt.bch_kdf_scrypt_n());
        write!(out, "scrypt r:          {}\n", crypt.bch_kdf_scrypt_r());
        write!(out, "scrypt p:          {}\n", crypt.bch_kdf_scrypt_p());
    }
}

// ── The key ──────────────────────────────────────────────────────────────

/// What the keyring has @sb's key under.
fn key_description(sb: &c::bch_sb) -> Printbuf {
    let mut out = Printbuf::new();
    write!(out, "bcachefs:{}", sb.uuid());
    out
}

/// The user key @desc, from the kernel's keyring.
#[cfg(kernel)]
fn keyring_key(desc: &core::ffi::CStr) -> Result<c::bch_key, BchError> {
    // SAFETY: a string
    let key = unsafe { c::rust_request_user_key(desc.as_ptr().cast()) };
    let key = core::ptr::NonNull::new(key).expect("request_key(): a key, or an ERR_PTR()");
    let key = crate::errcode::errptr_to_result(key)?.as_ptr();

    // SAFETY: a key, our ref on it put after; its payload read under its sem
    unsafe {
        c::down_read(&raw mut (*key).sem);
        let mut len = 0;
        let data = c::rust_user_key_data(key, &mut len);
        let ret = if usize::from(len) == size_of::<c::bch_key>() {
            Ok(core::ptr::read_unaligned(data.cast::<c::bch_key>()))
        } else {
            Err(BchError::from(c::EINVAL))
        };
        c::up_read(&raw mut (*key).sem);
        c::key_put(key);
        ret
    }
}

/// The user key @desc, from the session keyring, the user's, or the user
/// session's.
#[cfg(not(kernel))]
fn keyring_key(desc: &core::ffi::CStr) -> Result<c::bch_key, BchError> {
    for ring in [c::KEY_SPEC_SESSION_KEYRING, c::KEY_SPEC_USER_KEYRING, c::KEY_SPEC_USER_SESSION_KEYRING] {
        // SAFETY: strings
        let id = unsafe { c::request_key(c"user".as_ptr(), desc.as_ptr(), core::ptr::null(), ring) };
        if id >= 0 {
            let mut key = c::bch_key::default();
            // SAFETY: room for a key
            let n = unsafe { c::keyctl_read(id, (&raw mut key).cast(), size_of_val(&key)) };
            return if n == size_of_val(&key) as i64 { Ok(key) } else { Err(BchError::from(c::EINVAL)) };
        }
    }
    Err(BchError::from_raw(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)))
}

/// The key derived from a passphrase, asked for on the terminal, if it's the
/// one @sb's key is encrypted with.
///
/// Whether it was the right passphrase used to be thrown away here: a wrong
/// one handed back a garbage key and success, and the caller found out as a
/// checksum error somewhere further on.
#[cfg(not(kernel))]
fn passphrase_key(sb: &c::bch_sb) -> Result<c::bch_key, BchError> {
    use crate::util::vstructs::Head;
    use core::ffi::{c_char, CStr};

    // c_src/crypto.h's
    extern "C" {
        fn read_passphrase(prompt: *const c_char) -> *mut c_char;
        fn bch2_passphrase_check(sb: *const Head<c::bch_sb>, passphrase: *const c_char,
                                 passphrase_key: *mut c::bch_key, sb_key: *mut c::bch_encrypted_key) -> bool;
    }

    let (mut key, mut sb_key) = (c::bch_key::default(), c::bch_encrypted_key::default());
    // SAFETY: the prompt a string; the passphrase a malloc()ed one, ours
    unsafe {
        let passphrase = read_passphrase(c"Enter passphrase: ".as_ptr());
        let ok = bch2_passphrase_check(sb.as_ffi(), passphrase, &mut key, &mut sb_key);
        c::wipe(core::slice::from_raw_parts_mut(passphrase.cast::<u8>(), CStr::from_ptr(passphrase).count_bytes()));
        c::rust_kfree(passphrase.cast());
        if ok { Ok(key) } else { Err(BchError::from(c::ENOKEY)) }
    }
}

/// The user's key, for @sb: from the keyring - or in userspace, failing
/// that, a passphrase.
pub fn request_key(sb: &c::bch_sb) -> Result<c::bch_key, BchError> {
    let ret = keyring_key(key_description(sb).as_c_str());
    #[cfg(not(kernel))]
    let ret = ret.or_else(|_| passphrase_key(sb));
    ret
}

/// @sb's key taken out of the user keyring: its passphrase is changing.
#[cfg(not(kernel))]
pub fn revoke_key(sb: &c::bch_sb) -> Result<(), BchError> {
    let desc = key_description(sb);
    // SAFETY: strings
    let id = unsafe {
        c::request_key(c"user".as_ptr(), desc.as_c_str().as_ptr(), core::ptr::null(), c::KEY_SPEC_USER_KEYRING)
    };
    if id < 0 {
        return Err(BchError::from_raw(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)));
    }
    // SAFETY: a key
    unsafe { c::keyctl_revoke(id) };
    Ok(())
}

/// The filesystem's key: the superblock's, decrypted with the user's if a
/// passphrase was ever set on it - @user_key, or the keyring's.
fn decrypt_sb_key(fs: &Fs, sb: &c::bch_sb, crypt: &c::bch_sb_field_crypt, user_key: Option<&c::bch_key>)
    -> Result<c::bch_key, BchError>
{
    let mut sb_key = crypt.key.clone();
    if sb_key.is_encrypted() {
        /*
         * Handed to us by whoever opened the filesystem, if they had it: the
         * mount helper derives this key from the passphrase and checks it
         * against the superblock before we are called at all, so there is
         * nothing for a keyring to do except carry it between two processes
         * that are, on the fsconfig(2) path, the same process.
         *
         * The keyring is still how a key gets here from somewhere else -
         * `bcachefs unlock` ahead of time, mount(2), an agent that unlocked
         * the filesystem for us - so it stays as the fallback.
         */
        let user_key = match user_key {
            Some(k) => k.clone(),
            None => request_key(sb).inspect_err(|e| crate::bch_err!(fs, "error requesting encryption key: {e}"))?,
        };

        sb_key.chacha20(&user_key, sb.nonce());
        if sb_key.is_encrypted() {
            crate::bch_err!(fs, "incorrect encryption key");
            return Err(BchError::from(c::EINVAL));
        }
    }
    Ok(sb_key.into_key())
}

/// The filesystem's key set, if it's encrypted: at mount, @user_key the key
/// whoever opened it was handed - its owner's still, to wipe.
#[c_export]
pub fn bch2_fs_encryption_init(c: &Opaque<c::bch_fs>, user_key: Option<&c::bch_key>) -> c_int {
    let fs = Fs::from_c(c);
    let mut sb = fs.sb_lock();
    let Some(crypt) = sb.field::<c::bch_sb_field_crypt>() else { return 0 };

    match decrypt_sb_key(fs, sb.sb(), crypt, user_key) {
        Ok(key) => {
            assert!(sb.published().encryption_key.set(key), "the filesystem's key set before mount");
            0
        }
        Err(e) => ret_to_c(Err(e)),
    }
}

/// Whether @c has its key - an encryption type is no use without it.
#[c_export]
pub fn bch2_fs_has_encryption_key(c: &Opaque<c::bch_fs>) -> bool {
    Fs::from_c(c).encryption_key().is_some()
}

// ── Turning encryption on and off ────────────────────────────────────────
//
// Nothing calls these yet - C had them under #if 0, and neither had been
// built since the crypto API they used went. disable_encryption() might
// replace the tools' cmd_remove_passphrase(), and with an ioctl take the
// passphrase off a mounted filesystem; enable_encryption() should be hooked
// up.

/// The key stored unencrypted, and encryption off for what's written from now.
pub fn disable_encryption(fs: &Fs) -> Result<(), BchError> {
    let mut sb = fs.sb_lock();
    let Some(crypt) = sb.field::<c::bch_sb_field_crypt>() else {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_disable_encryption_no_crypt);
    };
    // XXX: as C had it - but a key that isn't encrypted is the one that has
    // nothing to do, so this looks inverted
    if crypt.key.is_encrypted() {
        return Ok(());
    }
    let key = decrypt_sb_key(fs, sb.sb(), crypt, None)?;

    sb.field_mut::<c::bch_sb_field_crypt>().expect("its crypt section, just read").key =
        c::bch_encrypted_key::new_unencrypted(key);
    sb.sb_mut().set_encryption_type(0);
    fs.write_super(&mut sb);
    Ok(())
}

/// Encryption on, with a new key - encrypted under the user's, from the
/// keyring, if @keyed. Not on a filesystem that has a key already.
pub fn enable_encryption(fs: &Fs, keyed: bool) -> Result<(), BchError> {
    let mut sb = fs.sb_lock();
    if sb.field::<c::bch_sb_field_crypt>().is_some() {
        return Err(BchError::from(c::EINVAL));
    }

    let mut key = c::bch_key::default();
    // SAFETY: the key's bytes
    unsafe { c::get_random_bytes((&raw mut key).cast(), size_of_val(&key) as _) };

    let mut sb_key = c::bch_encrypted_key::new_unencrypted(key.clone());
    if keyed {
        let user_key = request_key(sb.sb())
            .inspect_err(|e| crate::bch_err!(fs, "error requesting encryption key: {e}"))?;
        sb_key.chacha20(&user_key, sb.sb().nonce());
    }

    let u64s = (size_of::<c::bch_sb_field_crypt>() / size_of::<u64>()) as u32;
    match fs.sb_resize(&mut sb).field_get_minsize::<c::bch_sb_field_crypt>(u64s) {
        Some(crypt) => crypt.key = sb_key,
        None => return fs.throw(bch_errcode::BCH_ERR_ENOSPC_sb_crypt),
    }
    sb.sb_mut().set_encryption_type(1);

    // C's setkey was given the key after it was encrypted under the user's,
    // when @keyed: this is the key itself
    assert!(sb.published().encryption_key.set(key), "a key, on a filesystem with no crypt section");
    fs.write_super(&mut sb);
    Ok(())
}
