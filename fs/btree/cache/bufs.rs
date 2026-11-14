// SPDX-License-Identifier: GPL-2.0

use crate::btree::locking::bch2_btree_lock_init;
use crate::btree::node::bset::BSET_CACHELINE;
use crate::btree::node::{BtreeNode, NodeContents};
use crate::c;
use crate::errcode::{Errcode, BchError};
use crate::format::BkeyType;
use crate::fs::Fs;
use crate::locking::six::{six_lock_exit, SixHold};
use crate::locking::MemallocFlags;
use crate::util::alloc::{kfree, KVmalloc};
use crate::util::mempool::Elements;
use crate::os::kernel::is_vmalloc_addr;
use core::alloc::Layout;
use core::ffi::c_void;
use core::ptr::NonNull;

/// The bounce pool's elements, kvmalloc()'d as a bounce buffer is: accounted
/// and reclaimable.
pub struct BounceBufs;

unsafe impl Elements for BounceBufs {
    fn element_alloc(layout: Layout, gfp: c::gfp_t) -> Option<NonNull<u8>> {
        KVmalloc::element_alloc(layout, gfp | c::__GFP_ACCOUNT | c::__GFP_RECLAIMABLE)
    }

    unsafe fn element_free(p: NonNull<u8>, layout: Layout) {
        unsafe { KVmalloc::element_free(p, layout) }
    }
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
        let gfp = c::GFP_NOWAIT | c::__GFP_ACCOUNT | c::__GFP_RECLAIMABLE;
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

        let old = core::mem::replace(&mut contents.data, self.p.as_ptr().cast());
        btree_node_buf_swap_account(self.fs, old.cast(), contents.data.cast());
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
unsafe impl crate::util::bio::MapBuf for Bounce<'_> {
    fn as_raw(&self) -> (*mut u8, usize) {
        (self.p.as_ptr().cast(), self.bytes)
    }
}

