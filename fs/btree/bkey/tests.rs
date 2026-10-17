use super::*;
use crate::c;
use crate::util::vstructs::ArmRef;
use crate::types::Le32;

/// A snapshot key from before depth, skip and btime - 24 bytes of its 64 -
/// reads as a snapshot: the fields it has as they are, the rest zero, not
/// what's in the buffer past it.
#[test]
fn bkey_short_value() {
    assert_eq!(c::Snapshot::MIN_VAL_SIZE, 24);

    let mut k = BkeyI::<c::Snapshot>::new();
    k.v.subvol = Le32::new(5);
    k.v.children = [Le32::new(3), Le32::new(4)];
    k.v.depth = Le32::new(7);
    k.k.set_val_bytes(24);

    let s = BkeySC::from(k.as_ref()).arm::<c::Snapshot>().expect("a snapshot").v();
    assert_eq!((s.subvol.get(), s.children[0].get(), s.children[1].get()), (5, 3, 4));
    assert_eq!(s.depth().get(), 0);
    assert!(s.whole().is_none());
    assert!(BkeySC::from(k.as_ref()).arm::<c::Subvolume>().is_none());
}

/// A key by its type: a grown type's value its family's, an older value
/// of it read as one - inline too: dispatch is by type, as C's.
#[test]
fn bkey_typed_short_values() {
    let mut k = BkeyI::<c::Subvolume>::new();
    k.v.snapshot = Le32::new(5);
    k.k.set_val_bytes(16);
    match BkeySC::from(k.as_ref()).typed() {
        Typed::subvolume(s) =>
            assert_eq!((s.v().snapshot.get(), s.v().fs_path_parent().get()), (5, 0)),
        _ => panic!("a subvolume"),
    }
    match k.as_ref().typed() {
        Typed::subvolume(s) =>
            assert_eq!((s.v.snapshot.get(), s.v.fs_path_parent().get()), (5, 0)),
        _ => panic!("a subvolume"),
    }

    let mut g = BkeyI::<c::InodeGeneration>::new();
    g.v.bi_generation = Le32::new(3);
    assert_eq!(BkeySC::from(g.as_ref()).arm::<c::InodeGeneration>().map(|g| g.v().bi_generation.get()),
               Some(3));
    assert!(matches!(g.as_ref().typed(), Typed::inode_generation(_)));
}

/// A snapshot can't be had over fewer bytes than every snapshot has -
/// its floor, 24 - rather than reading past them; over 24, it can.
#[test]
fn bkey_unfloored() {
    let b = [0u64; 3];
    // SAFETY: b's bytes, aligned for any value
    let b = unsafe { core::slice::from_raw_parts(b.as_ptr().cast::<u8>(), size_of_val(&b)) };
    use crate::util::vstructs::Validated;
    assert!(<c::Snapshot<Validated> as ArmRef>::arm_ref(&b[..16]).is_none());
    assert!(<c::Snapshot<Validated> as ArmRef>::arm_ref(&b[..24]).is_some());
}

/// A subvolume key from before fs_path_parent - 16 bytes of its 48 - edited
/// in place has only the fields it has: one it lacks has no setter, a
/// write the key wouldn't carry. One grown whole does.
#[test]
fn bkey_edit_short_value() {
    let mut k = BkeyI::<c::Subvolume>::new();
    k.k.set_val_bytes(16);
    let k = k.as_mut();

    assert!(k.arm::<c::Snapshot>().is_none());

    let s = BkeyS::from(&mut *k);
    let Typed::subvolume(mut s) = s.typed() else { panic!("a subvolume") };
    s.v_mut().snapshot = Le32::new(5);

    let mut full = BkeyI::<c::Subvolume>::new();
    full.v.fs_path_parent = Le32::new(9);
    assert_eq!(full.v.fs_path_parent, 9);
    let s = BkeySC::from(&*k).arm::<c::Subvolume>().expect("a subvolume").v();
    assert_eq!((s.snapshot.get(), s.fs_path_parent().get()), (5, 0));
}

#[test]
fn bpos_successor_predecessor_carry() {
    let cases = [
        (spos(1, 2, 3),               spos(1, 2, 4)),
        (spos(1, 2, u32::MAX),        spos(1, 3, 0)),
        (spos(1, u64::MAX, u32::MAX), spos(2, 0, 0)),
        (POS_MIN,                     spos(0, 0, 1)),
    ];

    for (p, next) in cases {
        assert_eq!(p.successor(), next, "successor of {p}");
        assert_eq!(next.predecessor(), p, "predecessor of {next}");
    }
}

#[test]
#[should_panic(expected = "no position after SPOS_MAX")]
fn bpos_successor_of_max() {
    SPOS_MAX.successor();
}

#[test]
#[should_panic(expected = "no position before POS_MIN")]
fn bpos_predecessor_of_min() {
    POS_MIN.predecessor();
}
