use core::time::Duration;

use super::{Question, Reply, UNANSWERED};
use crate::c;
use crate::fs::Fs;
use crate::util::alloc::KVVec;
use crate::os::kernel::{jiffies, HZ};
use crate::os::thread_with_file::StdioRedirect;
use crate::util::Printbuf;
use kernel::ffi::{c_long, c_ulong};

pub struct Prompt<'a>(&'a StdioRedirect);

/// What to do if the answer's slow coming: @then, once, after @after.
pub struct Slow<'a> {
    pub after: Duration,
    pub then:  &'a mut dyn FnMut(),
}

impl<'a> Prompt<'a> {
    /// None: there's nobody to ask.
    pub fn for_fs(fs: &'a Fs) -> Option<Self> {
        crate::util::log::stdio_redirect(fs, true).map(Prompt)
    }

    pub fn put<A: Copy + PartialEq>(&self, q: &Question<'_, A>, slow: Option<Slow<'_>>) -> Reply<A> {
        let mut rec = Printbuf::new();
        q.record(&mut rec);

        let mut slow = slow.map(|s| {
            (jiffies().wrapping_add(s.after.as_millis() as core::ffi::c_ulong * HZ / 1000), s.then)
        });
        let mut line = KVVec::new();

        loop {
            if self.0.write(false, rec.as_bytes()).is_err() {
                return Reply::Unanswered;
            }

            loop {
                let timeout = match &slow {
                    Some((at, _)) => (at.wrapping_sub(jiffies()) as c_long).max(0) as c_ulong,
                    None          => c_long::MAX as c_ulong,
                };
                match self.0.readline_timeout(&mut line, timeout) {
                    Ok(()) => break,
                    Err(e) if e.matches(c::ETIME) => {
                        if let Some((_, then)) = slow.take() {
                            then();
                        }
                    }
                    Err(_) => return Reply::Unanswered,
                }
            }

            let reply = core::str::from_utf8(&line).unwrap_or("").trim_end();
            if reply == UNANSWERED {
                return Reply::Unanswered;
            }
            if let Some(a) = q.parse(reply) {
                return Reply::Answer(a);
            }
        }
    }
}
