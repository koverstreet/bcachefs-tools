// SPDX-License-Identifier: GPL-2.0

//! Errors: inconsistencies fsck finds or the running filesystem trips over,
//! and devices' IO errors.
//!
//! An fsck error's verdict - fix it, ignore it, ask, or give up - is an
//! errcode: fsck_fix, fsck_ignore, or a real error (fsck_errors_not_fixed,
//! fsck_repair_unimplemented, ...). Never 0. C's macros turn that into a bool
//! and a `goto fsck_err`; here it's Result<bool>, and the `?` is the goto:
//!
//! ```ignore
//! if fsck_err_on!(trans, nlink != count, id::inode_wrong_nlink,
//!                 "inode has wrong i_nlink ({}, should be {})", nlink, count)? {
//!     repair
//! }
//! ```
//!
//! The work is in plain functions, the message a &dyn ToText - not written
//! when there's no error to report; the macros of the same names make the
//! message with message!() - a format string, or a ToText - so call sites
//! read as C's do.
//!
//! In order: the fsck error API; the engine reaching a verdict - counted,
//! ratelimited, decided by the options or asked - under the errors lock;
//! the paths fsck damaged, for its summary; inode-and-offset messages;
//! filesystem errors, acted on as the errors option says; IO errors; the
//! macros. error.c keeps only the printf-style entry points, which format
//! the message for the *_str exports here.

#![allow(non_camel_case_types)]

use crate::init::passes::RecoveryPass;
use crate::fs::Dev;
use crate::btree::bkey::{pos, BkeySC, POS_MIN, ValidateFlags};
use crate::btree::trans::TransGuard;
use crate::btree::iter::{is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, TransAttempt};use crate::c;
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::prt;
use crate::sb::errors::{FsckFlags, SbErrorId};
use crate::util::alloc::KVVec;
use crate::util::kernel::{jiffies, time_after, HZ};
use crate::util::log::{log_msg_start, print_str, LogLevel, LogMsg};
use crate::util::printbuf::Msg;
use crate::util::prompt::{Choice, Prompt, Question, Reply};
use crate::util::workqueue::Queue;
use crate::util::{Printbuf, ToText};
use core::ffi::{c_ulong, CStr};
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{CStruct, Init, TypeInfo};
use crate::locking::{Locked, MemallocFlags};
use crate::new_lock;
use pin_init::pin_data;
use crate::format::{Bpos, Btree, MemberState};

/*
 * A path that fsck damaged or couldn't fully recover, remembered so we can
 * print a short per-path summary at the end instead of burying it in the error
 * firehose. Keyed by (inum, snapshot); reasons is a bitmask of enum
 * bch_fsck_damage_type - deduped and OR'd on insert, so one line per path lists
 * everything that happened to it, and re-recording on transaction restart is a
 * no-op.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct fsck_damaged_path {
    pub inum: u64,
    pub snapshot: u32,
    /* SbErrorId, deduped; 0 terminated (error 0 is fs-level) */
    pub nr_errors: u16,
    pub errors: [u16; 7],
}

/// One kind of fsck error's state this mount: how often it's been seen, the
/// last message - a restart reports it again, and replays the verdict - and
/// a Y or N for all of its kind: init/error.rs.
pub(crate) struct FsckErrState {
    pub(crate) id:          SbErrorId,
    pub(crate) nr:          u64,
    /// Past FSCK_ERR_RATELIMIT_NR, the rest unprinted
    pub(crate) ratelimited: bool,
    /// The verdict, once there's been one
    pub(crate) ret:         Option<BchError>,
    /// FSCK_FIX_yes or FSCK_FIX_no for all of them: a Y or N answer
    pub(crate) fix:         Option<c::FsckErrOpt>,
    pub(crate) last_msg:    Option<KVVec<u8>>,
}

impl FsckErrState {
    pub(crate) fn new(id: SbErrorId) -> Self {
        FsckErrState { id, nr: 0, ratelimited: false, ret: None, fix: None, last_msg: None }
    }
}

/// The error paths' state, under the errors lock: each fsck error's, and the
/// paths fsck damaged, for its summary.
pub(crate) struct ErrorsLocked {
    pub(crate) msgs:                    KVVec<FsckErrState>,
    pub(crate) msgs_alloc_err:          bool,
    pub(crate) damaged_paths:           KVVec<c::fsck_damaged_path>,
    pub(crate) damaged_paths_alloc_err: bool,
}

impl ErrorsLocked {
    fn new() -> Self {
        ErrorsLocked {
            msgs:                    KVVec::new(),
            msgs_alloc_err:          false,
            damaged_paths:           KVVec::new(),
            damaged_paths_alloc_err: false,
        }
    }
}

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_errors {
    /* What the error paths keep: init/error.rs */
    #[pin]
    #[init(new_lock!(ErrorsLocked::new()))]
    pub(crate) state: Locked<c::mutex, ErrorsLocked>,

    /* The superblock's error counts, by id: sb/errors.rs */
    #[pin]
    #[init(new_lock!(KVVec::new()))]
    pub(crate) counts: Locked<c::mutex, KVVec<c::bch_sb_error_entry_cpu>>,
}

/// The fsck error ids by their names: id::inode_wrong_nlink.
pub type id = SbErrorId;

/// What an fsck error is reported against. A transaction is passed down so
/// __bch2_fsck_err() can drop its locks while it waits on a question; an error
/// reported against just the filesystem must not have one held.
pub trait FsckErrCtx {
    fn fsck_err_ctx(&self) -> (&Fs, Option<&TransAttempt<'_, '_>>);
}

impl FsckErrCtx for Fs {
    fn fsck_err_ctx(&self) -> (&Fs, Option<&TransAttempt<'_, '_>>) {
        crate::warn_on!(self.current_has_btree_trans());
        (self, None)
    }
}

