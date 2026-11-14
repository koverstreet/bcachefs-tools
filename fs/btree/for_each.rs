// SPDX-License-Identifier: GPL-2.0

use super::bkey::*;
use super::types::{BtreeIter, BtreeTrans};
use super::iter::{polonius_key, BtreeIterFlags, BtreeNodeIter};
use super::trans_attempt::{is_restart, lockrestart_do, lockrestart_do_in, CommitOpts, TransAttempt};
use crate::alloc::capacity::DiskReservation;
use crate::btree::node::BtreeNode;
use crate::btree::path::{LockedNode, Paths};
use crate::errcode::{BchError, Errcode};
use crate::fs::Fs;
use crate::SPOS_MAX;
use core::ops::ControlFlow;
use crate::data::ptrs::BtreePtrs;
use crate::format::{Bpos, Btree};

/// What a for_each body returns on success: whether the loop goes on - to the
/// next key, or from a position of the body's - and if not, what it stopped
/// with. `()` for a body that always goes on to the next key - most of them,
/// as a C loop body returns 0 - `ControlFlow` for one that can stop early,
/// breaking with what the loop returns, and `LoopFlow` for one that can also
/// move the loop on.
///
/// A loop that runs to the end returns Break's default: `()`, `None`,
/// `false` - nothing found.
pub trait LoopControl {
    type Break: Default;

    fn into_step(self) -> Step<Self::Break>;
}

/// Where a for_each loop goes after a key.
pub enum Step<B> {
    /// The next key.
    Next,
    /// The keys from @pos, in place of the next. A C loop body's
    /// bch2_btree_iter_set_pos(p) is followed by the loop's advance, from p:
    /// its From is where that lands - p's successor, or for extents p.
    From(Bpos),
    Break(B),
}

impl LoopControl for () {
    type Break = ();

    fn into_step(self) -> Step<()> { Step::Next }
}

impl<B: Default> LoopControl for ControlFlow<B> {
    type Break = B;

    fn into_step(self) -> Step<B> {
        match self {
            ControlFlow::Continue(()) => Step::Next,
            ControlFlow::Break(b)     => Step::Break(b),
        }
    }
}

/// ControlFlow, and continuing from a position: for a body that moves the
/// loop on - past the rest of an inode's snapshot versions, say.
pub enum LoopFlow<B = ()> {
    Continue,
    ContinueFrom(Bpos),
    Break(B),
}

impl<B: Default> LoopControl for LoopFlow<B> {
    type Break = B;

    fn into_step(self) -> Step<B> {
        match self {
            LoopFlow::Continue          => Step::Next,
            LoopFlow::ContinueFrom(pos) => Step::From(pos),
            LoopFlow::Break(b)          => Step::Break(b),
        }
    }
}

/// A for_each loop's body, called on each key with the key's attempt and the
/// iterator.
///
/// Its result is two things, kept apart: whether its work succeeded
/// (`Result`), and whether to go on (@R, LoopControl: `()` for a body that
/// always goes on, `ControlFlow` for one that can stop early - what it
/// breaks with is what the loop returns).
///
/// What it does beyond reading the key - a lookup, an update - is in the
/// key's attempt.
///
/// It gets the iterator as the C loop bodies do - to update at its position,
/// or read where it is - shared, borrowed with the key: the key's header is
/// in the iterator, and its value in the node the iterator's path holds, so
/// nothing may move the iterator while the key is in use. A body that needs
/// to move it is a loop of its own.
pub trait KeyFn<'t, R>:
    for<'a> FnMut(&'a TransAttempt<'_, 't>, &'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>
{}

impl<'t, R, F> KeyFn<'t, R> for F
where
    F: for<'a> FnMut(&'a TransAttempt<'_, 't>, &'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
{}

/// The keys a for_each loop walks, and how it goes on from one.
#[derive(Clone, Copy)]
pub(crate) enum Keys {
    /// Forwards, up to @end, peeking as the flags say.
    Max(Bpos, BtreeIterFlags),
    /// Backwards, down to @min.
    Reverse(Bpos),
    /// As Max, at a subvolume's snapshot, read again in each attempt:
    /// snapshotting the subvolume gives it a new one.
    InSubvolume(Bpos, BtreeIterFlags, u32),
}

