// SPDX-License-Identifier: GPL-2.0

//! Inode-number sharding's boundaries, in the btrees it shards.

use crate::btree::bkey::{spos, POS_MIN};
use crate::btree::interior::split_leaf;
use crate::btree::iter::{lockrestart_do, BtreeIterFlags, BtreeNodeIter, CommitOpts, TransAttempt};
use crate::btree::node::{BtreeNode, NodeContents};
use crate::c;
use crate::errcode::BchError;
use crate::format::{Bpos, Btree};
use crate::fs::Fs;
use crate::util::log::{LogLevel, LogMsg};
use crate::util::refcount::TryRefcount;
use crate::{bch_err, btree_trans, prt};

/*
 * Inode-number sharding (shard_inode_numbers_bits) splits concurrent
 * allocators across disjoint ranges of three btrees so they don't fight
 * for the same nodes:
 *
 *   inodes:  SPOS(0,    inum, snap)   — shard field is offset
 *   extents: SPOS(inum, offset, snap) — shard field is inode
 *   dirents: SPOS(inum, hash, snap)   — shard field is inode
 *   xattrs:  SPOS(inum, hash, snap)   — shard field is inode
 *
 * Shard boundaries on inum sit at multiples of
 * (1 << (63 - shard_inode_numbers_bits)).
 *
 * The helpers below let the splitter force a split at a shard boundary
 * and the merger refuse a merge that would straddle one — keeping the
 * leaf level shard-aligned over normal growth, no separate pre-split
 * pass needed.
 */

fn shard_field(btree: Btree, p: Bpos) -> Option<u64> {
    match btree {
        Btree::inodes => Some(p.offset),
        Btree::extents |
        Btree::dirents |
        Btree::xattrs  => Some(p.inode),
        _ => None,
    }
}

/*
 * Return the greatest shard-boundary split pivot strictly less than @key.
 *
 * For n shards there are n - 1 boundaries, at the starts of shards
 * 1..n - 1. Since btree split pivots are the last key in the left node,
 * the pivot for a boundary is bpos_predecessor(boundary start).
 *
 * POS_MIN if none (non-shard btree, sharding disabled, or @key already
 * in shard 0) — pivots by definition separate intervals, so POS_MIN is
 * a safe not-found sentinel.
 */
fn shard_pivot_below(fs: &Fs, btree: Btree, key: Bpos) -> Bpos {
    let shard_bits = fs.opts().shard_inode_numbers_bits.get() as u32;
    if shard_bits == 0 {
        return POS_MIN;
    }

    let bits = 63 - shard_bits;
    let Some(field) = shard_field(btree, key) else { return POS_MIN };

    let shard = field >> bits;
    if shard == 0 {
        return POS_MIN;
    }

    /*
     * Clamp to the largest valid boundary. Callers may pass SPOS_MAX (or
     * field values above the shard-encoded range) when looking up the pivot
     * for an unbounded leaf; those should map to the last real boundary,
     * not create a fake boundary next to SPOS_MAX.
     */
    let max_shard = (1u64 << shard_bits) - 1;
    let boundary = shard.min(max_shard) << bits;
    if boundary == 0 {
        return POS_MIN;
    }

    if btree == Btree::inodes {
        spos(0, boundary - 1, u32::MAX)
    } else {
        spos(boundary - 1, u64::MAX, u32::MAX)
    }
}

/*
 * True iff @key is at the start of a shard — the key immediately
 * before @key is in a different shard. Used at both ends of a node:
 *
 *   key_is_shard_boundary(fs, btree, b->data->min_key)
 *       — prev sib is in a different shard.
 *   key_is_shard_boundary(fs, btree, bpos_successor(b->data->max_key))
 *       — next sib is in a different shard.
 *
 * The POS_MIN guard avoids a phantom match at SPOS(0,0,1) when
 * pivot_below returns POS_MIN for shard 0.
 */
pub(crate) fn key_is_shard_boundary(fs: &Fs, btree: Btree, key: Bpos) -> bool {
    let p = shard_pivot_below(fs, btree, key);
    p != POS_MIN && key == p.successor()
}