impl FsckErrCtx for TransAttempt<'_, '_> {
    fn fsck_err_ctx(&self) -> (&Fs, Option<&TransAttempt<'_, '_>>) {
        (self.fs(), Some(self))
    }
}

fn __fsck_err(
    ctx:   &impl FsckErrCtx,
    pos:   Bpos,
    flags: FsckFlags,
    err:   SbErrorId,
    msg:   &dyn ToText,
) -> Result<bool, BchError> {
    let (fs, trans) = ctx.fsck_err_ctx();
    let e = fsck_err_verdict(fs, trans, pos, flags, err, msg);

    if e.matches(Errcode::BCH_ERR_fsck_fix) {
        Ok(true)
    } else if e.matches(Errcode::BCH_ERR_fsck_ignore) {
        Ok(false)
    } else {
        Err(e)
    }
}

/// Fixable or ignorable: Ok(true) to fix, Ok(false) to leave it, Err to stop.
pub fn fsck_err(
    ctx: &impl FsckErrCtx,
    err: SbErrorId,
    msg: &dyn ToText,
) -> Result<bool, BchError> {
    __fsck_err(ctx, POS_MIN,
               FsckFlags::CAN_FIX | FsckFlags::CAN_IGNORE, err, msg)
}

/// Fixable, not ignorable.
pub fn mustfix_fsck_err(
    ctx: &impl FsckErrCtx,
    err: SbErrorId,
    msg: &dyn ToText,
) -> Result<bool, BchError> {
    __fsck_err(ctx, POS_MIN, FsckFlags::CAN_FIX, err, msg)
}

/// Against an inode: @pos, its (0, inum, snapshot), is where __bch2_fsck_err()
/// records the damage - which takes a transaction.
pub fn inode_fsck_err(
    t:   &TransAttempt<'_, '_>,
    pos: Bpos,
    err: SbErrorId,
    msg: &dyn ToText,
) -> Result<bool, BchError> {
    __fsck_err(t, pos,
               FsckFlags::CAN_FIX | FsckFlags::CAN_IGNORE, err, msg)
}

/// For a caller that decides whether the error can be fixed. @pos is POS_MIN
/// for none.
pub fn fsck_err_flags(
    ctx:   &impl FsckErrCtx,
    pos:   Bpos,
    flags: FsckFlags,
    err:   SbErrorId,
    msg:   &dyn ToText,
) -> Result<bool, BchError> {
    __fsck_err(ctx, pos, flags, err, msg)
}

/// Logged and counted, ignorable, with no repair offered - the caller
/// repairs, or carries on, whatever's decided: C's log_fsck_err(). Its
/// error, if fsck stops there.
pub fn log_fsck_err(ctx: &impl FsckErrCtx, err: SbErrorId, msg: &dyn ToText) -> Result<(), BchError> {
    __fsck_err(ctx, POS_MIN, FsckFlags::CAN_IGNORE, err, msg).map(|_| ())
}

/// Without offering a repair, for damage another pass owns: counted in the
/// superblock, and logged unless it's a repeat or being ratelimited.
pub fn fsck_err_report(fs: &Fs, err: SbErrorId, msg: &dyn ToText) {
    let mut buf = LogMsg::new(fs);
    msg.to_text(&mut buf);

    let log = count_fsck_err(fs, err, &mut buf);
    buf.set_suppressed(!log);
}

/// Whether to log @msg: not if it's a repeat or being ratelimited. If this is
/// where ratelimiting starts, @msg says so.
pub fn count_fsck_err(fs: &Fs, err: SbErrorId, msg: &mut Printbuf) -> bool {
    let c = count_locked(fs, &mut fs.errors.state.lock(), err, msg.as_bytes()).1;
    if c.suppress {
        write!(msg, "Ratelimiting new instances of previous error\n");
    }
    c.print && !c.repeat
}

pub fn sb_error_count(fs: &Fs, err: SbErrorId) {
    fs.sb_error_count(err)
}

/// fs_inconsistent(), with what the transaction has queued in the message.
pub fn trans_inconsistent(t: &TransAttempt<'_, '_>, msg: &dyn ToText) -> bool {
    fs_trans_inconsistent(t.fs(), Some(t), msg)
}

/// Acts as the errors option says - continue, go emergency read-only, or
/// panic - and returns whether we're going read-only. From atomic context
/// too, as C's.
pub fn fs_inconsistent(fs: &Fs, msg: &dyn ToText) -> bool {
    fs_trans_inconsistent(fs, None, msg)
}

/// Go emergency read-only, logging @msg as a fatal error from @from - if this
/// is what took us read-only.
pub fn fs_fatal_error(fs: &Fs, from: &CStr, msg: &dyn ToText) {
    let mut m = LogMsg::new(fs);
    m.set_suppressed(true);

    prt!(&mut m, "{}: fatal error ", from.to_str().unwrap_or("?"));
    msg.to_text(&mut m);
    m.newline();

    fs.emergency_read_only(&mut m);
    prt!(&mut m, "fatal error - emergency read only");
}

/// What C's bkey_fsck_err() finds by name in a key type's validate: c, k,
/// from.
pub struct BkeyValidate<'a, 'k> {
    pub fs:   &'a Fs,
    pub k:    BkeySC<'k>,
    pub from: &'a c::bkey_validate_context,
}

