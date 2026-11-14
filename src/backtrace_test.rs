//! The userspace backtrace's capture and symbols (os/linux_user/stacktrace.rs):
//! a stack, the calling thread's and another thread's - unwound in that thread,
//! by signal - names the function it was taken in. A symbolizer that breaks
//! prints addresses, or the wrong names, and nothing else notices.

use std::sync::mpsc;
use std::thread;

use bcachefs_kernel::os::sched;
use bcachefs_kernel::util::backtrace::{prt_current_backtrace, prt_task_backtrace};
use bcachefs_kernel::util::Printbuf;

#[test]
#[inline(never)]
fn backtrace_current() {
    let mut out = Printbuf::new();
    prt_current_backtrace(&mut out, 0).expect("a backtrace");
    assert!(out.as_str().contains("backtrace_current"), "no backtrace_current in:\n{}", out.as_str());
}

#[inline(never)]
fn parked(tx: mpsc::Sender<usize>) {
    tx.send(sched::current() as usize).unwrap();
    thread::park();
}

#[test]
fn backtrace_other_thread() {
    let (tx, rx) = mpsc::channel();
    let t = thread::spawn(move || parked(tx));
    let task = rx.recv().unwrap() as *mut _;

    let mut out = Printbuf::new();
    prt_task_backtrace(&mut out, task, 0).expect("a backtrace");

    t.thread().unpark();
    t.join().unwrap();

    assert!(out.as_str().contains("backtrace_test::parked"), "no backtrace_test::parked in:\n{}", out.as_str());
}