impl Keys {
    /// At the start of each attempt.
    fn begin(self, t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, iter: &mut BtreeIter<'_>)
        -> Result<(), BchError>
    {
        if let Keys::InSubvolume(_, _, subvol) = self {
            let snapshot = paths.lend(|| crate::snapshots::subvolume::get_snapshot(t, subvol))?;
            iter.set_snapshot_in(paths, snapshot);
        }
        Ok(())
    }

    /// The next key, under attempt @t, and @iter, which it borrows, shared -
    /// so nothing moves the iterator while they're in use.
    fn peek<'k, 't>(&self, t: &'k TransAttempt<'_, 't>, paths: &mut Paths<'_>, iter: &'k mut BtreeIter<'t>)
        -> Result<Option<(&'k BtreeIter<'t>, BkeySC<'k>)>, BchError>
    {
        match self {
            Keys::Max(end, flags) | Keys::InSubvolume(end, flags, _) =>
                iter.peek_with(t, paths, |iter, t, paths| iter.peek_max_type_in(t, paths, *end, *flags)),
            Keys::Reverse(min) =>
                iter.peek_with(t, paths, |iter, t, paths| iter.peek_prev_min_in(t, paths, *min)),
        }
    }

    /// Past the key peeked: false at the btree's end - or for a walk forwards,
    /// when the key ended at SPOS_MAX and the position can't move on, as the
    /// rightmost key of any interior node level does. Looping again would
    /// peek the same key forever.
    fn advance(self, paths: &mut Paths<'_>, iter: &mut BtreeIter<'_>) -> bool {
        match self {
            Keys::Reverse(_) => iter.rewind_in(paths),
            _                => iter.advance_in(paths),
        }
    }
}

/// lockrestart_do(), committing after @f.
pub fn commit_do<'t, F>(
    trans: &BtreeTrans<'t>,
    disk_res: Option<&DiskReservation<'_>>,
    flags: impl Into<CommitOpts>,
    mut f: F,
) -> Result<(), BchError>
where
    F: for<'a> FnMut(&TransAttempt<'a, 't>) -> Result<(), BchError>,
{
    let flags = flags.into();
    lockrestart_do(trans, |t| {
        // If @f fails, the commit - which resets the updates whether it
        // succeeds or fails - never runs. An error that isn't a restart
        // abandons the attempt: drop what @f queued, as the commit would
        // have, so the next begin doesn't take them for updates someone
        // forgot to commit. A restart's are dropped by the restart. As C's
        // __commit_do().
        if let Err(e) = f(t) {
            if !is_restart(&e) {
                let trans = t.trans();
                trans.reset_updates(&mut trans.paths());
            }
            return Err(e);
        }
        t.commit(disk_res, flags)
    })
}

/// commit_do() in a transaction of its own.
pub fn trans_commit_do<'t, F>(
    fs: &'t Fs,
    disk_res: Option<&DiskReservation<'_>>,
    flags: impl Into<CommitOpts>,
    f: F,
) -> Result<(), BchError>
where
    F: for<'a> FnMut(&TransAttempt<'a, 't>) -> Result<(), BchError>,
{
    let trans = crate::btree_trans!(fs);
    commit_do(&trans, disk_res, flags, f)
}