/// The error validate returns: fsck_delete_bkey if the key is to be dropped -
/// fsck said fix or ignore, or the error is silenced - else what stops us.
pub fn bkey_fsck_err(
    v:   &BkeyValidate<'_, '_>,
    err: SbErrorId,
    msg: &dyn ToText,
) -> BchError {
    let e = bkey_fsck_err_verdict(v.fs, v.k, v.from, err, msg);

    if e.matches(Errcode::BCH_ERR_fsck_fix) || e.matches(Errcode::BCH_ERR_fsck_ignore) {
        v.fs.err(Errcode::BCH_ERR_fsck_delete_bkey)
    } else {
        e
    }
}

// ── The fsck error engine ────────────────────────────────────────────────
//
// Each kind of fsck error has its state this mount, under the errors lock:
// how often it's been seen, the last message, the verdict reached on it, and
// a Y or N for all of its kind. A transaction restart reports an error again,
// with the same message: that's a repeat, which replays the verdict rather
// than ask twice. Past FSCK_ERR_RATELIMIT_NR of a kind, with ratelimit_errors,
// the rest aren't printed; the flush at the end of fsck says how many there
// were. The lock is taken under btree locks - nothing takes them under it but
// a relock after asking, which doesn't block - and is held across asking: one
// question at a time.

const FSCK_ERR_RATELIMIT_NR: u64 = 10;

/// What an error's declaration adds to its flags: AUTOFIX, for the ones safe to
/// repair without asking.
fn flags_extra(err: SbErrorId) -> FsckFlags {
    crate::warn_on!(err.name().is_none());
    err.flags()
}

/// Whether the superblock says to keep quiet about @err.
fn error_silent(fs: &Fs, err: SbErrorId) -> bool {
    let (bits, id) = (c_ulong::BITS as usize, err.0 as usize);
    fs.sb.errors_silent.get(id / bits).is_some_and(|w| w.get() & (1 << (id % bits)) != 0)
}

#[derive(Default)]
struct Counted {
    /// The last message again: a restart's
    repeat:   bool,
    print:    bool,
    /// Where the ratelimiting starts: to say so
    suppress: bool,
}

/// @id's state - None if there's no memory to keep it - counted, with how to
/// report it.
fn count_locked<'a>(fs: &Fs, st: &'a mut ErrorsLocked, id: SbErrorId, msg: &[u8])
    -> (Option<&'a mut FsckErrState>, Counted)
{
    fs.sb_error_count(id);

    let mut c = Counted { print: true, ..Default::default() };
    let i = match st.msgs.iter().position(|s| s.id == id) {
        Some(i) => i,
        None if st.msgs.push(FsckErrState::new(id)).is_ok() => st.msgs.len() - 1,
        None => {
            if !st.msgs_alloc_err {
                crate::bch_err!(fs, "kmalloc err, cannot ratelimit fsck errs");
            }
            st.msgs_alloc_err = true;
            return (None, c);
        }
    };
    let s = &mut st.msgs[i];

    if s.last_msg.as_deref() == Some(msg) {
        c.repeat = true;
        c.print = false;
        return (Some(s), c);
    }

    let mut last = KVVec::new();
    s.last_msg = last.extend_from_slice(msg).is_ok().then_some(last);

    if fs.opts.ratelimit_errors.get() != 0 && s.nr >= FSCK_ERR_RATELIMIT_NR {
        if s.nr == FSCK_ERR_RATELIMIT_NR {
            c.suppress = true;
            s.ratelimited = true;
        } else {
            c.print = false;
        }
    }
    s.nr += 1;
    (Some(s), c)
}

#[derive(Clone, Copy, PartialEq)]
enum Answer {
    No,
    Yes,
    AllNo,
    AllYes,
}

const CHOICES: &[Choice<Answer>] = &[
    Choice { key: 'y', aliases: &[], short: "",                  blurb: "", answer: Answer::Yes },
    Choice { key: 'n', aliases: &[], short: "",                  blurb: "", answer: Answer::No },
    Choice { key: 'Y', aliases: &[], short: "all of this type",  blurb: "", answer: Answer::AllYes },
    Choice { key: 'N', aliases: &[], short: "none of this type", blurb: "", answer: Answer::AllNo },
];

/// The answer from whoever's running fsck, through its stdio redirect - No
/// without one: with @trans unlocked, and after two seconds, unlocked long.
#[cfg(kernel)]
fn ask_answer(fs: &Fs, trans: Option<&TransAttempt<'_, '_>>, q: &Question<'_, Answer>) -> Answer {
    use crate::util::prompt::Slow;

    let Some(p) = Prompt::for_fs(fs) else {
        crate::util::log::print_user(fs, q.prompt.as_bytes());
        return Answer::No;
    };

    if let Some(t) = trans {
        t.unlock();
    }
    let mut unlock_long = trans.map(|t| move || t.unlock_long());
    let slow = unlock_long.as_mut()
        .map(|then| Slow { after: core::time::Duration::from_secs(2), then });

    match p.put(q, slow) {
        Reply::Answer(a) => a,
        Reply::Settled | Reply::Unanswered => Answer::No,
    }
}

/// The answer on stdin, or through an agent; its end is fatal.
#[cfg(not(kernel))]
fn ask_answer(fs: &Fs, _: Option<&TransAttempt<'_, '_>>, q: &Question<'_, Answer>) -> Answer {
    let reply = match Prompt::detect() {
        Some(p) => p.put(q, None).ok(),
        None => {
            crate::util::log::print_user(fs, q.prompt.as_bytes());
            None
        }
    };
    match reply {
        Some(Reply::Answer(a)) => a,
        _ => {
            std::eprintln!("error reading from standard input");
            std::process::exit(1);
        }
    }
}

/// @question asked, ending ", @action": the answer, with @trans relocked - a
/// restart if it can't be.
fn ask(fs: &Fs, trans: Option<&TransAttempt<'_, '_>>, question: &mut Printbuf, action: &[u8])
    -> Result<Answer, BchError>
{
    write!(question, ", ");
    question.write_bytes(action);

    let mut uuid = Printbuf::new();
    prt!(&mut uuid, "{}", fs.sb.user_uuid());
    let q = Question {
        prompt:  question.as_str(),
        choices: CHOICES,
        silence: None,
        alarm:   false,
        uuid:    uuid.as_str(),
        timeout: None,
    };
    let a = ask_answer(fs, trans, &q);
    if let Some(t) = trans {
        t.relock()?;
    }
    Ok(a)
}

/// "fix?" as "fixing", "recreate?" as "recreating".
fn prt_actioning(out: &mut Printbuf, action: &[u8]) {
    let action = action.strip_suffix(b"?").expect("an fsck action ends in '?'");
    out.write_bytes(action.strip_suffix(b"e").unwrap_or(action));
    write!(out, "ing");
}

fn error_action_name(a: c::ErrorAction) -> &'static str {
    a.name().unwrap_or("?")
}

