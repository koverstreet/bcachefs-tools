// SPDX-License-Identifier: GPL-2.0

//! ChaCha20, Poly1305 and SHA-256: the kernel's lib/crypto, called as
//! RustCrypto's chacha20, poly1305 and sha2 crates are - userspace's.

use crate::c;
use crate::data::checksum::CHACHA_BLOCK_SIZE;

/// Each call but the last whole blocks: the kernel's goes on from the next
/// block after a partial one.
pub struct ChaCha20 {
    state:   c::chacha_state,
    partial: bool,
}

impl ChaCha20 {
    pub fn new(key: &[u8; 32], nonce: &[u8; 12]) -> Self {
        let mut words: [u32; 8] = core::array::from_fn(|i| u32::from_le_bytes(*key[i * 4..].first_chunk().unwrap()));
        let mut iv = [0u8; 16];
        iv[4..].copy_from_slice(nonce);

        let mut state = c::chacha_state::default();
        unsafe { c::chacha_init(&mut state, words.as_ptr(), iv.as_ptr()) };
        crate::util::wipe(&mut words);
        ChaCha20 { state, partial: false }
    }

    /// To a block.
    pub fn seek(&mut self, pos: u64) {
        let block = u64::from(CHACHA_BLOCK_SIZE);
        assert!(pos % block == 0, "ChaCha20 seek to {pos}: not a block");
        self.state.x[12] = (pos / block).try_into()
            .unwrap_or_else(|_| panic!("ChaCha20 seek to {pos}: past its 32 bit block counter"));
        self.partial = false;
    }

    pub fn apply_keystream(&mut self, buf: &mut [u8]) {
        debug_assert!(!self.partial, "ChaCha20 continued after a partial block");
        self.partial = buf.len() % CHACHA_BLOCK_SIZE as usize != 0;

        let p = buf.as_mut_ptr();
        // a size_t from 7.4, an unsigned int before
        unsafe { c::chacha20_crypt(&mut self.state, p, p, buf.len() as _) }
    }
}

impl Drop for ChaCha20 {
    fn drop(&mut self) {
        crate::util::wipe(&mut self.state)
    }
}

/// hybrid_array's Array<u8, U16>, as poly1305's Block is.
#[repr(transparent)]
pub struct Block([u8; 16]);

impl Block {
    pub fn slice_as_chunks(buf: &[u8]) -> (&[Block], &[u8]) {
        let (blocks, rest) = buf.split_at(buf.len() / 16 * 16);
        (unsafe { core::slice::from_raw_parts(blocks.as_ptr().cast(), blocks.len() / 16) }, rest)
    }
}

impl From<[u8; 16]> for Block {
    fn from(b: [u8; 16]) -> Self {
        Block(b)
    }
}

pub struct Poly1305(c::poly1305_desc_ctx);

impl Poly1305 {
    pub fn new(key: &[u8; 32]) -> Self {
        let mut ctx = c::poly1305_desc_ctx::default();
        unsafe { c::poly1305_init(&mut ctx, key.as_ptr()) };
        Poly1305(ctx)
    }

    pub fn update(&mut self, blocks: &[Block]) {
        unsafe { c::poly1305_update(&mut self.0, blocks.as_ptr().cast(), size_of_val(blocks) as _) }
    }

    pub fn compute_unpadded(mut self, data: &[u8]) -> [u8; 16] {
        unsafe { c::poly1305_update(&mut self.0, data.as_ptr(), data.len() as _) };
        self.finalize()
    }

    pub fn finalize(mut self) -> [u8; 16] {
        let mut tag = [0u8; 16];
        unsafe { c::poly1305_final(&mut self.0, tag.as_mut_ptr()) };
        tag
    }
}

impl Drop for Poly1305 {
    fn drop(&mut self) {
        crate::util::wipe(&mut self.0)
    }
}

pub struct Sha256;

impl Sha256 {
    pub fn digest(data: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        unsafe { c::sha256(data.as_ptr(), data.len() as _, out.as_mut_ptr()) };
        out
    }
}
