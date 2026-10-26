// SPDX-License-Identifier: GPL-2.0

//! RAID parity over a stripe's blocks - data, then P, then Q, each a slice of
//! one length: P is raid5's, an xor, and Q raid6's. generate() makes the
//! parity of the data; recover() rebuilds the blocks it's told failed from the
//! rest.
//!
//! In-kernel it's lib/raid's raid6 and xor - which 7.1 and 7.2 renamed - with
//! generate() and recover() built here as raid/raid.h's raid_gen() and raid_rec(),
//! which userspace has. Each C call has a wrapper that takes slices and checks
//! what the C asks of them: every block one length, a multiple of 512 bytes
//! (in userspace, 64); in-kernel, 64 byte aligned, and for raid6's recovery a
//! page at most, page aligned; and the failed blocks in order, in range, and
//! no more than the parity. That they're slices, &mut - or & where only read -
//! is what the C assumes and doesn't say: no block aliases another.
//!
//! A stripe of one data block is copies of it - P its xor, Q its first power -
//! which lib/raid's raid6, wanting four blocks, isn't asked for.
//!
//! Userspace's raid_rec() stands a zero block - raid_zero()'s, as long as the
//! blocks - in for what it's rebuilding: it's given a page, once, and
//! recover() goes a page at a time.

use crate::c;
use crate::util::kernel::PAGE_SIZE;
use arrayvec::ArrayVec;
use core::ffi::{c_int, c_void};

/// The most blocks a stripe has.
const MAX_BLOCKS: usize = c::BCH_BKEY_PTRS_MAX as usize;

/// The most parity blocks: bcachefs's, though raid/raid.h has six.
const MAX_PARITY: usize = 2;

/// What a block's length is a multiple of.
#[cfg(kernel)]
const MULTIPLE: usize = 512;
#[cfg(not(kernel))]
const MULTIPLE: usize = 64;

type Ptrs = ArrayVec<*mut c_void, MAX_BLOCKS>;

/// @b is a block of @bytes: see the top of the file.
fn check(b: &[u8], bytes: usize) {
    assert!(bytes != 0 && bytes % MULTIPLE == 0, "raid: blocks of {bytes} bytes");
    assert_eq!(b.len(), bytes, "raid: blocks of different lengths");
    #[cfg(kernel)]
    assert!(b.as_ptr().addr() % 64 == 0, "raid: a block at {:p}, not 64 byte aligned", b.as_ptr());
}

/// @blocks, checked, for C - and their length.
fn ptrs(blocks: &mut [&mut [u8]]) -> (Ptrs, usize) {
    let bytes = blocks.first().map_or(0, |b| b.len());
    let p = blocks.iter_mut()
        .map(|b| {
            check(b, bytes);
            b.as_mut_ptr().cast()
        })
        .collect();
    (p, bytes)
}

/// How many of @blocks, @np of them parity, are data.
fn nr_data(blocks: &[&mut [u8]], np: usize) -> usize {
    assert!((1..=MAX_PARITY).contains(&np) && blocks.len() > np && blocks.len() <= MAX_BLOCKS,
            "raid: {} blocks, {np} of them parity", blocks.len());
    blocks.len() - np
}

/// Block @from of @blocks, into block @to.
fn copy_block(blocks: &mut [&mut [u8]], from: usize, to: usize) {
    let (lo, hi) = blocks.split_at_mut(from.max(to));
    let (src, dst) = if from < to { (&*lo[from], &mut *hi[0]) } else { (&*hi[0], &mut *lo[to]) };
    dst.copy_from_slice(src);
}

/// The parity of @blocks - its last @np, one or two - from the rest.
pub fn generate(blocks: &mut [&mut [u8]], np: usize) {
    let nd = nr_data(blocks, np);

    if nd == 1 {
        (1..blocks.len()).for_each(|p| copy_block(blocks, 0, p));
    } else {
        imp::generate(blocks, nd, np)
    }
}

/// The blocks of @blocks that @failed names - in order, parity included -
/// rebuilt from the rest, @np of them parity.
pub fn recover(blocks: &mut [&mut [u8]], np: usize, failed: &[u8]) {
    let nd = nr_data(blocks, np);
    assert!(failed.len() <= np &&
            failed.is_sorted_by(|a, b| a < b) &&
            failed.last().is_none_or(|&f| usize::from(f) < blocks.len()),
            "raid: rebuilding blocks {failed:?} of {}, {np} of them parity", blocks.len());

    if failed.is_empty() {
        return;
    }

    if nd == 1 {
        let good = (0..blocks.len()).find(|&i| !failed.contains(&(i as u8))).expect("checked: a block left");
        failed.iter().for_each(|&f| copy_block(blocks, good, f.into()));
    } else {
        imp::recover(blocks, nd, np, failed)
    }
}