/// The verdict on an fsck error, reported as the options say, or asked. An
/// error at @pos, not POS_MIN, records damage against that inode first, which
/// takes @trans; with @trans, a fix is logged in the journal with the repair,
/// and asking drops its locks.
#[cold]
pub fn fsck_err_verdict(fs: &Fs, trans: Option<&TransAttempt<'_, '_>>, pos: Bpos, mut flags: FsckFlags,
                        err: SbErrorId, msg: &dyn ToText) -> BchError {
    use Errcode as e;
    use c::ErrorAction as a;
    use c::FsFlag as fl;

    flags |= flags_extra(err);

    // the error is the damage, whatever's decided: recorded first
    if pos != POS_MIN && !crate::warn_on!(trans.is_none()) {
        if let Err(r) = crate::init::damage::record(trans.expect("checked"), pos, err) {
            return r;
        }
    }

    // Asking needs the transaction, to unlock it: bkey errors, which have
    // none, are all AUTOFIX, so there's no asking without one
    crate::warn_on!(flags.contains(FsckFlags::CAN_FIX) && !flags.contains(FsckFlags::AUTOFIX) && trans.is_none() &&
                    fs.current_has_btree_trans());

    if flags.contains(FsckFlags::ERR_SILENT) || error_silent(fs, err) {
        fs.flags.set(fl::errors_fixed_silent, true);
        return fs.err(if flags.contains(FsckFlags::CAN_FIX) { e::BCH_ERR_fsck_fix } else { e::BCH_ERR_fsck_ignore });
    }

    let mut buf = Printbuf::new();
    msg.to_text(&mut buf);
    let body = buf.as_bytes();

    // a custom action - "..., recreate?" - in place of "fix?"
    let (text, action): (&[u8], &[u8]) = match body.iter().rposition(|&b| b == b',') {
        Some(p) if body.last() == Some(&b'?') => (&body[..p], body.get(p + 2..).unwrap_or_default()),
        _ => (body, b"fix?"),
    };

    let mut out = Printbuf::new();
    out.indent_add_nextline(2);
    #[cfg(kernel)]
    if !body.starts_with(b"bcachefs") {
        write!(out, "bcachefs ({}): ", fs.name().to_str().unwrap_or("?"));
    }
    out.write_bytes_indented(text);

    let fix_errors = c::FsckErrOpt::from_bits(fs.opts.fix_errors.get().into());
    let errors = fs.opts.errors();
    let (mut inconsistent, mut exiting) = (false, false);

    let mut st = fs.errors.state.lock();
    let (mut s, mut cnt) = count_locked(fs, &mut st, err, out.as_bytes());

    // C's goto err_unlock is 'unlock: no report, nothing remembered; its goto
    // print, 'print
    let ret: BchError = 'unlock: {
        // a restart's repeat: the verdict reached the first time, if one was
        if cnt.repeat {
            if let Some(r) = s.as_ref().and_then(|s| s.ret) {
                break 'unlock r;
            }
        }

        let ret = 'print: {
            let ret = if flags.contains(FsckFlags::AUTOFIX) &&
                         (errors == a::BCH_ON_ERROR_continue || errors == a::fix_safe) {
                break 'print if flags.contains(FsckFlags::CAN_FIX) {
                    write!(out, ", ");
                    prt_actioning(&mut out, action);
                    fs.err(e::BCH_ERR_fsck_fix)
                } else {
                    write!(out, ", continuing");
                    fs.err(e::BCH_ERR_fsck_ignore)
                };
            } else if !fs.flags.get(fl::in_fsck) {
                if errors != a::BCH_ON_ERROR_continue || !flags.intersects(FsckFlags::CAN_FIX | FsckFlags::CAN_IGNORE) {
                    if flags.contains(FsckFlags::AUTOFIX) {
                        write!(out, ", shutting down\n\
                                     error is autofix, but errors={} prevents self-healing\n\
                                     run fsck or set errors=fix_safe to allow self-healing",
                               error_action_name(errors));
                    } else if flags.intersects(FsckFlags::CAN_FIX | FsckFlags::CAN_IGNORE) {
                        write!(out, ", shutting down\n\
                                     error is fixable, but not marked safe for self-healing\n\
                                     run fsck");
                    } else {
                        write!(out, ", shutting down\n\
                                     error not marked as autofix and not in fsck\n\
                                     run fsck, and forward to devs so error can be marked for self-healing");
                    }
                    inconsistent = true;
                    cnt.print = true;
                    fs.err(e::BCH_ERR_fsck_errors_not_fixed)
                } else if flags.contains(FsckFlags::CAN_FIX) {
                    write!(out, ", ");
                    prt_actioning(&mut out, action);
                    fs.err(e::BCH_ERR_fsck_fix)
                } else {
                    write!(out, ", continuing");
                    fs.err(e::BCH_ERR_fsck_ignore)
                }
            } else if fix_errors == c::FsckErrOpt::exit {
                write!(out, ", exiting");
                fs.err(e::BCH_ERR_fsck_errors_not_fixed)
            } else if flags.contains(FsckFlags::CAN_FIX) {
                let fix = s.as_ref().and_then(|s| s.fix).unwrap_or(fix_errors);

                if fix == c::FsckErrOpt::ask {
                    cnt.print = false;

                    let answer = match ask(fs, trans, &mut out, action) {
                        Ok(ans) => ans,
                        // relocking after the question restarted: undecided
                        Err(r) => break 'unlock r,
                    };
                    if let Some(s) = s.as_deref_mut() {
                        match answer {
                            Answer::AllNo  => s.fix = Some(c::FsckErrOpt::no),
                            Answer::AllYes => s.fix = Some(c::FsckErrOpt::yes),
                            _ => {}
                        }
                    }
                    fs.err(if matches!(answer, Answer::Yes | Answer::AllYes) {
                        e::BCH_ERR_fsck_fix
                    } else {
                        e::BCH_ERR_fsck_ignore
                    })
                } else if fix == c::FsckErrOpt::yes ||
                          (fs.opts.nochanges.get() != 0 && !flags.contains(FsckFlags::CAN_IGNORE)) {
                    write!(out, ", ");
                    prt_actioning(&mut out, action);
                    fs.err(e::BCH_ERR_fsck_fix)
                } else {
                    write!(out, ", not ");
                    prt_actioning(&mut out, action);
                    fs.err(e::BCH_ERR_fsck_ignore)
                }
            } else if flags.contains(FsckFlags::CAN_IGNORE) {
                write!(out, ", continuing");
                fs.err(e::BCH_ERR_fsck_ignore)
            } else {
                write!(out, " (repair unimplemented)");
                fs.err(e::BCH_ERR_fsck_repair_unimplemented)
            };

            let ret = if ret.matches(e::BCH_ERR_fsck_ignore) &&
                         (fix_errors == c::FsckErrOpt::exit || !flags.contains(FsckFlags::CAN_IGNORE)) {
                fs.err(e::BCH_ERR_fsck_errors_not_fixed)
            } else {
                ret
            };

            if fs.flags.get(fl::in_fsck) &&
               !ret.matches(e::BCH_ERR_fsck_fix) && !ret.matches(e::BCH_ERR_fsck_ignore) {
                exiting = true;
                cnt.print = true;
            }
            ret
        };

        report(fs, &mut out, s.as_deref_mut(), cnt, inconsistent, exiting, ret)
    };

    finish(fs, trans, flags, err, st, ret)
}

