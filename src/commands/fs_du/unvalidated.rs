//! Forked canonical code: what fs du needs from two kernel helpers that take
//! only validated keys, for the unvalidated keys BCH_IOCTL_QUERY_BTREE_KEYS
//! hands out (online_iter, since upstream fe816e170).
//!
//! - extent_ptr_to_bp() forks bcachefs_kernel::alloc::backpointers::
//!   extent_ptr_to_bp() and its extent_ptr_to_bp_pos() and ptr_data_type(),
//!   for level 0 extent and reflink_v keys only, the ones sample.rs traces a
//!   backpointer to. The reconcile phys bits are left zero: backpointers_match()
//!   ignores them.
//! - dirent_name() forks Dirent::new() and dirent_points_to_inode(): a dirent
//!   whose names overrun its value is Err, where Dirent::from() would panic,
//!   as only validate rules that out.
//!
//! Revert to the canonical functions once they accept unvalidated keys, as
//! alloc_to_v4() already does.

use bcachefs_kernel::alloc::backpointers::BackpointerPos;
use bcachefs_kernel::btree::bkey::prelude::*;
use bcachefs_kernel::btree::bkey::{pos, AnyKeySC};
use bcachefs_kernel::c;
use bcachefs_kernel::data::ptrs::{ptr_disk_sectors, DecodedPtr};
use bcachefs_kernel::dirent::{DirentTarget, RawNames};
use bcachefs_kernel::format::{BkeyType, Bpos, Btree, DataType};
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::util::os_str::OsStr;
use bcachefs_kernel::util::vstructs::Unvalidated;

/// The backpointer @d, a pointer of @k, a level 0 key of @btree, has: its
/// position and value. None if @k isn't an extent or reflink_v.
pub(super) fn extent_ptr_to_bp(fs: &Fs, btree: Btree, k: AnyKeySC<'_, Unvalidated>,
                               d: &DecodedPtr<'_>) -> Option<(Bpos, c::Backpointer)> {
    if !matches!(k.k.key_type(), BkeyType::extent | BkeyType::reflink_v) {
        return None;
    }

    let p = &d.p;
    let shift = fs.sb.extent_bp_shift.get();
    let mut bp_pos = pos(p.ptr.dev().into(), (p.ptr.offset() << shift) + u64::from(p.crc.offset));
    if u32::from(p.ptr.dev()) == c::BCH_SB_MEMBER_INVALID && p.has_ec {
        bp_pos.inode = BackpointerPos::ec_removed_dev(p.ec.idx(), p.ec.block().into());
    }

    let data_type = if p.has_ec {
        DataType::stripe
    } else if p.ptr.cached() != 0 {
        DataType::cached
    } else {
        DataType::user
    };

    let mut bp = c::Backpointer {
        btree_id:   btree.0 as u8,
        level:      0,
        data_type:  data_type.0 as u8,
        bucket_gen: p.ptr.generation(),
        bucket_len: ptr_disk_sectors(k.k.size.into(), p) as u32,
        pos:        k.k.p,
        ..Default::default()
    };
    bp.set_erasure_coded(p.has_ec);
    bp.set_stripe_ptr(p.has_ec && u32::from(p.ptr.dev()) == c::BCH_SB_MEMBER_INVALID);
    Some((bp_pos, bp))
}

/// A dirent whose value is shorter than a dirent's, or whose names overrun it.
pub(super) struct MalformedDirent;

/// @k's name if it's a dirent naming @inode, as dirent_points_to_inode()
/// matches them: a DT_SUBVOL dirent by subvolume, any other by inode number.
pub(super) fn dirent_name<'k>(k: AnyKeySC<'k, Unvalidated>, inode: &c::bch_inode_unpacked)
                              -> Result<Option<&'k OsStr>, MalformedDirent> {
    if k.k.key_type() != BkeyType::dirent {
        return Ok(None);
    }
    if k.k.val_bytes() < k.k.key_type().min_val_size() {
        return Err(MalformedDirent);
    }
    let d = k.arm::<c::Dirent>().ok_or(MalformedDirent)?;

    let names_it = match d.v().target() {
        DirentTarget::Subvol { child, .. } => child == inode.bi_subvol,
        DirentTarget::Inode(inum)          => inum == inode.bi_inum,
    };
    if !names_it {
        return Ok(None);
    }
    RawNames::new(k).names()
        .map(|n| Some(n.name()))
        .ok_or(MalformedDirent)
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;
    use bcachefs_kernel::btree::bkey::{AnyKey, BkeySC, BKEY_U64S};
    use bcachefs_kernel::util::vstructs::{from_bytes, plain_bytes, plain_bytes_mut};

    const DT_REG: u8 = 8;

    /// A dirent key with @val as its value, zero padded to a u64.
    fn dirent_key(val: &[u8]) -> Vec<u64> {
        let u64s = BKEY_U64S + val.len().div_ceil(8);
        let mut k = c::bkey::new();
        k.u64s = u64s as u8;
        k.type_ = BkeyType::dirent.0 as u8;

        let mut buf = vec![0u64; u64s];
        let bytes = plain_bytes_mut(&mut buf[..]);
        let hdr = plain_bytes(&k);
        bytes[..hdr.len()].copy_from_slice(hdr);
        bytes[hdr.len()..][..val.len()].copy_from_slice(val);
        buf
    }

    /// The value of a dirent naming inode @inum: target, bits, then the name.
    fn plain_dirent(inum: u64, name: &[u8]) -> Vec<u8> {
        let mut v = inum.to_le_bytes().to_vec();
        v.push(DT_REG);
        v.extend_from_slice(name);
        v
    }

    fn name_of(buf: &[u64], inode: &c::bch_inode_unpacked) -> Result<Option<Vec<u8>>, MalformedDirent> {
        let k = from_bytes::<AnyKey<Unvalidated>>(plain_bytes(buf)).expect("a key");
        dirent_name(BkeySC::from(k), inode).map(|n| n.map(|n| n.as_bytes().to_vec()))
    }

    fn inode(inum: u64) -> c::bch_inode_unpacked {
        c::bch_inode_unpacked { bi_inum: inum, ..Default::default() }
    }

    #[test]
    fn dirent_naming_the_inode_gives_its_name() {
        let k = dirent_key(&plain_dirent(42, b"some_name"));
        assert_eq!(name_of(&k, &inode(42)).ok(), Some(Some(b"some_name".to_vec())));
        assert_eq!(name_of(&k, &inode(7)).ok(), Some(None));
    }

    #[test]
    fn dirent_shorter_than_a_dirent_is_malformed() {
        let k = dirent_key(&42u64.to_le_bytes());
        assert!(name_of(&k, &inode(42)).is_err());
    }

    #[test]
    fn dirent_name_past_the_value_is_malformed() {
        // casefolded, d_name_len 200 in a 16 byte value
        let mut v = 42u64.to_le_bytes().to_vec();
        v.extend_from_slice(&[DT_REG | 0x80, 0, 0, 200, 0, 0, 0]);
        let k = dirent_key(&v);
        assert!(name_of(&k, &inode(42)).is_err());
    }
}