impl Drop for Bounce<'_> {
    fn drop(&mut self) {
        if self.mempool {
            unsafe { self.fs.btree.bounce_pool.free_raw(self.p.cast()) }
        } else {
            unsafe { crate::util::alloc::kvfree_unsized(self.p.as_ptr().cast()) }
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
    Kmalloc,
    Vmalloc,
    Kvmalloc,
}

/// @bytes of @mem, not zeroed, with @gfp: kvfree()'s to take back.
/// Userspace's is one allocator, aligned as kmalloc() aligns.
fn alloc_raw(mem: Mem, bytes: usize, gfp: c::gfp_t) -> *mut c_void {
    #[cfg(kernel)]
    unsafe {
        use kernel::bindings::{krealloc_node_align, kvrealloc_node_align, NUMA_NO_NODE};

        let gfp = crate::util::alloc::current_gfp_context_raw(gfp);
        match mem {
            Mem::Kmalloc  => krealloc_node_align(core::ptr::null(), bytes, 1, gfp, NUMA_NO_NODE).cast(),
            Mem::Vmalloc  => c::__vmalloc_noprof(bytes as _, gfp).cast(),
            Mem::Kvmalloc => kvrealloc_node_align(core::ptr::null(), bytes, 1, gfp, NUMA_NO_NODE).cast(),
        }
    }
    #[cfg(not(kernel))]
    {
        let _ = (mem, gfp);
        crate::util::alloc::kvrealloc(core::ptr::null_mut(), 0, bytes)
            .map_or(core::ptr::null_mut(), |p| p.as_ptr().cast())
    }
}

/// # Safety
/// @l is valid to write, and on no list.
unsafe fn init_list_head(l: *mut c::list_head) {
    unsafe {
        (*l).next = l;
        (*l).prev = l;
    }
}

/// A node's buffers, allocated before the node they're for: its data and
/// aux_data, for a node of 1 << byte_order bytes.
pub(crate) struct NodeBufs {
    pub(super) data:     *mut c_void,
    pub(super) aux_data: *mut c_void,
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

    /// Its data and aux_data, those it doesn't have yet, with @gfp: data
    /// avoiding compaction if @avoid_compaction.
    pub fn alloc(&mut self, fs: &Fs, gfp: c::gfp_t, avoid_compaction: bool) -> Result<(), BchError> {
        /*
         * We probably ought to be using __GFP_RECLAIMABLE - but vmalloc barfs.
         *
         * Shrinkable memory accounting is fubar.
         */
        let gfp = gfp | c::__GFP_ACCOUNT;

        if self.data.is_null() {
            let bytes = 1usize << self.byte_order;

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
                let mem = if gfp & c::__GFP_RECLAIM != 0 { Mem::Vmalloc } else { Mem::Kmalloc };
                self.data = alloc_raw(mem, bytes, gfp);
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
            let bytes = ((1usize << self.byte_order) / BSET_CACHELINE) * size_of::<u64>();
            self.aux_data = alloc_raw(Mem::Kvmalloc, bytes, gfp);
            if self.aux_data.is_null() {
                return fs.throw(Errcode::ENOMEM_btree_node_mem_alloc);
            }
        }

        Ok(())
    }

    pub fn free(&mut self) {
        unsafe {
            crate::util::alloc::kvfree_unsized(self.data);
            crate::util::alloc::kvfree_unsized(self.aux_data);
        }
        self.data = core::ptr::null_mut();
        self.aux_data = core::ptr::null_mut();
    }
}

/// A struct btree: its lock initialized and nothing else, with @gfp. Freed by
/// node_struct_free().
fn node_struct_alloc(fs: &Fs, pcpu_read_locks: bool, gfp: c::gfp_t) -> Option<NonNull<BtreeNode>> {
    let layout = Layout::new::<BtreeNode>();
    #[cfg(kernel)]
    let p = unsafe {
        use kernel::bindings::{krealloc_node_align, NUMA_NO_NODE};

        krealloc_node_align(core::ptr::null(), layout.size(), layout.align(),
                            crate::util::alloc::current_gfp_context_raw(gfp | c::__GFP_ZERO), NUMA_NO_NODE)
    };
    #[cfg(not(kernel))]
    let p = {
        let _ = gfp;
        unsafe { std::alloc::alloc_zeroed(layout) }
    };
    let b = NonNull::new(p.cast::<BtreeNode>())?;

    unsafe {
        let p = b.as_ptr();
        let k = &mut (*p).contents_mut().key.key_mut().k;
        *k = c::bkey::new();
        k.set_key_type(BkeyType::btree_ptr);
        init_list_head((&raw mut (*p).write_blocked).cast());
        (*p).byte_order = fs.opts.btree_node_size.get().ilog2() as u8;
        let flags = if pcpu_read_locks {
            c::SixLockInitFlags::pcpu
        } else {
            c::SixLockInitFlags::empty()
        };
        bch2_btree_lock_init(&raw mut (*p).c, flags, gfp);
    }
    Some(b)
}

/// A node of the cache's, borrowed for as long as the filesystem: the cache
/// never frees one while it runs, only reuses it - so whoever finds one
/// unlocked checks it's still the node it wants once it's locked.
///
/// # Safety
/// @b is the cache's: cache_node_struct_alloc()'s, or found through its
/// lists, its table or a key's mem_ptr.
pub(super) unsafe fn cached_node<'f>(_fs: &'f Fs, b: *const BtreeNode) -> &'f BtreeNode {
    unsafe { &*b }
}

/// node_struct_alloc(), on bc->nodes, as every node the cache has is. Freed by
/// node_mem_free().
pub(super) fn cache_node_struct_alloc(fs: &Fs, pcpu_read_locks: bool, gfp: c::gfp_t) -> Option<NonNull<BtreeNode>> {
    let slot = {
        let _gfp = MemallocFlags::for_gfp(gfp);
        fs.btree.cache.nodes.get_slot().ok()?
    };

    let b = node_struct_alloc(fs, pcpu_read_locks, gfp)?;
    unsafe { (*b.as_ptr()).nodes_idx = slot.set(b.cast()) };
    Some(b)
}

/// A new node's locks, intent and write: no one else has it to hold them.
pub(super) fn new_node_lock(b: &BtreeNode) -> SixHold<'_> {
    b.c.lock.try_lock_as(c::SixLockType::SIX_LOCK_intent)
        .and_then(|intent| intent.try_write().ok())
        .expect("a new node locked by someone else")
}