/// The verdict's report: printed if it's to be, and remembered.
fn report(fs: &Fs, out: &mut Printbuf, s: Option<&mut FsckErrState>, cnt: Counted, inconsistent: bool,
          exiting: bool, ret: BchError) -> BchError {
    out.newline();

    if inconsistent {
        fs.inconsistent_error(out);
    } else if exiting {
        write!(out, "Unable to continue, halting\n");
    } else if cnt.suppress {
        write!(out, "Ratelimiting new instances of previous error\n");
    }

    if cnt.print {
        // possibly an empty line, from the indent
        let len = out.as_bytes().len() - out.as_bytes().iter().rev().take_while(|&&b| b == b' ').count();
        out.truncate(len);
        crate::util::log::print_user(fs, out.as_bytes());
    }

    if let Some(s) = s {
        s.ret = Some(ret);
    }
    ret
}

/// After the lock: a fix logged in the journal with its repair, and the
/// filesystem's error flags.
fn finish(fs: &Fs, trans: Option<&TransAttempt<'_, '_>>, flags: FsckFlags, err: SbErrorId,
          st: impl Sized, mut ret: BchError) -> BchError {
    use Errcode as e;
    use c::FsFlag as fl;

    drop(st);

    if let Some(t) = trans {
        if !flags.contains(FsckFlags::ERR_NO_LOG) && ret == BchError::from(e::BCH_ERR_fsck_fix) {
            let mut name = Printbuf::new();
            err.to_text(&mut name);
            if let Err(r) = t.log_str(name.as_bytes()) {
                ret = r;
            }
        }
    }

    // Whether the filesystem has errors still isn't tracked, for
    // log_fsck_err()s: that'd take knowing which pass corrects each
    if ret.matches(e::BCH_ERR_transaction_restart) {
    } else if ret.matches(e::BCH_ERR_fsck_fix) {
        fs.flags.set(fl::errors_fixed, true);
    } else {
        fs.flags.set(fl::errors_not_fixed, true);
        fs.flags.set(fl::error, true);
    }
    ret
}

/// The verdict on an invalid key - fsck_delete_bkey, at once, for a check
/// that's silent, or an error silenced; else fsck_err_verdict()'s, offering
/// to delete it - fixed without asking, but where it's a write's or a commit's.
pub fn bkey_fsck_err_verdict(fs: &Fs, k: BkeySC<'_>, from: &c::bkey_validate_context, err: SbErrorId,
                             msg: &dyn ToText) -> BchError {
    let vflags = from.bits.flags();
    if vflags.contains(ValidateFlags::silent) {
        return fs.err(Errcode::BCH_ERR_fsck_delete_bkey);
    }

    let mut flags = FsckFlags::empty();
    if !vflags.intersects(ValidateFlags::write | ValidateFlags::commit) {
        if error_silent(fs, err) {
            return fs.err(Errcode::BCH_ERR_fsck_delete_bkey);
        }
        flags |= FsckFlags::AUTOFIX | FsckFlags::CAN_FIX;
    }

    let from_ty = from.bits.from();
    let msg = Msg(|out: &mut Printbuf| {
        write!(out, "invalid bkey in {}", from_ty.name());
        if from_ty == c::BkeyValidateFrom::journal {
            write!(out, " journal seq={} offset={}", from.journal_seq, from.journal_offset);
        }
        let btree = from.btree;
        write!(out, " btree={btree} level={}: ", from.level);
        crate::btree::bkey::bkey_val_to_text(out, Some(fs), k.into());
        out.newline();
        msg.to_text(out);
        write!(out, ", delete?");
    });
    fsck_err_verdict(fs, None, POS_MIN, flags, err, &msg)
}

