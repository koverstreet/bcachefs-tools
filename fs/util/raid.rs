// SPDX-License-Identifier: GPL-2.0

//! RAID parity over a stripe's blocks - data, then parity, each a slice of
//! one length: Reed-Solomon over GF(2^8), parity block r the sum of A[r][j]
//! times data block j. A's first two rows are P and Q, raid5's xor and
//! raid6's sum of 2^j times block j, as Linux's raid6 makes them; past them,
//! snapraid's (raid/raid.c): with every square submatrix of A invertible, any
//! as many blocks as there's parity can be rebuilt from the rest.
//!
//! fold() adds a data block to the parity made so far; recover() rebuilds
//! the blocks it's told failed from the rest.

use crate::c;
use crate::data_structures::array_vec::ArrayVec;
use crate::os::simd;
use g2p::bulk::{self, MulTable};
use g2p::{GaloisField, GaloisField8, Matrix};

g2p::g2p!(Gf, 8, modulus: 0b1_0001_1101);

/// The most blocks a stripe has.
const MAX_BLOCKS: usize = c::BCH_BKEY_PTRS_MAX as usize;

/// The most parity blocks: A's rows.
const MAX_PARITY: usize = 6;

/// The most MulTables a multiply is given at once - 640 bytes of stack.
const MAX_TABLES: usize = 16;

/// A[r][j]: (x + 1) / (x + 2^-j), x the r'th of ∞, 0, 2, 4, 8 and 16 - P's 1,
/// Q's 2^j, then Cauchy rows, scaled so the first column is 1.
fn coeff(r: usize, j: usize) -> Gf {
    let two = Gf::from(2);
    let x = match r {
        0 => return Gf::ONE,
        1 => Gf::ZERO,
        _ => two.pow(r - 1),
    };
    (x + Gf::ONE) / (x + Gf::ONE / two.pow(j))
}

/// Data block @idx, @data, added to @parity - its first parity.len() rows -
/// as they'd be made with @data where a zero block was. Parity is linear, so a
/// stripe's is its data blocks' folded in one by one, in any order, from
/// zeroes.
pub fn fold(parity: &mut [&mut [u8]], data: &[u8], idx: usize) {
    assert!((1..=MAX_PARITY).contains(&parity.len()) && idx + parity.len() < MAX_BLOCKS,
            "raid: folding data block {idx} into {} parity blocks", parity.len());

    let m: ArrayVec<MulTable, MAX_PARITY> = (0..parity.len()).map(|r| coeff(r, idx).mul_table()).collect();
    simd::with(|| bulk::mul_add_matrix(parity, &[data], &m));
}

/// The blocks of @blocks that @failed names - in order, parity included -
/// rebuilt from the rest, @np of them parity.
pub fn recover(blocks: &mut [&mut [u8]], np: usize, failed: &[u8]) {
    let n = blocks.len();
    assert!((1..=MAX_PARITY).contains(&np) && n > np && n <= MAX_BLOCKS &&
            failed.len() <= np &&
            failed.is_sorted_by(|a, b| a < b) &&
            failed.last().is_none_or(|&f| usize::from(f) < n),
            "raid: rebuilding blocks {failed:?} of {n}, {np} of them parity");

    let nd = n - np;
    let lost = |i: usize| failed.contains(&(i as u8));

    // One block lost, P left or the one lost - raid5's, and the most common:
    // the sum of the others up to P, with nothing to work out first.
    if let &[f] = failed && usize::from(f) <= nd {
        let one = Gf::ONE.mul_table();
        return rebuild(blocks, lost, |i| i <= nd, |_, _| one);
    }

    let data: ArrayVec<usize, MAX_BLOCKS> = (0..nd).filter(|&j| !lost(j)).collect();
    let data_lost: ArrayVec<usize, MAX_PARITY> = (0..nd).filter(|&j| lost(j)).collect();
    let nl = data_lost.len();
    // as many parity blocks as there's data lost: the first
    let rows: ArrayVec<usize, MAX_PARITY> = (0..np).filter(|&r| !lost(nd + r)).take(nl).collect();

    // From what's left - the data, then those rows' parity, in block order -
    // the lost data: for parity rows R, A[R][lost] d_lost = A[R][data] d_data
    // + p_R.
    let a: Matrix<Gf, MAX_PARITY, MAX_PARITY> = Matrix::from_fn(nl, nl, |i, k| coeff(rows[i], data_lost[k]));
    let rhs: Matrix<Gf, MAX_PARITY, MAX_BLOCKS> = Matrix::from_fn(nl, nd, |i, k| match data.get(k) {
        Some(&j) => coeff(rows[i], j),
        None => Gf::from((k - data.len() == i) as u8),
    });
    let d = &a.inverse().expect("raid: every square submatrix of A is invertible") * &rhs;

    // and the lost parity, of all the data: A[lost parity][lost] d_lost +
    // A[lost parity][data] d_data
    let parity_lost = &failed[nl..];
    let r = |p: usize| usize::from(parity_lost[p]) - nd;
    let a_lost: Matrix<Gf, MAX_PARITY, MAX_PARITY> = Matrix::from_fn(parity_lost.len(), nl, |p, l| coeff(r(p), data_lost[l]));
    let p = &a_lost * &d;
    let m: Matrix<Gf, MAX_PARITY, MAX_BLOCKS> = Matrix::from_fn(failed.len(), nd, |o, k| match o.checked_sub(nl) {
        None => d[(o, k)],
        Some(o) => p[(o, k)] + data.get(k).map_or(Gf::ZERO, |&j| coeff(r(o), j)),
    });

    rebuild(blocks, lost, |i| i < nd || rows.contains(&(i - nd)), |o, k| m[(o, k)].mul_table());
}