/// A node with its buffers, GFP_KERNEL - off the cache's lists, not hashed -
/// from @struct_alloc.
fn node_with_bufs(fs: &Fs, struct_alloc: fn(&Fs, bool, c::gfp_t) -> Option<NonNull<BtreeNode>>)
    -> Option<NonNull<BtreeNode>>
{
    let mut bufs = NodeBufs::new(fs);
    let b = bufs.alloc(fs, c::GFP_KERNEL, false).ok().and_then(|()| struct_alloc(fs, false, c::GFP_KERNEL));
    let Some(b) = b else {
        bufs.free();
        return None;
    };
    unsafe {
        (*b.as_ptr()).contents_mut().data = bufs.data.cast();
        (*b.as_ptr()).aux_data = bufs.aux_data;
    }
    Some(b)
}

/// A node for the cache, with its buffers. Freed by node_data_free() then
/// node_mem_free().
pub fn node_mem_alloc(fs: &Fs) -> Option<NonNull<BtreeNode>> {
    node_with_bufs(fs, cache_node_struct_alloc)
}

/// A node with its buffers for node scan to read into: not the cache's, so
/// not on bc->nodes, which walks the cache's nodes while they're live and
/// can't find one freed under it. Freed by node_data_free() then
/// node_struct_free().
pub fn node_scratch_alloc(fs: &Fs) -> Option<NonNull<BtreeNode>> {
    node_with_bufs(fs, node_struct_alloc)
}

/// # Safety
/// @b is ours alone, its write done.
pub unsafe fn node_data_free(b: NonNull<BtreeNode>) {
    let p = b.as_ptr();
    ebug_assert!(!unsafe { (*p).flags.write_in_flight() }, "freeing the buffers of a node with a write in flight");

    let mut bufs = unsafe {
        NodeBufs {
            data:       (*p).contents_mut().data.cast(),
            aux_data:   (*p).aux_data,
            byte_order: (*p).byte_order as u32,
        }
    };
    if bufs.data.is_null() {
        return;
    }

    /*
     * This should really be done in slub/vmalloc, but we're using the
     * kmalloc_large() path, so we're working around a slub bug by doing
     * this here:
     */
    #[cfg(kernel)]
    unsafe {
        use crate::os::kernel::PAGE_SIZE;

        c::mm_account_reclaimed_pages(((*p).buf_bytes() / PAGE_SIZE) as _);
        if !bufs.aux_data.is_null() {
            c::mm_account_reclaimed_pages(((*p).aux_data_bytes() / PAGE_SIZE) as _);
        }
    }

    unsafe {
        (*p).flags.set_just_written(false);
        (*p).contents_mut().data = core::ptr::null_mut();
        (*p).aux_data = core::ptr::null_mut();
    }
    bufs.free();
}

/// @b, a struct btree node_struct_alloc() made, freed: its buffers already
/// are.
///
/// # Safety
/// @b is ours alone, nothing uses it after, and it's on no list.
pub unsafe fn node_struct_free(b: NonNull<BtreeNode>) {
    unsafe {
        six_lock_exit((&raw mut (*b.as_ptr()).c.lock).cast());
        kfree(b.cast(), Layout::new::<BtreeNode>());
    }
}

/// A node of the cache's freed, off bc->nodes: its buffers already are.
///
/// # Safety
/// As node_struct_free() - and nothing walks bc->nodes, which could still
/// find it: the cache's are only freed at exit.
pub unsafe fn node_mem_free(fs: &Fs, b: NonNull<BtreeNode>) {
    unsafe {
        fs.btree.cache.nodes.remove((*b.as_ptr()).nodes_idx);
        node_struct_free(b);
    }
}

/// A node's buffer swapped, @old for @new: the cache counts the vmalloc'd.
fn btree_node_buf_swap_account(fs: &Fs, old: *const c_void, new: *const c_void) {
    let delta = is_vmalloc_addr(new) as isize - is_vmalloc_addr(old) as isize;

    if delta != 0 {
        let mut lists = fs.btree.cache.lock.lock();
        lists.nr_vmalloc = lists.nr_vmalloc.wrapping_add_signed(delta);
    }
}