// ── For C: the fsck errors ───────────────────────────────────────────────

pub(crate) fn fsck_errs_seen(fs: &Fs) -> bool {
    !fs.errors.state.lock().msgs.is_empty()
}

/// Each kind of error seen, and how often: most often first.
pub(crate) fn fsck_err_counts_to_text(out: &mut Printbuf, fs: &Fs) {
    let mut st = fs.errors.state.lock();
    st.msgs.sort_unstable_by(|l, r| r.nr.cmp(&l.nr));
    for s in st.msgs.iter() {
        s.id.to_text(out);
        write!(out, "\t{}\n", s.nr);
    }
}

/// The errors' states freed - the ratelimited ones' counts printed if
/// @print: at the end of fsck, or with nothing printed, at teardown.
pub(crate) fn flush_fsck_errs(fs: &Fs, print: bool) {
    let msgs = core::mem::take(&mut fs.errors.state.lock().msgs);
    for s in msgs.iter() {
        if let (true, true, Some(m)) = (print, s.ratelimited, &s.last_msg) {
            crate::bch_err!(fs, "Saw {} errors like:\n  {}", s.nr, core::str::from_utf8(m).unwrap_or("?"));
        }
    }
}

pub fn free_fsck_errs(fs: &Fs) {
    flush_fsck_errs(fs, false)
}

// ── Damaged paths ────────────────────────────────────────────────────────
//
// Paths fsck damaged, for the summary at its end, and debugfs: deduped by
// (inum, snapshot), each with the errors that hit it - which makes a
// restart's report again a no-op. Recorded with the damage btree's update,
// by damage::record(); goes when reporting reads the damage btree.

const DAMAGED_PATHS_MAX: usize = 4096;
const DAMAGED_PATHS_PRINT: usize = 200;

impl c::fsck_damaged_path {
    fn add_err(&mut self, err: SbErrorId) {
        let nr = usize::from(self.nr_errors);
        if !self.errors[..nr].contains(&(err.0 as u16)) && nr < self.errors.len() {
            self.errors[nr] = err.0 as u16;
            self.nr_errors += 1;
        }
    }
}

/// @err against the path at @pos - an extent's, inum in its inode field, or
/// an inode key's, at (0, inum, snapshot).
pub fn fsck_damaged(fs: &Fs, pos: Bpos, err: SbErrorId) {
    let inum = if pos.inode != 0 { pos.inode } else { pos.offset };
    let mut st = fs.errors.state.lock();
    let paths = &mut st.damaged_paths;

    // damage to one inode arrives in runs: from the last entry back
    if let Some(i) = paths.iter().rposition(|p| p.inum == inum && p.snapshot == pos.snapshot) {
        paths[i].add_err(err);
        return;
    }

    let mut p = c::fsck_damaged_path { inum, snapshot: pos.snapshot, nr_errors: 1, ..Default::default() };
    p.errors[0] = err.0 as u16;
    if paths.len() >= DAMAGED_PATHS_MAX || paths.push(p).is_err() {
        st.damaged_paths_alloc_err = true;
    }
}

/// @inum's path in @snapshot - which may itself be dead by the report, as
/// the deleted keys' entries are - or else in a snapshot of the inode
/// that's live, which still names the file.
fn damaged_path_resolve(t: &TransAttempt<'_, '_>, inum: u64, snapshot: u32, path: &mut Printbuf)
    -> Result<(), BchError>
{
    path.reset();
    match crate::namei::inum_snapshot_to_path(t, inum, snapshot, path) {
        Err(e) if !is_restart(&e) => {}
        r => return r,
    }

    let mut iter = BtreeIter::new(t, Btree::inodes, pos(0, inum), BtreeIterFlags::ALL_SNAPSHOTS);
    let mut ret = Ok(());
    while let Some(k) = iter.peek(t)? {
        if k.k.p.offset != inum {
            break;
        }
        let snap = k.k.p.snapshot;
        if snap != snapshot && crate::inode::bkey_is_inode(k.k) {
            path.reset();
            match crate::namei::inum_snapshot_to_path(t, inum, snap, path) {
                Err(e) if !is_restart(&e) => ret = Err(e),
                r => return r,
            }
        }
        iter.advance();
    }
    ret.and_then(|()| Err(t.fs().err(Errcode::BCH_ERR_ENOENT_inode)))
}

/// One damaged path, and what happened to it.
pub fn damaged_path_to_text(out: &mut Printbuf, trans: &TransGuard<'_>, p: &c::fsck_damaged_path) {
    // into a buffer of its own, so a restart doesn't print it twice
    let mut path = Printbuf::new();
    match lockrestart_do(trans, |t| damaged_path_resolve(t, p.inum, p.snapshot, &mut path)) {
        Ok(()) => out.write_bytes(path.as_bytes()),
        Err(_) => write!(out, "inum {}:{}", p.inum, p.snapshot),
    }
    write!(out, ": ");
    for (j, &e) in p.errors[..usize::from(p.nr_errors)].iter().enumerate() {
        if j != 0 {
            write!(out, ", ");
        }
        SbErrorId(e.into()).to_text(out);
    }
    out.newline();
}

