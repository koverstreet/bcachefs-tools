// SPDX-License-Identifier: GPL-2.0

use super::OwnedNode;
use super::state::state_hashed;
use crate::btree::node::bset::BSET_CACHELINE;
use crate::btree::node::{BtreeNode, NodeContents};
use crate::c;
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::locking::six::six_lock_exit;
use crate::locking::MemallocFlags;
use crate::util::alloc::{flags, Flags, kfree, kv_layout, AllocGfp, Allocator, Global, KVmalloc, Vmalloc, WithFlags};
use crate::os::kernel::is_vmalloc_addr;
use crate::util::maybe_rcu_alloc::alloc_at;
use core::alloc::Layout;
use core::ffi::c_void;
use core::ptr::NonNull;
use cstruct_macros::MaybeZeroable;

/// The bounce pool's elements, kvmalloc()'d as a bounce buffer is: accounted
/// and reclaimable.
pub type BounceBufs = WithFlags<Global>;

pub fn bounce_bufs() -> BounceBufs {
    Global.with_flags(flags::ACCOUNT | flags::RECLAIMABLE)
}

/// Up to a node's size, to sort a node's keys into: kvmalloc()'d, or the
/// bounce pool's, so it never fails outside NORECLAIM. Freed when dropped - whichever buffer it
/// holds then: swap() trades it for a node's, which it's allocated as.
pub struct Bounce<'f> {
    fs:      &'f Fs,
    p:       NonNull<u64>,
    bytes:   usize,
    mempool: bool,
}

impl<'f> Bounce<'f> {
    pub fn new(fs: &'f Fs, bytes: usize) -> Self {
        assert!(bytes <= fs.opts.btree_node_size.get() as usize,
                "a {bytes} byte bounce buffer, past the {} byte node size", fs.opts.btree_node_size.get());

        let _noio = MemallocFlags::noio();
        let gfp = flags::NOWAIT | flags::ACCOUNT | flags::RECLAIMABLE;
        let (p, mempool) = match NonNull::new(alloc_raw(Mem::Kvmalloc, bytes, gfp)) {
            Some(p) => (p.cast(), false),
            None    => (fs.btree.bounce_pool.alloc_raw().expect("a bounce buffer outside NORECLAIM").cast(), true),
        };
        Self { fs, p, bytes, mempool }
    }

    pub fn u64s(&mut self) -> &mut [u64] {
        unsafe { core::slice::from_raw_parts_mut(self.p.as_ptr(), self.bytes / size_of::<u64>()) }
    }

    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.p.as_ptr().cast(), self.bytes) }
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.p.as_ptr().cast(), self.bytes) }
    }

    /// The node's buffer this, and this the node's old one.
    pub fn swap(&mut self, b: &BtreeNode, contents: &mut NodeContents) {
        assert!(self.bytes == b.buf_bytes(), "swapping a {} byte bounce for a {} byte node buffer",
                self.bytes, b.buf_bytes());

        let old = core::mem::replace(&mut contents.bufs.data, self.p.as_ptr().cast());
        btree_node_buf_swap_account(self.fs, b, old.cast(), contents.bufs.data.cast());
        self.p = NonNull::new(old.cast()).expect("a node's buffer");
    }
}

unsafe impl Send for Bounce<'_> {}

impl core::ops::Deref for Bounce<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl core::ops::DerefMut for Bounce<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_bytes_mut()
    }
}

// Its buffer, until it's dropped: swap() trades it, but isn't reached
// through the bytes
unsafe impl crate::os::bio::MapBuf for Bounce<'_> {
    fn as_raw(&self) -> (*mut u8, usize) {
        (self.p.as_ptr().cast(), self.bytes)
    }
}

impl Drop for Bounce<'_> {
    fn drop(&mut self) {
        if self.mempool {
            unsafe { self.fs.btree.bounce_pool.free_raw(self.p.cast()) }
        } else {
            unsafe { free_raw(self.p.as_ptr().cast(), self.bytes) }
        }
    }
}

// ── Node buffers ─────────────────────────────────────────────────────────

