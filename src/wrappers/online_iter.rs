//! Iterator over btree keys on a mounted filesystem, via
//! BCH_IOCTL_QUERY_BTREE_KEYS.
//!
//! The ioctl is stateless - the cursor lives in the arg struct and the
//! kernel returns one buffer of densely-packed bkeys (struct bkey_i,
//! unpacked in-memory format) per call. This wrapper hides the
//! buffer/refill/cursor machinery and yields `BkeySC`s, so consumers
//! (`bcachefs list`, kvdb) use the same key types and formatting as the
//! offline `BtreeIter` path.
//!
//! Lending iterator: keys borrow from the internal buffer, which is
//! invalidated on refill - so this can't implement std::iter::Iterator.
//! Use `while let Some(k) = iter.next()?` or `for_each()`.

use std::ops::ControlFlow;

use bcachefs_kernel::c;
use bcachefs_kernel::btree::bkey::BkeySC;
use errno::Errno;

use crate::wrappers::handle::BcachefsHandle;

/// Iteration flags, mirroring BCH_IOCTL_QUERY_BTREE_KEYS_*:
#[derive(Clone, Copy, Default)]
pub struct OnlineIterFlags(pub u32);

impl OnlineIterFlags {
    pub const SLOTS: Self         = Self(c::BCH_IOCTL_QUERY_BTREE_KEYS_slots);
    pub const PREV: Self          = Self(c::BCH_IOCTL_QUERY_BTREE_KEYS_prev);
    pub const ALL_SNAPSHOTS: Self = Self(c::BCH_IOCTL_QUERY_BTREE_KEYS_all_snapshots);
}

impl std::ops::BitOr for OnlineIterFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

pub struct OnlineBtreeIter<'h> {
    fs:   &'h BcachefsHandle,
    arg:  c::bch_ioctl_query_btree_keys,
    buf:  Vec<u8>,
    /// Offset of the next unconsumed key in `buf`; == arg.used when the
    /// buffer is drained.
    pos:  u32,
}

impl<'h> OnlineBtreeIter<'h> {
    pub fn new(fs: &'h BcachefsHandle,
	       btree: c::btree_id,
	       level: u32,
	       start: c::bpos,
	       end: c::bpos,
	       flags: OnlineIterFlags) -> Self {
        Self::with_buf_size(fs, btree, level, start, end, flags, 1 << 20)
    }

    /// The kernel fills the whole buffer per call, so single-key queries
    /// (get/peek) should pass a small size rather than pay for a full
    /// buffer of iteration they'll never consume. Grows automatically if
    /// even one key doesn't fit.
    pub fn with_buf_size(fs: &'h BcachefsHandle,
			 btree: c::btree_id,
			 level: u32,
			 start: c::bpos,
			 end: c::bpos,
			 flags: OnlineIterFlags,
			 buf_size: usize) -> Self {
        OnlineBtreeIter {
            fs,
            arg: c::bch_ioctl_query_btree_keys {
                btree: btree.into(),
                level,
                flags: flags.0,
                start,
                end,
                ..Default::default()
            },
            buf: vec![0; buf_size],
            pos: 0,
        }
    }

    fn refill(&mut self) -> Result<(), Errno> {
        loop {
            self.arg.buf = self.buf.as_mut_ptr() as u64;
            self.arg.buf_size = self.buf.len() as u32;
            self.arg.used = 0;
            self.pos = 0;

            match self.fs.query_btree_keys(&mut self.arg) {
                Ok(()) => return Ok(()),
                // Buffer can't fit even one key - grow and retry:
                Err(e) if e.0 == libc::ERANGE => {
                    let new_len = self.buf.len() * 2;
                    self.buf = vec![0; new_len];
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The next key, or None at the end of the range. Lending: the
    /// returned key borrows the internal buffer and dies at the next
    /// call.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<BkeySC<'_>>, Errno> {
        if self.pos >= self.arg.used {
            if self.arg.done != 0 {
                return Ok(None);
            }
            self.refill()?;
            if self.arg.used == 0 {
                return Ok(None);
            }
        }

        // This is the trust boundary for buffer contents from the ioctl:
        // validate the record header before stepping by it, or a zero/short
        // u64s spins us forever or walks off the valid region.
        const HDR: u32 = std::mem::size_of::<c::bkey>() as u32;
        let rest = self.arg.used - self.pos;
        if rest < HDR {
            return Err(Errno(libc::EPROTO));
        }

        let k = unsafe {
            &*(self.buf.as_ptr().add(self.pos as usize) as *const c::bkey_i)
        };
        let bytes = k.k.u64s as u32 * 8;
        if bytes < HDR || bytes > rest {
            return Err(Errno(libc::EPROTO));
        }
        self.pos += bytes;

        Ok(Some(BkeySC::from(k)))
    }

    pub fn for_each<F>(&mut self, mut f: F) -> Result<(), Errno>
    where
        F: FnMut(BkeySC<'_>) -> ControlFlow<()>,
    {
        while let Some(k) = self.next()? {
            if f(k).is_break() {
                break;
            }
        }
        Ok(())
    }
}
