// SPDX-License-Identifier: GPL-2.0

//! util/raid.rs's parity, timed through fold() and recover() - whatever is
//! under them: a stripe's data folded into its parity as it's written, and
//! its blocks rebuilt, every way a stripe can lose them. In the kernel, as
//! perf_test's test_raid_bench; in userspace,
//!
//!     cargo bench -p bcachefs-kernel --bench raid

use super::TestRet;
use crate::c;
use crate::data_structures::array_vec::ArrayVec;
use crate::errcode::BchError;
use crate::os::bio::{io_buf, IoBuf};
use crate::os::kernel::local_clock;
use crate::prt;
use crate::util::alloc::Flags;
use crate::util::printbuf::Printbuf;
use crate::util::raid::{fold, recover};

#[cfg(not(kernel))]
use bcachefs_shim as kernel;

/// A block's bytes: a reconstructed read's window, and a bucket's worth
const BLOCKS: &[usize] = &[4 << 10, 256 << 10];

/// The stripes: data blocks and parity blocks
const SHAPES: &[(usize, usize)] = &[(4, 1), (4, 2), (4, 3), (8, 1), (8, 2), (8, 3)];

/// Picoseconds a call of @f takes: the best of five runs, each of enough
/// calls to take a tenth of a second
fn time(mut f: impl FnMut()) -> u64 {
    let run = |n: u64, f: &mut dyn FnMut()| {
        let start = local_clock();
        (0..n).for_each(|_| f());
        local_clock() - start
    };
    let mut n = 1;
    while run(n, &mut f) < 100_000_000 {
        n *= 2;
    }
    (0..5).map(|_| run(n, &mut f) * 1000 / n).min().unwrap_or(0)
}

/// @ps a call, over @nd data blocks of @bytes
fn report(what: core::fmt::Arguments<'_>, nd: usize, bytes: usize, ps: u64) {
    let mut msg = Printbuf::new();
    prt!(&mut msg, "raid: {what}\t{} MB/s of data\t{} ns\n",
         (nd * bytes) as u64 * 1_000_000 / ps.max(1), ps / 1000);
    kernel::pr_info!("{}", msg.as_str());
}

/// @f of every way to lose blocks that can be rebuilt: the first k data
/// blocks, and any of the parity, no more than @np in all
fn for_each_loss(nd: usize, np: usize, mut f: impl FnMut(&[u8])) {
    for k in 0..=np {
        for p in 0u32..1 << np {
            let n = k + p.count_ones() as usize;
            if n != 0 && n <= np {
                let lost: ArrayVec<u8, 8> = (0..k)
                    .chain((0..np).filter(|r| p & 1 << r != 0).map(|r| nd + r))
                    .map(|i| i as u8)
                    .collect();
                f(&lost);
            }
        }
    }
}

pub fn bench() -> TestRet {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    for &bytes in BLOCKS {
        for &(nd, np) in SHAPES {
            let mut blocks: ArrayVec<IoBuf, 16> = ArrayVec::new();
            for i in 0..nd + np {
                let mut b = io_buf(bytes, Flags::empty()).map_err(|_| BchError::from(c::ENOMEM))?;
                if i < nd {
                    b.fill_with(|| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed as u8
                    });
                }
                blocks.push(b);
            }
            let mut b: ArrayVec<&mut [u8], 16> = blocks.iter_mut().map(|b| &mut b[..]).collect();

            let (data, parity) = b.split_at_mut(nd);
            report(format_args!("fold {nd}+{np} {}k", bytes >> 10), nd, bytes, time(|| {
                parity.iter_mut().for_each(|p| p.fill(0));
                for (i, d) in data.iter().enumerate() {
                    fold(parity, d, i);
                }
            }));

            for_each_loss(nd, np, |lost| {
                report(format_args!("recover {nd}+{np} {}k {lost:?}", bytes >> 10), nd, bytes,
                       time(|| recover(&mut b, np, lost)));
            });
        }
    }
    Ok(())
}

#[cfg(CONFIG_BCACHEFS_TESTS)]
pub(super) fn test_raid_bench(_fs: &crate::fs::Fs, _nr: u64) -> TestRet {
    bench()
}
