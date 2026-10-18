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

use anyhow::anyhow;
use bcachefs_kernel::c;
use bcachefs_kernel::btree::bkey::BkeySC;
use bcachefs_kernel::util::vstructs::{plain_bytes, VstructRef};

use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::ioctl::grow_on_erange;

/// Iteration flags, mirroring BCH_IOCTL_QUERY_BTREE_KEYS_*:
#[derive(Clone, Copy, Default)]
pub struct OnlineIterFlags(pub u32);

impl OnlineIterFlags {
    pub const SLOTS: Self         = Self(c::BCH_IOCTL_QUERY_BTREE_KEYS_slots);
    pub const PREV: Self          = Self(c::BCH_IOCTL_QUERY_BTREE_KEYS_prev);
    pub const ALL_SNAPSHOTS: Self = Self(c::BCH_IOCTL_QUERY_BTREE_KEYS_all_snapshots);
    pub const NOFILTER_WHITEOUTS: Self =
        Self(c::BCH_IOCTL_QUERY_BTREE_KEYS_nofilter_whiteouts);
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
    /// u64s: the kernel writes each key at a multiple of 8 from the start,
    /// aligned for one if the buffer is
    buf:  Vec<u64>,
    /// Byte offset of the next unconsumed key in `buf`; == arg.used when
    /// the buffer is drained.
    pos:  usize,
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
            buf: vec![0; buf_size.div_ceil(8)],
            pos: 0,
        }
    }

    /// ERANGE is a buffer too small for even one key: grown, and kept grown.
    fn refill(&mut self) -> std::io::Result<()> {
        self.pos = 0;
        grow_on_erange(self.buf.len() * 8, 1 << 20, |bytes| {
            self.buf.resize(bytes / 8, 0);
            self.arg.buf = self.buf.as_mut_ptr() as u64;
            self.arg.buf_size = bytes as u32;
            self.arg.used = 0;
            self.fs.query_btree_keys(&mut self.arg)
        })
    }

    /// The next key, or None at the end of the range. Lending: the
    /// returned key borrows the internal buffer and dies at the next
    /// call.
    ///
    /// The trust boundary for what the ioctl wrote: each key is checked
    /// against the bytes left before it's stepped past - a short u64s would
    /// spin us, a long one walk off what was written - and one that doesn't
    /// fit is an error saying where and why.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> anyhow::Result<Option<BkeySC<'_>>> {
        if self.pos >= self.arg.used as usize {
            if self.arg.done != 0 {
                return Ok(None);
            }
            self.refill()?;
            if self.arg.used == 0 {
                return Ok(None);
            }
        }

        let reply = plain_bytes(&self.buf[..]).get(..self.arg.used as usize).ok_or_else(|| {
            anyhow!("{} bytes used, of {}", self.arg.used, self.buf.len() * 8)
        })?;
        let k = VstructRef::<c::bkey_i>::new(&reply[self.pos..])
            .map_err(|e| anyhow!("reply at byte {}: {e}", self.pos))?;
        self.pos += k.as_bytes().len();

        Ok(Some(BkeySC::from(k.key())))
    }

    pub fn for_each<F>(&mut self, mut f: F) -> anyhow::Result<()>
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
