use core::time::Duration;
use crate::c;
use crate::fs::Fs;
use crate::os::kernel::ktime_get_ns;
use crate::util::alloc::{Arc, Vec};
use crate::util::prompt::{Question, Reply, UNANSWERED};
use crate::util::thread_with_file::StdioRedirect;
use crate::util::Printbuf;

/// A question for whoever's at the redirect.
pub struct Prompt(Arc<StdioRedirect>);

/// What to do if the answer's slow coming: @then, once, after @after.
pub struct Slow<'a> {
    pub after: Duration,
    pub then:  &'a mut dyn FnMut(),
}

impl Prompt {
    /// None: there's nobody to ask.
    pub fn for_fs(fs: &Fs) -> Option<Self> {
        fs.stdio.get(true).map(Prompt)
    }

    pub fn put<A: Copy + PartialEq>(&self, q: &Question<'_, A>, slow: Option<Slow<'_>>) -> Reply<A> {
        let mut rec = Printbuf::new();
        q.record(&mut rec);

        let mut slow = slow.map(|s| (ktime_get_ns().saturating_add(s.after.as_nanos() as u64), s.then));
        let mut line = Vec::new();

        loop {
            if self.0.write(rec.as_bytes()).is_err() {
                return Reply::Unanswered;
            }

            loop {
                let timeout = slow.as_ref().map(|(at, _)| Duration::from_nanos(at.saturating_sub(ktime_get_ns())));
                match self.0.readline(&mut line, timeout) {
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