impl<'t> BtreeIter<'t> {
    /// The next key of the walk @keys, given to @f in attempt @t: where the
    /// walk goes from it, None at the walk's end.
    fn key<F, R>(&mut self, t: &TransAttempt<'_, 't>, paths: &mut Paths<'_>, keys: Keys, f: &mut F)
        -> Result<Option<Step<R::Break>>, BchError>
    where
        F: KeyFn<'t, R>,
        R: LoopControl,
    {
        match keys.peek(t, paths, self)? {
            Some((iter, k)) => Ok(Some(paths.lend(|| f(t, iter, k))?.into_step())),
            None => Ok(None),
        }
    }

    /// On from a key, as the walk @keys goes: Some, what the walk returns.
    fn move_on<B: Default>(&mut self, paths: &mut Paths<'_>, keys: Keys, step: Option<Step<B>>) -> Option<B> {
        match step {
            None                  => Some(B::default()),
            Some(Step::Break(b))  => Some(b),
            Some(Step::From(pos)) => {
                self.set_pos_in(paths, pos);
                None
            }
            Some(Step::Next)      => (!keys.advance(paths, self)).then(B::default),
        }
    }

    /// The loop behind the for_each family: each key of the walk @keys in an
    /// attempt of its own, given to @f. A transaction restart, from the peek
    /// or from @f, retries the key; any other error is returned.
    pub(crate) fn walk<F, R>(&mut self, trans: &BtreeTrans<'t>, keys: Keys, mut f: F) -> Result<R::Break, BchError>
    where
        F: KeyFn<'t, R>,
        R: LoopControl,
    {
        let mut paths = trans.paths();
        loop {
            let step = lockrestart_do_in(trans, &mut paths, |t, paths| {
                keys.begin(t, paths, self)?;
                self.key(t, paths, keys, &mut f)
            })?;
            if let Some(b) = self.move_on(&mut paths, keys, step) {
                return Ok(b);
            }
        }
    }

    /// walk(), inside the caller's attempt @t: a restart is returned, for the
    /// loop the caller is in.
    fn walk_norestart<F, R>(&mut self, t: &TransAttempt<'_, 't>, keys: Keys, mut f: F)
        -> Result<R::Break, BchError>
    where
        F: KeyFn<'t, R>,
        R: LoopControl,
    {
        let mut paths = t.trans().paths();
        keys.begin(t, &mut paths, self)?;
        loop {
            let step = self.key(t, &mut paths, keys, &mut f)?;
            if let Some(b) = self.move_on(&mut paths, keys, step) {
                return Ok(b);
            }
        }
    }

    /// walk(), the keys in one attempt, not one each: a restart begins
    /// another at the key that restarted, as does a key ended with
    /// ignore_restarts(). @f may unlock: the next key relocks.
    pub(crate) fn walk_in_trans<F, R>(&mut self, trans: &BtreeTrans<'t>, keys: Keys, mut f: F)
        -> Result<R::Break, BchError>
    where
        F: KeyFn<'t, R>,
        R: LoopControl,
    {
        let mut paths = trans.paths();
        loop {
            if let Some(b) = lockrestart_do_in(trans, &mut paths,
                                               |t, paths| self.walk_attempt(t, paths, keys, &mut f))? {
                return Ok(b);
            }
        }
    }

    /// walk_in_trans()'s keys in attempt @t: what the walk returns, or None
    /// if a key ended the attempt with ignore_restarts().
    fn walk_attempt<F, R>(&mut self, t: &TransAttempt<'_, 't>, paths: &mut Paths<'_>, keys: Keys, f: &mut F)
        -> Result<Option<R::Break>, BchError>
    where
        F: KeyFn<'t, R>,
        R: LoopControl,
    {
        keys.begin(t, paths, self)?;
        loop {
            t.trans().relock_from(paths)?;
            let step = self.key(t, paths, keys, f)?;
            t.verify_not_restarted();
            if let Some(b) = self.move_on(paths, keys, step) {
                return Ok(Some(b));
            }
            if t.is_spent() {
                return Ok(None);
            }
        }
    }

    /// The keys up to @end, peeking as the iterator's own flags say - for a
    /// SLOTS iterator, a slot at a time, holes included: the for_each
    /// family's, as C's loops' are, so no loop can ignore its iterator's
    /// flags.
    fn keys_to(&self, end: Bpos) -> Keys {
        Keys::Max(end, self.iter_flags())
    }

    pub fn for_each_max<F, R>(&mut self, trans: &BtreeTrans<'t>, end: Bpos, f: F)
        -> Result<R::Break, BchError>
    where
        F: KeyFn<'t, R>,
        R: LoopControl,
    {
        self.walk(trans, self.keys_to(end), f)
    }

    /// Walk the keys from the iterator's position, inside the caller's attempt
    /// @t. Unlike the rest of the family this doesn't begin or retry anything -
    /// a restart is returned, for the loop the caller is in.
    pub fn for_each_norestart<F, R>(&mut self, t: &TransAttempt<'_, 't>, f: F)
        -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(&'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.for_each_max_norestart(t, SPOS_MAX, f)
    }

    /// As for_each_norestart(), up to and including @end.
    pub fn for_each_max_norestart<F, R>(&mut self, t: &TransAttempt<'_, 't>, end: Bpos, mut f: F)
        -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(&'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.walk_norestart(t, self.keys_to(end), |_, iter, k| f(iter, k))
    }

    /// As for_each_max_norestart(), stopping at the first key @f picks: that
    /// key, borrowed from the iterator, which is left at it, and no longer
    /// than attempt @t; None if @f picks none. As Iterator::find().
    pub fn find_max_norestart<'i, F>(&'i mut self, t: &'i TransAttempt<'_, 't>, end: Bpos,
                                     mut f: F)
        -> Result<Option<BkeySC<'i>>, BchError>
    where
        F: for<'k> FnMut(&'k BtreeIter<'t>, BkeySC<'k>) -> Result<bool, BchError>,
    {
        let mut paths = t.trans().paths();
        loop {
            let Some((iter, k)) = self.keys_to(end).peek(t, &mut paths, self)? else {
                return Ok(None);
            };

            if paths.lend(|| f(iter, k))? {
                return Ok(Some(unsafe { polonius_key(k) }));
            }
            if !self.advance_in(&mut paths) {
                return Ok(None);
            }
        }
    }

    /// As for_each_norestart(), backwards from the iterator's position down to
    /// @min.
    pub fn for_each_reverse_norestart<F, R>(&mut self, t: &TransAttempt<'_, 't>, min: Bpos,
                                            mut f: F)
        -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(&'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.walk_norestart(t, Keys::Reverse(min), |_, iter, k| f(iter, k))
    }

    pub fn for_each<F, R>(&mut self, trans: &BtreeTrans<'t>, f: F) -> Result<R::Break, BchError>
    where
        F: KeyFn<'t, R>,
        R: LoopControl,
    {
        self.for_each_max(trans, SPOS_MAX, f)
    }

    /// As walk(), committing @f's updates after each key when @commit says
    /// how (None: @f commits for itself). A restart from @f or the commit
    /// retries the key.
    ///
    /// @f finishes a key in one of three ways:
    ///  - Ok(()): commit what it queued, and go on to the next key - also
    ///    how a body finishes a key early, keeping its repairs.
    ///  - Err(fc_continue): go on to the next key *without* committing. What
    ///    was queued is dropped - and bch2_trans_begin()'s dropped-updates
    ///    check reports it - so this is for a body that has queued nothing,
    ///    or wants it discarded.
    ///  - Err(fc_break): stop the walk, not an error, without committing.
    ///
    /// As in C, fc_continue and fc_break come from the body itself, never
    /// passed up from a helper: they name *a* loop, and one that travels
    /// through a helper's own loop names the wrong one. Helpers return what
    /// happened; the body decides what that means for the walk.
    pub(crate) fn walk_commit<F>(
        &mut self,
        trans:  &BtreeTrans<'t>,
        keys:   Keys,
        commit: Option<(Option<&DiskReservation<'_>>, CommitOpts)>,
        mut f:  F,
    ) -> Result<(), BchError>
    where
        F: KeyFn<'t, ()>,
    {
        let mut paths = trans.paths();
        loop {
            let step = lockrestart_do_in(trans, &mut paths, |t, paths| {
                keys.begin(t, paths, self)?;
                match self.key(t, paths, keys, &mut f) {
                    Err(e) if e.matches(Errcode::fc_continue) => Ok(Some(Step::Next)),
                    Err(e) if e.matches(Errcode::fc_break)    => Ok(Some(Step::Break(()))),
                    Ok(Some(step)) => {
                        // a spent attempt's step has nothing queued: what
                        // ignore_restarts() ran committed for itself
                        if let Some((disk_res, flags)) = commit.filter(|_| !t.is_spent()) {
                            paths.lend(|| t.commit(disk_res, flags))?;
                        }
                        Ok(Some(step))
                    }
                    r => r,
                }
            })?;
            if let Some(()) = self.move_on(&mut paths, keys, step) {
                return Ok(());
            }
        }
    }

    /// Walk the keys from the iterator's position, @f getting each key's
    /// attempt and committing for itself - for a body with work to do after its
    /// commit, which a restart there would get wrong. Otherwise as
    /// for_each_commit().
    pub fn for_each_attempt<F>(&mut self, trans: &BtreeTrans<'t>, f: F) -> Result<(), BchError>
    where
        F: KeyFn<'t, ()>,
    {
        self.walk_commit(trans, Keys::Max(SPOS_MAX, BtreeIterFlags::empty()), None, f)
    }

    pub fn for_each_commit<F>(
        &mut self,
        trans:    &BtreeTrans<'t>,
        disk_res: Option<&DiskReservation<'_>>,
        flags:    impl Into<CommitOpts>,
        f:        F,
    ) -> Result<(), BchError>
    where
        F: KeyFn<'t, ()>,
    {
        self.walk_commit(trans, Keys::Max(SPOS_MAX, BtreeIterFlags::empty()), Some((disk_res, flags.into())), f)
    }

    /// As for_each_commit(), from the iterator's position back down to @min.
    pub fn for_each_reverse_commit<F>(
        &mut self,
        trans:    &BtreeTrans<'t>,
        min:      Bpos,
        disk_res: Option<&DiskReservation<'_>>,
        flags:    impl Into<CommitOpts>,
        f:        F,
    ) -> Result<(), BchError>
    where
        F: KeyFn<'t, ()>,
    {
        self.walk_commit(trans, Keys::Reverse(min), Some((disk_res, flags.into())), f)
    }

    pub fn for_each_max_commit<F>(
        &mut self,
        trans:      &BtreeTrans<'t>,
        end:        Bpos,
        iter_flags: BtreeIterFlags,
        disk_res:   Option<&DiskReservation<'_>>,
        flags:      impl Into<CommitOpts>,
        f:          F,
    ) -> Result<(), BchError>
    where
        F: KeyFn<'t, ()>,
    {
        self.walk_commit(trans, Keys::Max(end, iter_flags), Some((disk_res, flags.into())), f)
    }

    pub fn for_each_reverse<F, R>(&mut self, trans: &BtreeTrans<'t>, min: Bpos, f: F)
        -> Result<R::Break, BchError>
    where
        F: KeyFn<'t, R>,
        R: LoopControl,
    {
        self.walk(trans, Keys::Reverse(min), f)
    }

}

impl<'t> BtreeNodeIter<'t> {
    /// for_each_btree_node(): @f's result as BtreeIter::for_each()'s body's. A
    /// restart retries the node.
    pub fn for_each<F, R>(&mut self, trans: &BtreeTrans<'_>, mut f: F) -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(LockedNode<'a, BtreeNode>) -> Result<R, BchError>,
        R: LoopControl,
    {
        let mut paths = trans.paths();
        loop {
            let step = lockrestart_do_in(trans, &mut paths, |t, paths| {
                let Some(b) = self.raw.peek_node_in(t, paths)? else { return Ok(None) };
                let end = b.contents().key.k.p;
                Ok(Some((end, paths.lend(|| f(b))?.into_step())))
            })?;

            // peek_node() leaves iter->pos at the node's min_key (so a restart
            // re-finds the node across splits/merges), so we can't use
            // bch2_btree_iter_advance(); advance explicitly off the node's
            // max_key. set_pos + bpos_successor re-traverses from the root, so
            // the journal overlay applies and journal-only nodes aren't skipped
            // the way next_node()'s sibling walk did. Matches for_each_btree_node().
            match step {
                None => return Ok(Default::default()),
                Some((_, Step::Break(v))) => return Ok(v),
                Some((_, Step::From(pos))) => self.raw.set_pos_in(&mut paths, pos),
                Some((end, Step::Next)) => {
                    if end == SPOS_MAX {
                        return Ok(Default::default());
                    }
                    self.raw.set_pos_in(&mut paths, end.successor());
                }
            }
        }
    }
}

/// A btree's keys, level by level: at each of @levels, the keys from @start
/// to @end. Above the leaves those are the pointers to the nodes a level
/// below - and the root's comes first, if it's one of them. The root's
/// pointer is in btree_root, not a node, so a walk of the level's keys alone
/// misses it; taken first, a root that splits after is seen as its new root's
/// keys. The root's is taken whatever @start and @end. Level 0 is the
/// leaves', with no root key.
pub struct BtreeLevels {
    pub btree:  Btree,
    pub levels: core::ops::Range<u32>,
    pub start:  Bpos,
    pub end:    Bpos,
}

impl BtreeLevels {
    /// for_each_commit() above the leaves, where every key is a btree node
    /// pointer: @f gets it as one.
    pub fn for_each_btree_ptr_commit<'t, F>(
        &self,
        trans:    &'t BtreeTrans<'t>,
        disk_res: Option<&DiskReservation<'_>>,
        flags:    impl Into<CommitOpts>,
        mut f:    F,
    ) -> Result<(), BchError>
    where
        F: for<'a, 'k> FnMut(&'k TransAttempt<'a, 't>, &'k BtreeIter<'t>, u32, BkeySC<'k, BtreePtrs>)
            -> Result<(), BchError>,
    {
        assert!(self.levels.start >= 1, "btree node pointers start at level 1, not {}", self.levels.start);
        self.for_each_commit(trans, disk_res, flags, |t, iter, level, k| {
            f(t, iter, level, k.arm::<BtreePtrs>().expect("a key above the leaves, a btree node pointer"))
        })
    }

    /// Each key in an attempt of its own, committed after @f: as
    /// for_each_max_commit(), @f told the level.
    pub fn for_each_commit<'t, F>(
        &self,
        trans:    &'t BtreeTrans<'t>,
        disk_res: Option<&DiskReservation<'_>>,
        flags:    impl Into<CommitOpts>,
        f:        F,
    ) -> Result<(), BchError>
    where
        F: for<'a, 'k> FnMut(&'k TransAttempt<'a, 't>, &'k BtreeIter<'t>, u32, BkeySC<'k>)
            -> Result<(), BchError>,
    {
        self.walk(trans, Some((disk_res, flags.into())), f)
    }

    /// for_each_commit() without the commit, @f committing for itself if
    /// anything: for a body that leaves the transaction unlocked - a move's.
    pub fn for_each<'t, F>(&self, trans: &'t BtreeTrans<'t>, f: F) -> Result<(), BchError>
    where
        F: for<'a, 'k> FnMut(&'k TransAttempt<'a, 't>, &'k BtreeIter<'t>, u32, BkeySC<'k>)
            -> Result<(), BchError>,
    {
        self.walk(trans, None, f)
    }

    fn walk<'t, F>(&self, trans: &'t BtreeTrans<'t>, commit: Option<(Option<&DiskReservation<'_>>, CommitOpts)>,
                   mut f: F) -> Result<(), BchError>
    where
        F: for<'a, 'k> FnMut(&'k TransAttempt<'a, 't>, &'k BtreeIter<'t>, u32, BkeySC<'k>)
            -> Result<(), BchError>,
    {
        let iter_flags = BtreeIterFlags::PREFETCH | BtreeIterFlags::NOT_EXTENTS | BtreeIterFlags::ALL_SNAPSHOTS;

        for level in self.levels.clone() {
            lockrestart_do(trans, |t| {
                let mut iter = BtreeIter::uninit();
                let Some((iter, k)) = iter.peek_root(t, self.btree, level)? else { return Ok(()) };
                f(t, iter, level, k)?;
                commit.map_or(Ok(()), |(disk_res, flags)| t.commit(disk_res, flags))
            })?;

            let mut node_iter = BtreeNodeIter::new(trans, self.btree, self.start, 0, level, iter_flags);
            let iter = node_iter.iter();
            match commit {
                Some((disk_res, flags)) =>
                    iter.for_each_max_commit(trans, self.end, iter_flags, disk_res, flags,
                                             |t, iter, k| f(t, iter, level, k))?,
                None => iter.for_each_max(trans, self.end, |t, iter, k| f(t, iter, level, k))?,
            }
        }
        Ok(())
    }
}