impl BtreeNode {
    pub fn shard(&self, contents: &NodeContents, fs: &Fs) -> Option<usize> {
        let bits = fs.opts().shard_inode_numbers_bits.get() as u32;
        let field = shard_field(Btree(self.c.btree_id.into()), contents.key.k.p)?;
        (bits != 0).then(|| ((field << 1) >> (64 - bits)) as usize)
    }

    /*
     * Return the shard-boundary pivot strictly inside (b->min_key, b->max_key)
     * — the rightmost boundary strictly above min_key and strictly below
     * max_key. POS_MIN if none, or if @b isn't a leaf.
     *
     * Passing max_key to pivot_below already enforces the pivot < max_key
     * half; a right-edge-aligned leaf still finds the next-smaller boundary
     * inside, so successive rewrites converge toward full shard alignment.
     */
    pub fn shard_pivot(&self, contents: &NodeContents, fs: &Fs) -> Bpos {
        if self.c.level != 0 {
            return POS_MIN;
        }

        let data = contents.data();
        let p = shard_pivot_below(fs, Btree(self.c.btree_id.into()), data.max_key);
        if p > data.min_key { p } else { POS_MIN }
    }
}

/*
 * For each shard boundary in each shard-aware btree, look up the leaf
 * containing it; if the leaf spans the boundary, rewrite it (which forces
 * a split at the boundary, since bch2_btree_node_alloc_replacement honors
 * the "leaves don't cross shard boundaries" constraint).
 */
/// Whether @pos's leaf spanned a shard boundary, and was split at it.
fn split_at_shard_boundary(t: &TransAttempt<'_, '_>, btree: Btree, pos: Bpos) -> Result<bool, BchError> {
    let mut iter = BtreeNodeIter::new(t, btree, pos, 0, 0, BtreeIterFlags::empty());
    iter.iter().traverse(t)?;
    let path = iter.iter().path;

    let trans = t.trans();
    {
        let paths = trans.paths();
        let b = paths.held_btree_node(path, 0).expect("the leaf its iterator traversed to");
        let pivot = b.shard_pivot(b.contents(), t.fs());
        if pivot == POS_MIN || pivot == b.contents().key_i().k.p {
            return Ok(false);
        }
    }

    split_leaf(t, &mut trans.paths(), path, 0, CommitOpts::new())?;
    Ok(true)
}

impl Fs {
    /// Startup doesn't need it to succeed: an error ends that btree's splits,
    /// and is logged.
    pub fn presplit_shard_boundaries(&self) {
        let shard_bits = u32::from(self.opts.shard_inode_numbers_bits.get());
        if shard_bits == 0 {
            return;
        }

        let Some(_write) = self.writes.tryget_live() else { return };

        let bits = 63 - shard_bits;
        let n_shards = 1u64 << shard_bits;

        let trans = btree_trans!(self);

        for btree in [Btree::inodes, Btree::extents, Btree::dirents, Btree::xattrs] {
            let mut split_count = 0;

            let ret = (1..n_shards).try_for_each(|shard| -> Result<(), BchError> {
                let boundary = shard << bits;
                let pos = if btree == Btree::inodes { spos(0, boundary, 0) } else { spos(boundary, 0, 0) };
                while lockrestart_do(&trans, |t| split_at_shard_boundary(t, btree, pos))? {
                    split_count += 1;
                }
                Ok(())
            });

            if split_count != 0 {
                let mut msg = LogMsg::with_level(self, LogLevel::INFO);
                prt!(&mut msg, "presplit_shard_boundaries: {split_count} splits in btree {btree}");
            }

            if let Err(e) = ret {
                if !e.matches(c::ENOSPC) {
                    bch_err!(self, "presplit_shard_boundaries: error splitting btree {btree} at its shard boundaries, \
                                    after {split_count} splits: {}; the rest left as they are", e.msg());
                }
            }
        }
    }
}

crate::recovery_pass!(presplit_shard_boundaries => |fs: &Fs| -> Result<(), BchError> {
    fs.presplit_shard_boundaries();
    Ok(())
});