/// The paths fsck damaged: the first DAMAGED_PATHS_PRINT, then how many more.
pub(crate) fn fsck_damaged_paths_to_text(out: &mut Printbuf, fs: &Fs) {
    let (nr, alloc_err) = {
        let st = fs.errors.state.lock();
        (st.damaged_paths.len(), st.damaged_paths_alloc_err)
    };
    if nr == 0 {
        return;
    }

    write!(out, "{nr} paths damaged by fsck");
    if alloc_err {
        write!(out, " (list incomplete)");
    }
    write!(out, ":\n");
    out.indent_add(2);

    let trans = crate::btree_trans!(fs);
    for i in 0..nr.min(DAMAGED_PATHS_PRINT) {
        // copied under the lock, resolved outside it: resolving takes btree
        // locks, which the lock is taken under
        let Some(p) = fs.errors.state.lock().damaged_paths.get(i).copied() else { break };
        damaged_path_to_text(out, &trans, &p);
    }
    if nr > DAMAGED_PATHS_PRINT {
        write!(out, "... and {} more (untruncated list in debugfs: fsck_damaged_paths)\n",
               nr - DAMAGED_PATHS_PRINT);
    }

    out.indent_sub(2);
}

// ── Inode and offset, for an error message ───────────────────────────────

/// "path offset N" for @pos in @subvol - "inum N" for an inode with no
/// subvolume or snapshot, "inum subvol:N" if there's no path to it - in the
/// caller's attempt: a restart is the caller's.
pub fn inum_offset_err_msg(t: &TransAttempt<'_, '_>, out: &mut Printbuf, subvol: u32, pos: Bpos)
    -> Result<(), BchError>
{
    let ret = if subvol != 0 {
        crate::namei::inum_to_path(t, c::subvol_inum { subvol: subvol.into(), inum: pos.inode }, out)
    } else if pos.snapshot != 0 {
        crate::namei::inum_snapshot_to_path(t, pos.inode, pos.snapshot, out)
    } else {
        Ok(())
    };
    if ret.as_ref().is_err_and(is_restart) {
        return ret;
    }

    if subvol == 0 && pos.snapshot == 0 {
        write!(out, "inum {}", { pos.inode });
    } else if ret.is_err() {
        write!(out, "inum {subvol}:{}", { pos.inode });
    }
    write!(out, " offset {}", pos.offset << 9);
    Ok(())
}

/// inum_offset_err_msg(), in transactions of its own - so never from inside
/// an attempt: that's the attempt's end. For C's callers.
pub fn bch2_inum_offset_err_msg_trans(trans: &BtreeTrans<'_>, out: &mut Printbuf, subvol: u32, pos: Bpos) {
    let _ = lockrestart_do(trans, |t| inum_offset_err_msg(t, out, subvol, pos));
}

// ── Filesystem errors ────────────────────────────────────────────────────

impl Fs {
    /// An inconsistency, acted on as the errors option says - carry on, go
    /// emergency read-only, or panic - and noted in @out: whether we're going
    /// read-only.
    pub fn inconsistent_error(&self, out: &mut Printbuf) -> bool {
        use c::ErrorAction as a;

        self.flags.set(c::FsFlag::error, true);
        match self.opts.errors() {
            a::BCH_ON_ERROR_continue => false,
            a::fix_safe | a::ro => {
                self.emergency_read_only(out);
                true
            }
            a::panic => {
                print_str(self, LogLevel::ERR, out);
                panic_after_error(self)
            }
            e => panic!("errors option {}", e.0),
        }
    }

    /// A btree topology error, noted in @out - an inconsistency, or in
    /// recovery, check_topology run explicitly: the error to stop on, the
    /// pass's or btree_need_topology_repair.
    pub fn topology_error(&self, out: &mut Printbuf) -> BchError {
        write!(out, "btree topology error: ");

        self.flags.set(c::FsFlag::topology_error, true);
        if !self.flags.get(c::FsFlag::in_recovery) {
            self.inconsistent_error(out);
        } else if let Err(e) = crate::init::passes::run_explicit(self, out,
                RecoveryPass::check_topology,
                c::RunRecoveryPassFlags::from_bits_retain(0)) {
            return e;
        }
        self.err(Errcode::BCH_ERR_btree_need_topology_repair)
    }
}

fn panic_after_error(fs: &Fs) -> ! {
    #[cfg(kernel)]
    {
        let mut msg = Printbuf::new();
        write!(msg, "bcachefs ({}): panic after error\n", fs.name().to_str().unwrap_or("?"));
        // SAFETY: a format, and the string it takes
        unsafe { c::panic(c"%s".as_ptr().cast(), msg.as_c_str().as_ptr()) }
    }
    #[cfg(not(kernel))]
    panic!("{}: panic after error", fs.name().to_str().unwrap_or("?"))
}

/// The inconsistency @msg says - in @t, whose queued updates go with it -
/// acted on, and printed: whether we're going read-only. From atomic context
/// too: in a NORECLAIM scope, the message doesn't sleep to grow.
fn fs_trans_inconsistent(fs: &Fs, t: Option<&TransAttempt<'_, '_>>, msg: &dyn ToText) -> bool {
    let _noreclaim = MemallocFlags::noreclaim();
    let mut out = Printbuf::new();

    log_msg_start(&mut out, fs.name());
    msg.to_text(&mut out);
    out.newline();

    if let Some(t) = t {
        t.updates_to_text(&mut out);
    }
    let ret = fs.inconsistent_error(&mut out);
    print_str(fs, LogLevel::ERR, &mut out);
    ret
}