crate::module_param! {
    static mm_avoid_compaction: bool = true;
    perm = 0o644, desc = "";
}

#[derive(Clone, Copy)]
enum Mem {
    Vmalloc,
    Kvmalloc,
}

/// @bytes of @mem, not zeroed, with @gfp: free_raw()'s to take back.
fn alloc_raw(mem: Mem, bytes: usize, gfp: Flags) -> *mut c_void {
    let Ok(layout) = kv_layout(bytes) else { return core::ptr::null_mut() };
    let p = match mem {
        Mem::Vmalloc  => Vmalloc.allocate_gfp(layout, gfp),
        Mem::Kvmalloc => KVmalloc.allocate_gfp(layout, gfp),
    };
    p.map_or(core::ptr::null_mut(), |p| p.as_ptr().cast())
}

/// alloc_raw()'s @p, of @bytes - whichever Mem it was, and swapped between
/// a node and a bounce buffer of its size as it may have been: KVmalloc's
/// free takes them all.
///
/// # Safety
/// @p is alloc_raw()'s, of @bytes, or null, and nothing uses it after.
unsafe fn free_raw(p: *mut c_void, bytes: usize) {
    if let Some(p) = NonNull::new(p) {
        unsafe { KVmalloc.deallocate(p.cast(), kv_layout(bytes).expect("its allocation's layout")) }
    }
}

/// A node's buffers, allocated before the node they're for: its data and
/// aux_data, for a node of 1 << byte_order bytes.
#[derive(MaybeZeroable)]
pub struct NodeBufs {
    pub(crate) data:     *mut c_void,
    pub(crate) aux_data: *mut c_void,
    byte_order:          u32,
}

impl NodeBufs {
    pub fn new(fs: &Fs) -> Self {
        Self {
            data:       core::ptr::null_mut(),
            aux_data:   core::ptr::null_mut(),
            byte_order: fs.opts.btree_node_size.get().ilog2(),
        }
    }

    /// BtreeNode::buf_bytes() and aux_data_bytes(), of the node these are for
    fn data_bytes(&self) -> usize {
        1 << self.byte_order
    }

    fn aux_data_bytes(&self) -> usize {
        self.data_bytes() / BSET_CACHELINE * size_of::<u64>()
    }

    /// Its data and aux_data, those it doesn't have yet, with @gfp: data
    /// avoiding compaction if @avoid_compaction.
    pub fn alloc(&mut self, fs: &Fs, gfp: Flags, avoid_compaction: bool) -> Result<(), BchError> {
        /*
         * We probably ought to be using __GFP_RECLAIMABLE - but vmalloc barfs.
         *
         * Shrinkable memory accounting is fubar.
         */
        let gfp = gfp | flags::ACCOUNT;

        if self.data.is_null() {
            let bytes = self.data_bytes();

            if avoid_compaction && mm_avoid_compaction.get() {
                /*
                 * Cursed hack: mm doesn't know how to limit the amount of time
                 * we spend blocked on compaction, even if we specified a
                 * vmalloc fallback.
                 *
                 * So we have to do that ourselves: only try for a high order
                 * page allocation if we're GFP_NOWAIT, otherwise straight to
                 * vmalloc.
                 */
                self.data = alloc_raw(Mem::Vmalloc, bytes, gfp);
            }
            /*
             * mm is cursed: vmalloc can fail for no sane reason, even on 64
             * bit machines, so - fall back to the page allocator if that
             * fails
             */
            if self.data.is_null() {
                self.data = alloc_raw(Mem::Kvmalloc, bytes, gfp);
            }
            if self.data.is_null() {
                return fs.throw(Errcode::ENOMEM_btree_node_mem_alloc);
            }
        }

        if self.aux_data.is_null() {
            self.aux_data = alloc_raw(Mem::Kvmalloc, self.aux_data_bytes(), gfp);
            if self.aux_data.is_null() {
                return fs.throw(Errcode::ENOMEM_btree_node_mem_alloc);
            }
        }

        Ok(())
    }