// ── In-kernel: lib/raid ──────────────────────────────────────────────────

#[cfg(kernel)]
mod imp {
    use super::*;

    /// @dest ^= each of @srcs: xor_gen(), which 7.1 made of xor_blocks(), at
    /// most MAX_XOR_BLOCKS at a time.
    fn xor_gen(dest: &mut [u8], srcs: &[&[u8]]) {
        if srcs.is_empty() {
            return;
        }

        let bytes = dest.len();
        check(dest, bytes);
        srcs.iter().for_each(|s| check(s, bytes));
        // only read
        let mut p: Ptrs = srcs.iter().map(|s| s.as_ptr().cast_mut().cast()).collect();

        // SAFETY: checked - each @bytes, aligned - and @dest not one of @srcs
        #[cfg(linux_ge = "7.1")]
        unsafe { c::xor_gen(dest.as_mut_ptr().cast(), p.as_mut_ptr(), p.len() as u32, bytes as u32) }
        #[cfg(not(linux_ge = "7.1"))]
        for p in p.chunks_mut(c::MAX_XOR_BLOCKS as usize) {
            // SAFETY: as above
            unsafe { c::xor_blocks(p.len() as u32, bytes as u32, dest.as_mut_ptr().cast(), p.as_mut_ptr()) }
        }
    }

    /// P and Q, @blocks' last two, from the rest: raid6's gen_syndrome().
    fn gen_syndrome(blocks: &mut [&mut [u8]]) {
        assert!(blocks.len() >= 4, "raid6 over {} blocks", blocks.len());
        let (mut p, bytes) = ptrs(blocks);

        // SAFETY: checked - each block's @bytes, aligned, and its own
        #[cfg(linux_ge = "7.2")]
        unsafe { c::raid6_gen_syndrome(p.len() as c_int, bytes, p.as_mut_ptr()) }
        #[cfg(not(linux_ge = "7.2"))]
        {
            // SAFETY: set up once, at raid6's init
            let f = unsafe { (*&raw const c::raid6_call).gen_syndrome }.expect("raid6's gen_syndrome");
            // SAFETY: as above
            unsafe { f(p.len() as c_int, bytes, p.as_mut_ptr()) }
        }
    }

    /// @blocks for raid6's recovery, which is of a page at most, page aligned.
    fn recov_ptrs(blocks: &mut [&mut [u8]]) -> (Ptrs, usize) {
        assert!(blocks.len() >= 4, "raid6 over {} blocks", blocks.len());
        let (p, bytes) = ptrs(blocks);
        assert!(bytes <= PAGE_SIZE && p.iter().all(|p| p.addr() % PAGE_SIZE == 0),
                "raid6 recovery of {bytes} bytes, or not page aligned");
        (p, bytes)
    }

    /// Data blocks @faila and @failb, from the rest and P and Q.
    fn recov_2data(blocks: &mut [&mut [u8]], faila: usize, failb: usize) {
        assert!(faila < failb && failb < blocks.len() - 2, "raid6 recovery of data blocks {faila} and {failb}");
        let (mut p, bytes) = recov_ptrs(blocks);
        let (disks, faila, failb) = (p.len() as c_int, faila as c_int, failb as c_int);

        // SAFETY: checked
        #[cfg(linux_ge = "7.2")]
        unsafe { c::raid6_recov_2data(disks, bytes, faila, failb, p.as_mut_ptr()) }
        #[cfg(not(linux_ge = "7.2"))]
        {
            // SAFETY: set up once, at raid6's init
            let f = unsafe { *&raw const c::raid6_2data_recov }.expect("raid6's 2data_recov");
            // SAFETY: as above
            unsafe { f(disks, bytes, faila, failb, p.as_mut_ptr()) }
        }
    }

    /// Data block @faila and P, from the rest and Q.
    fn recov_datap(blocks: &mut [&mut [u8]], faila: usize) {
        assert!(faila < blocks.len() - 2, "raid6 recovery of data block {faila}");
        let (mut p, bytes) = recov_ptrs(blocks);
        let (disks, faila) = (p.len() as c_int, faila as c_int);

        // SAFETY: checked
        #[cfg(linux_ge = "7.2")]
        unsafe { c::raid6_recov_datap(disks, bytes, faila, p.as_mut_ptr()) }
        #[cfg(not(linux_ge = "7.2"))]
        {
            // SAFETY: set up once, at raid6's init
            let f = unsafe { *&raw const c::raid6_datap_recov }.expect("raid6's datap_recov");
            // SAFETY: as above
            unsafe { f(disks, bytes, faila, p.as_mut_ptr()) }
        }
    }