impl Fs {
    /// An inconsistency, with nothing to say about it.
    pub fn inconsistent(&self) -> bool {
        let _noreclaim = MemallocFlags::noreclaim();
        let mut out = Printbuf::new();
        out.indent_add_nextline(2);

        let ret = self.inconsistent_error(&mut out);
        if ret {
            crate::bch_err!(self, "{}", out.as_str());
        }
        ret
    }
}

// ── IO errors ────────────────────────────────────────────────────────────
//
// A device's IO errors are counted as they happen, and writes failing for
// write_error_timeout set it read-only - the filesystem, if the device can't
// go. That's a work item's, as it takes state_lock, which device removal
// holds while it drains ca->ref: so the work holds a ref_outer - the
// device's memory only - and rechecks under the lock that it's not being
// removed, nor the filesystem stopping.

impl Dev {
    /// An IO error of type @ty: counted, and a write's starting the write
    /// error clock, which io_error_work() reads.
    pub fn io_error(&self, ty: c::MemberErrorType) {
        self.errors[ty.0 as usize].fetch_add(1, Relaxed);

        if ty == c::MemberErrorType::write && self.write_errors_start.load(Relaxed) == 0 {
            self.write_errors_start.store(jiffies() as usize, Relaxed);
        }

        self.io_error_work.queue(Queue::system_long());
    }
}

pub(crate) fn io_error_work(ca: &Dev) {
    let fs = ca.fs();

    // XXX: if it's reads or checksums that are failing, set it to failed
    let start = ca.write_errors_start.load(Relaxed) as c_ulong;
    let timeout = c_ulong::from(fs.opts.write_error_timeout.get());
    if start == 0 || !time_after(jiffies(), start.wrapping_add(timeout * HZ)) {
        return;
    }

    let state = fs.state_lock_write();
    if ca.removing() ||
       fs.flags.get(c::FsFlag::stopping) ||
       ca.mi.get().state >= MemberState::ro {
        return;
    }

    let mut buf = Printbuf::new();
    log_msg_start(&mut buf, ca.name());
    write!(buf, "writes erroring for {timeout} seconds\n");

    let dev = state.dev_set_state(ca, MemberState::ro,
                                  c::BCH_FORCE_IF_DEGRADED, &mut buf).is_ok();
    write!(buf, "setting {} ro", if dev { "device" } else { "filesystem" });
    if dev || fs.emergency_read_only(&mut buf) {
        print_str(fs, LogLevel::ERR, &mut buf);
    }
}

// ── Macros: the functions above, with message!() written for you ──────────
//
// Each reports and repairs through outlined(): the message's arguments and the
// error path's frames out of the caller's - a check, often, whose frame is
// under btree lookups and lock waits.

#[doc(hidden)]
#[cold]
#[inline(never)]
pub fn outlined<R>(f: impl FnOnce() -> R) -> R {
    f()
}

#[macro_export]
macro_rules! bkey_fsck_err {
    ($v:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::outlined(|| $crate::init::error::bkey_fsck_err($v, $err, $crate::message!($($msg)*)))
    };
}

#[macro_export]
macro_rules! bkey_fsck_err_on {
    ($v:expr, $cond:expr, $err:expr, $($msg:tt)*) => {
        if $cond { Err($crate::bkey_fsck_err!($v, $err, $($msg)*)) } else { Ok(()) }
    };
}

#[macro_export]
macro_rules! fsck_err {
    ($ctx:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::outlined(|| $crate::init::error::fsck_err($ctx, $err, $crate::message!($($msg)*)))
    };
}

#[macro_export]
macro_rules! fsck_err_on {
    ($ctx:expr, $cond:expr, $err:expr, $($msg:tt)*) => {
        if $cond { $crate::fsck_err!($ctx, $err, $($msg)*) } else { Ok(false) }
    };
}

#[macro_export]
macro_rules! mustfix_fsck_err {
    ($ctx:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::outlined(|| $crate::init::error::mustfix_fsck_err($ctx, $err, $crate::message!($($msg)*)))
    };
}

#[macro_export]
macro_rules! mustfix_fsck_err_on {
    ($ctx:expr, $cond:expr, $err:expr, $($msg:tt)*) => {
        if $cond { $crate::mustfix_fsck_err!($ctx, $err, $($msg)*) } else { Ok(false) }
    };
}

#[macro_export]
macro_rules! fsck_err_flags {
    ($ctx:expr, $pos:expr, $flags:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::outlined(|| $crate::init::error::fsck_err_flags($ctx, $pos, $flags, $err, $crate::message!($($msg)*)))
    };
}

#[macro_export]
macro_rules! log_fsck_err {
    ($ctx:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::outlined(|| $crate::init::error::log_fsck_err($ctx, $err, $crate::message!($($msg)*)))
    };
}

#[macro_export]
macro_rules! fsck_err_report {
    ($fs:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::outlined(|| $crate::init::error::fsck_err_report($fs, $err, $crate::message!($($msg)*)))
    };
}

#[macro_export]
macro_rules! trans_inconsistent {
    ($trans:expr, $($msg:tt)*) => {
        $crate::init::error::trans_inconsistent($trans, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! fs_inconsistent {
    ($fs:expr, $($msg:tt)*) => {
        $crate::init::error::fs_inconsistent($fs, $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! fs_fatal_error {
    ($fs:expr, $($msg:tt)*) => {
        $crate::init::error::fs_fatal_error($fs, $crate::here!(), $crate::message!($($msg)*))
    };
}

#[macro_export]
macro_rules! inode_fsck_err {
    ($trans:expr, $pos:expr, $err:expr, $($msg:tt)*) => {
        $crate::init::error::outlined(|| $crate::init::error::inode_fsck_err($trans, $pos, $err, $crate::message!($($msg)*)))
    };
}