    pub fn free(&mut self) {
        unsafe {
            free_raw(self.data, self.data_bytes());
            free_raw(self.aux_data, self.aux_data_bytes());
        }
        self.data = core::ptr::null_mut();
        self.aux_data = core::ptr::null_mut();
    }

    /// A node's, freed as it's freed: counted as reclaimed.
    pub fn free_accounted(&mut self) {
        if self.data.is_null() {
            return;
        }

        /*
         * This should really be done in slub/vmalloc, but we're using the
         * kmalloc_large() path, so we're working around a slub bug by doing
         * this here:
         */
        crate::os::kernel::account_reclaimed(self.data_bytes());
        if !self.aux_data.is_null() {
            crate::os::kernel::account_reclaimed(self.aux_data_bytes());
        }

        self.free();
    }
}

/// A node of the cache's, borrowed for as long as the filesystem: the cache
/// never frees one while it runs, only reuses it - so whoever finds one
/// unlocked checks it's still the node it wants once it's locked.
///
/// # Safety
/// @b is the cache's: cache_node_struct_alloc()'s, or found through its
/// lists, its table, a key's mem_ptr or a path.
pub(crate) unsafe fn cached_node<'f>(_fs: &'f Fs, b: *const BtreeNode) -> &'f BtreeNode {
    unsafe { &*b }
}

/// A struct btree, made by its init(), with @gfp, on bc->nodes: every btree
/// node is the cache's. Freed by node_mem_free().
pub(super) fn cache_node_struct_alloc(fs: &Fs, pcpu_read_locks: bool, gfp: Flags) -> Option<&BtreeNode> {
    let slot = {
        let _gfp = MemallocFlags::for_gfp(gfp);
        fs.btree.cache.nodes.get_slot().ok()?
    };

    let flags = if pcpu_read_locks { c::SixLockInitFlags::pcpu } else { c::SixLockInitFlags::empty() };
    let b = alloc_at::<_, Global, BchError>(BtreeNode::init(fs, flags, gfp), gfp).ok()?;
    unsafe {
        (*b.as_ptr()).nodes_idx = slot.set(b.cast());
        Some(cached_node(fs, b.as_ptr()))
    }
}

/// A node for the cache, with its buffers, GFP_KERNEL. Freed by
/// OwnedNode::free() then node_mem_free().
pub(crate) fn node_mem_alloc(fs: &Fs) -> Option<OwnedNode> {
    let mut bufs = NodeBufs::new(fs);
    let n = bufs.alloc(fs, Flags::empty(), false).ok().and_then(|()| OwnedNode::alloc(fs, false, Flags::empty()));
    let Some(mut n) = n else {
        bufs.free();
        return None;
    };
    n.locked().contents_mut().bufs = bufs;
    Some(n)
}

/// A node of the cache's freed, off bc->nodes: its buffers already are.
///
/// # Safety
/// @b is ours alone, nothing uses it after, and it's on no list - and nothing
/// walks bc->nodes, which could still find it: the cache's are only freed at
/// exit.
pub unsafe fn node_mem_free(fs: &Fs, b: NonNull<BtreeNode>) {
    unsafe {
        fs.btree.cache.nodes.remove((*b.as_ptr()).nodes_idx);
        six_lock_exit((&raw mut (*b.as_ptr()).c.lock).cast());
        kfree(b.cast(), Layout::new::<BtreeNode>());
    }
}

/// @b's buffer swapped, @old for @new: the cache counts the vmalloc'd of its
/// hashed nodes - an unhashed node's is counted when it's hashed.
fn btree_node_buf_swap_account(fs: &Fs, b: &BtreeNode, old: *const c_void, new: *const c_void) {
    let delta = is_vmalloc_addr(new) as isize - is_vmalloc_addr(old) as isize;

    if delta != 0 {
        let mut lists = fs.btree.cache.lock.lock();
        if state_hashed(b.cache_state(&lists)) {
            lists.nr_vmalloc = lists.nr_vmalloc.checked_add_signed(delta)
                .expect("the hashed nodes' vmalloc count, past them");
        }
    }
}