    /// Block @failed of @blocks, the xor of the others.
    fn raid5_rec(blocks: &mut [&mut [u8]], failed: usize) {
        let (before, rest) = blocks.split_at_mut(failed);
        let (dest, after) = rest.split_first_mut().expect("raid5 recovery of a block it hasn't");
        let mut srcs = before.iter().chain(after.iter()).map(|b| &**b);

        let first = srcs.next().expect("raid5 recovery from nothing");
        dest.copy_from_slice(first);
        xor_gen(dest, &srcs.collect::<ArrayVec<&[u8], MAX_BLOCKS>>());
    }

    /// Data block @faila, and data block @failb or P, from the rest - a page
    /// at a time, as raid6's recovery goes.
    fn raid6_rec(blocks: &mut [&mut [u8]], faila: usize, failb: usize) {
        let (nd, bytes) = (blocks.len() - 2, blocks[0].len());

        for offset in (0..bytes).step_by(PAGE_SIZE) {
            let end = (offset + PAGE_SIZE).min(bytes);
            let mut page: ArrayVec<&mut [u8], MAX_BLOCKS> = blocks.iter_mut().map(|b| &mut b[offset..end]).collect();

            if failb < nd {
                recov_2data(&mut page, faila, failb)
            } else {
                recov_datap(&mut page, faila)
            }
        }
    }

    pub(super) fn generate(blocks: &mut [&mut [u8]], nd: usize, np: usize) {
        match np {
            1 => raid5_rec(blocks, nd),
            _ => gen_syndrome(blocks),
        }
    }

    pub(super) fn recover(blocks: &mut [&mut [u8]], nd: usize, np: usize, failed: &[u8]) {
        let f = |i: usize| usize::from(failed[i]);

        match failed.len() {
            // data, or P
            1 if f(0) <= nd => raid5_rec(&mut blocks[..nd + 1], f(0)),
            // Q
            1 => gen_syndrome(blocks),
            // data and data, or data and P
            2 if f(1) <= nd => raid6_rec(blocks, f(0), f(1)),
            // data and Q
            2 if f(0) < nd => {
                raid5_rec(&mut blocks[..nd + 1], f(0));
                gen_syndrome(blocks);
            }
            // P and Q
            _ => generate(blocks, nd, np),
        }
    }
}

// ── Userspace: raid/raid.h ───────────────────────────────────────────────

#[cfg(not(kernel))]
mod imp {
    use super::*;
    use std::sync::Once;

    /// raid_rec()'s zero block, a page: recover() goes a page at a time.
    static ZERO: [u8; PAGE_SIZE] = [0; PAGE_SIZE];

    /// raid_init(), which picks the implementations, and raid_zero(): before
    /// anything else of raid.h's.
    fn init() {
        static INIT: Once = Once::new();

        // SAFETY: once; @ZERO is only read
        INIT.call_once(|| unsafe {
            c::raid_init();
            c::raid_zero(ZERO.as_ptr().cast_mut().cast());
        });
    }

    pub(super) fn generate(blocks: &mut [&mut [u8]], nd: usize, np: usize) {
        init();
        let (mut p, bytes) = ptrs(blocks);
        // SAFETY: checked - each block's @bytes, and its own
        unsafe { c::raid_gen(nd as c_int, np as c_int, bytes, p.as_mut_ptr()) }
    }

    pub(super) fn recover(blocks: &mut [&mut [u8]], nd: usize, np: usize, failed: &[u8]) {
        init();
        let mut ir: ArrayVec<c_int, MAX_PARITY> = failed.iter().map(|&f| f.into()).collect();
        let bytes = blocks[0].len();

        for offset in (0..bytes).step_by(PAGE_SIZE) {
            let end = (offset + PAGE_SIZE).min(bytes);
            let mut page: ArrayVec<&mut [u8], MAX_BLOCKS> = blocks.iter_mut().map(|b| &mut b[offset..end]).collect();
            let (mut p, bytes) = ptrs(&mut page);

            // SAFETY: checked - each block's @bytes, and its own, and @ir as
            // recover() checked @failed; the zero block, a page, as long
            unsafe { c::raid_rec(ir.len() as c_int, ir.as_mut_ptr(), nd as c_int, np as c_int, bytes, p.as_mut_ptr()) }
        }
    }
}
