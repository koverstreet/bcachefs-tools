// SPDX-License-Identifier: GPL-2.0

//! Nonces: ChaCha20's, for encrypted metadata and data. They're on-disk
//! format - decrypting takes the nonce the data was encrypted with.
//!
//! A nonce is four little-endian words. The top bits of the last are its
//! domain, what's encrypted with it, so that btree nodes, journal entries and
//! extents never share one; the first counts ChaCha20 blocks, which add()
//! advances. A domain's nonce is made where its structure is: bset::nonce()
//! in btree/read.rs, jset::nonce() in journal/read.rs.
//!
//! C has its own in data/checksum.h: BCH_NONCE_*, nonce_add().
//!
//! And the superblock's crypt section: the filesystem's key, encrypted under
//! one derived from the passphrase, and how it's derived.

use crate::c;
use crate::util::vstructs::Unvalidated;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::sb::io::SbFieldOps;
use crate::types::Le32;
use crate::util::Printbuf;
use core::fmt;

/// ChaCha20's block, in bytes: what a nonce's first word counts.
pub const CHACHA_BLOCK_SIZE: u32 = 64;

/// The domains, in a nonce's last word.
pub const BCH_NONCE_EXTENT:	u32 = 1 << 28;
pub const BCH_NONCE_BTREE:	u32 = 2 << 28;
pub const BCH_NONCE_JOURNAL:	u32 = 3 << 28;
pub const BCH_NONCE_PRIO:	u32 = 4 << 28;
pub const BCH_NONCE_POLY:	u32 = 1 << 31;

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
        self.0 < c::bch_csum_type::BCH_CSUM_NR.0 && (!self.is_encryption() || fs.chacha20_key_set)
    }
}

/// Its name, from BCH_CSUM_TYPES(): bch2_prt_csum_type().
impl fmt::Display for c::bch_csum_type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        macro_rules! csum_type_names {
            ($(($name:tt, $n:expr)),* $(,)?) => {
                const NAMES: &[(&str, u32)] = &[$((stringify!($name), $n),)*];
            };
        }
        c::BCH_CSUM_TYPES!(csum_type_names);

        match NAMES.iter().find(|&&(_, n)| n == self.0) {
            Some((name, _)) => f.write_str(name),
            None => write!(f, "(unknown csum_type {})", self.0),
        }
    }
}

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

// ── The superblock's crypt section ───────────────────────────────────────

/// A section shorter than the struct is refused before validate(), as too
/// short to be one.
impl SbFieldOps for c::bch_sb_field_crypt {
    fn validate(_sb: &c::bch_sb<Unvalidated>, crypt: &c::bch_sb_field_crypt<Unvalidated>,
                _flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
        let kdf = crypt.bch_crypt_kdf_type();
        if kdf != u64::from(c::bch_kdf_types::BCH_KDF_SCRYPT.0) {
            write!(err, "bad kdf type {kdf}");
            return Err(bch_errcode::BCH_ERR_invalid_sb_crypt.into());
        }
        Ok(())
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, crypt: &Self) {
        write!(out, "KFD:               {}\n", crypt.bch_crypt_kdf_type());
        write!(out, "scrypt n:          {}\n", crypt.bch_kdf_scrypt_n());
        write!(out, "scrypt r:          {}\n", crypt.bch_kdf_scrypt_r());
        write!(out, "scrypt p:          {}\n", crypt.bch_kdf_scrypt_p());
    }
}