/// The blocks of @blocks that @lost says, rebuilt: lost block o the sum of
/// @m(o, k) times the k'th block that @read says.
fn rebuild(blocks: &mut [&mut [u8]], lost: impl Fn(usize) -> bool, read: impl Fn(usize) -> bool,
           m: impl Fn(usize, usize) -> MulTable) {
    let mut outs: ArrayVec<&mut [u8], MAX_PARITY> = ArrayVec::new();
    let mut ins: ArrayVec<&[u8], MAX_BLOCKS> = ArrayVec::new();
    for (i, b) in blocks.iter_mut().enumerate() {
        if lost(i) {
            outs.push(&mut **b);
        } else if read(i) {
            ins.push(&**b);
        }
    }

    // Every lost block at once, a few inputs at a time: a whole matrix's
    // MulTables would be kilobytes of stack
    let cols = (MAX_TABLES / outs.len().max(1)).max(1);
    for (c, ins) in ins.chunks(cols).enumerate() {
        let t: ArrayVec<MulTable, MAX_TABLES> = (0..outs.len())
            .flat_map(|o| (0..ins.len()).map(move |k| (o, c * cols + k)))
            .map(|(o, k)| m(o, k))
            .collect();
        simd::with(|| match c {
            0 => bulk::mul_matrix(&mut outs, ins, &t),
            _ => bulk::mul_add_matrix(&mut outs, ins, &t),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// @nd blocks of noise, and @np of parity, folded from zeroes.
    fn stripe(nd: usize, np: usize) -> Vec<Vec<u8>> {
        let mut seed = 0x9e37_79b9_7f4a_7c15 ^ (nd * 8 + np) as u64;
        let mut blocks: Vec<Vec<u8>> = (0..nd).map(|_| (0..4096).map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        }).collect()).collect();
        let mut parity = vec![vec![0; 4096]; np];

        for (i, d) in blocks.iter().enumerate() {
            fold(&mut parity.iter_mut().map(|p| &mut p[..]).collect::<Vec<_>>(), d, i);
        }
        blocks.append(&mut parity);
        blocks
    }

    /// Every set of as many blocks as there's parity, and fewer, lost -
    /// scribbled over - and rebuilt.
    #[test]
    fn recover_rebuilds_what_fold_made() {
        for nd in 1..=8 {
            for np in 1..=MAX_PARITY.min(MAX_BLOCKS - nd) {
                let good = stripe(nd, np);
                let n = nd + np;
                for set in 0u32..1 << n {
                    if set.count_ones() as usize > np {
                        continue;
                    }
                    let failed: Vec<u8> = (0..n as u8).filter(|&i| set & 1 << i != 0).collect();
                    let mut blocks = good.clone();
                    failed.iter().for_each(|&f| blocks[usize::from(f)].fill(0xa5));

                    recover(&mut blocks.iter_mut().map(|b| &mut b[..]).collect::<Vec<_>>(), np, &failed);
                    assert!(blocks == good, "{nd}+{np}: blocks {failed:?} rebuilt wrong");
                }
            }
        }
    }

    /// recover()'s expect, for every stripe there can be - the one above
    /// only goes to 8 data blocks.
    #[test]
    fn every_square_submatrix_is_invertible() {
        for rows in 1u32..1 << MAX_PARITY {
            let rows: ArrayVec<usize, MAX_PARITY> = (0..MAX_PARITY).filter(|&r| rows & 1 << r != 0).collect();
            for cols in 1u32..1 << MAX_BLOCKS {
                if cols.count_ones() as usize != rows.len() {
                    continue;
                }
                let cols: ArrayVec<usize, MAX_PARITY> = (0..MAX_BLOCKS).filter(|&j| cols & 1 << j != 0).collect();
                let a: Matrix<Gf, MAX_PARITY, MAX_PARITY> =
                    Matrix::from_fn(rows.len(), rows.len(), |i, k| coeff(rows[i], cols[k]));
                assert!(a.inverse().is_some(), "A[{rows:?}][{cols:?}] singular");
            }
        }
    }

    /// The parity of raid/'s raid_gen(), snapraid's, for up to its 6.
    #[test]
    fn fold_is_raid_gen() {
        use crate::os::bio::io_buf;
        use crate::util::alloc::Flags;

        unsafe { c::raid_init() };
        for nd in 1..=8 {
            for np in 1..=MAX_PARITY {
                let ours = stripe(nd, np);
                let mut theirs: Vec<_> = ours.iter().enumerate().map(|(i, b)| {
                    let mut t = io_buf(4096, Flags::empty()).expect("a block");
                    if i < nd {
                        t.copy_from_slice(b);
                    }
                    t
                }).collect();
                let mut p: Vec<*mut core::ffi::c_void> = theirs.iter_mut().map(|b| b.as_mut_ptr().cast()).collect();
                unsafe { c::raid_gen(nd as _, np as _, 4096, p.as_mut_ptr()) };
                assert!(ours[nd..].iter().zip(&theirs[nd..]).all(|(o, t)| o[..] == t[..]),
                        "{nd}+{np}: parity isn't raid_gen()'s");
            }
        }
    }
}
