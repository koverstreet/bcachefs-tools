// SPDX-License-Identifier: GPL-2.0

use core::cmp::{Ordering, Reverse};
use core::ffi::{c_int, c_uint};
use core::mem::{offset_of, replace, size_of};
use core::ptr::NonNull;

use crate::data_structures::array_vec::ArrayVec;
use cstruct_macros::c_export;

use crate::btree::bkey::{BkeyI, BkeyV, POS_MIN, SPOS_MAX};
use crate::init::error::BkeyValidate;
use crate::btree::cache::{__bch2_btree_node_mem_alloc, bch2_btree_node_data_free, bch2_btree_node_mem_free};
use crate::btree::journal_overlay;
use crate::btree::node::BtreeNode;
use crate::btree::read::bch2_btree_node_read_done;
use crate::c::{self, bch_errcode, BtreePtrV2};
use crate::data::ptrs::extent_ptr_to_text;
use crate::errcode::{ret_to_c, BchError};
use crate::format::{Bpos, Btree, DataType};
use crate::fs::{btree_id_can_reconstruct, Dev, Fs};
use crate::init::passes;
use crate::locking::Locked;
use crate::types::{Le16, Le64};
use crate::util::alloc::KVVec;
use crate::util::bio::{buf_nr_bvecs, Bio, BioBox, BioOwner};
use crate::data_structures::eytzinger::Eytzinger0;
use crate::util::ffi::Opaque;
use crate::data_structures::heap::{MinHeap, PeekMut};
use crate::util::kernel::{cond_resched, jiffies, local_clock, sort_nonatomic, time_after, HZ};
use crate::util::log::{print_str, LogLevel, LogMsg};
use crate::util::par::{self, Backend};
use crate::util::rcu::read_lock;
use crate::data_structures::sort::dedup_by;
use crate::util::vstructs::Head;
use crate::util::Printbuf;
use crate::{bch_err, bch_err_dev_ratelimited, bch_info, bch_info_dev, prt};

pub(crate) type NodeScan = Locked<c::mutex, KVVec<FoundBtreeNode>>;

#[derive(Clone)]
pub(crate) struct FoundBtreeNode {
    btree:           Btree,
    level:           u8,
    range_updated:   bool,
    sectors_written: u16,
    seq:             u32,
    journal_seq:     u64,
    cookie:          u64,
    min_key:         Bpos,
    max_key:         Bpos,
    ptrs:            ArrayVec<c::bch_extent_ptr, { c::BCH_REPLICAS_MAX as usize }>,
}

#[repr(C)]
struct FoundKey {
    k:    c::bkey,
    v:    BtreePtrV2,
    ptrs: [c::bch_extent_ptr; c::BCH_REPLICAS_MAX as usize],
}

impl FoundKey {
    fn as_bkey_i(&self) -> &BkeyI {
        unsafe { &*BkeyI::from_raw(core::ptr::from_ref(self).cast()) }
    }
}

impl FoundBtreeNode {
    fn node_id(&self) -> (Btree, u8, u64) {
        (self.btree, self.level, self.cookie)
    }

    fn written(&self) -> (u16, u64) {
        (self.sectors_written, self.journal_seq)
    }

    /*
     * Given two found btree nodes, if their sequence numbers are equal, take the
     * one that's readable:
     */
    fn time(&self) -> (u32, u64) {
        (self.seq, self.journal_seq)
    }

    fn pos(&self) -> (Btree, Reverse<u8>, Bpos, Reverse<(u32, u64)>) {
        (self.btree, Reverse(self.level), self.min_key, Reverse(self.time()))
    }

    fn overlaps(&self, r: &Self) -> bool {
        self.btree == r.btree && self.level == r.level && self.max_key > r.min_key
    }

    #[cold]
    pub(crate) fn to_text(&self, out: &mut Printbuf, fs: &Fs) {
        prt!(out, "btree={} level={} seq={} journal_seq={} sectors_written={} cookie={:x} {}-{}",
             self.btree, self.level, self.seq, self.journal_seq, self.sectors_written, self.cookie,
             self.min_key, self.max_key);
        if self.range_updated {
            prt!(out, " range updated");
        }
        out.newline();

        let mut out = out.indent(2);
        let rcu = read_lock();
        for ptr in &self.ptrs {
            extent_ptr_to_text(&mut out, Some(fs), ptr, &rcu);
            out.newline();
        }
    }

    fn to_key(&self) -> FoundKey {
        let mut k = FoundKey {
            k:    c::bkey::new(),
            v:    BtreePtrV2::default(),
            ptrs: [c::bch_extent_ptr::default(); c::BCH_REPLICAS_MAX as usize],
        };

        k.k.type_ = c::bch_bkey_type::KEY_TYPE_btree_ptr_v2.0 as u8;
        k.k.set_val_bytes(size_of::<BtreePtrV2>() + size_of::<c::bch_extent_ptr>() * self.ptrs.len());
        k.k.p = self.max_key;
        k.v.seq = Le64::new(self.cookie);
        k.v.sectors_written = Le16::new(self.sectors_written);
        k.v.min_key = self.min_key;
        k.v.set_btree_ptr_range_updated(self.range_updated.into());
        k.ptrs[..self.ptrs.len()].copy_from_slice(&self.ptrs);
        k
    }
}

#[cold]
fn dump_nodes(fs: &Fs, what: &str, nodes: &[FoundBtreeNode]) -> bool {
    if !(false && fs.opts.verbose.get() != 0) {
        return false;
    }

    let mut buf = Printbuf::new();
    prt!(&mut buf, "bch2_scan_for_btree_nodes: {what}:\n");
    {
        let mut out = buf.indent(2);
        for n in nodes {
            n.to_text(&mut out, fs);
        }
    }
    print_str(fs, LogLevel::INFO, &mut buf);
    true
}

struct NodeBuf<'f> {
    fs: &'f Fs,
    b:  NonNull<BtreeNode>,
}

impl<'f> NodeBuf<'f> {
    fn alloc(fs: &'f Fs) -> Option<Self> {
        let b = NonNull::new(unsafe { __bch2_btree_node_mem_alloc(fs.as_ptr()) })?;
        Some(NodeBuf { fs, b })
    }

    fn node(&mut self) -> &mut BtreeNode {
        unsafe { self.b.as_mut() }
    }

    fn bytes(&mut self, bytes: usize) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.node().contents.get_mut().data.cast::<u8>(), bytes) }
    }
}

impl Drop for NodeBuf<'_> {
    fn drop(&mut self) {
        unsafe {
            bch2_btree_node_data_free(self.b.as_ptr());
            bch2_btree_node_mem_free(self.fs.as_ptr(), self.b.as_ptr());
        }
    }
}

struct DevScan<'f> {
    fs:  &'f Fs,
    ca:  &'f Dev,
    buf: NodeBuf<'f>,
    bio: BioBox<Bio>,
}

impl<'f> DevScan<'f> {
    fn new(fs: &'f Fs, ca: &'f Dev) -> Result<Self, BchError> {
        let Some(mut buf) = NodeBuf::alloc(fs) else {
            bch_err!(fs, "read_btree_nodes_worker: error allocating buf");
            return Err(c::ENOMEM.into());
        };

        let nr_vecs = buf_nr_bvecs(buf.node().contents.get_mut().data.cast(), fs.opts.btree_node_size.get() as usize)
            .try_into().expect("bvecs for a btree node");
        let Ok(bio) = BioBox::<Bio>::new::<BchError, _>(ca.disk_sb.bdev, nr_vecs, c::req_op_REQ_OP_READ, |b| b.init())
        else {
            bch_err!(fs, "read_btree_nodes_worker: error allocating bio");
            return Err(c::ENOMEM.into());
        };

        Ok(DevScan { fs, ca, buf, bio })
    }

    fn read(&mut self, offset: u64, bytes: usize) -> Result<(), BchError> {
        let DevScan { ca, buf, bio, .. } = self;

        bio.reset(ca.disk_sb.bdev, c::req_op_REQ_OP_READ);
        bio.bio_mut().set_sector(offset);

        let submit_time = local_clock();
        // remapped each read: read_done swaps the node's buffer
        let ret = unsafe { bio.remap_read(buf.bytes(bytes)) }.submit_wait();
        ca.account_io_completion(c::bch_member_error_type::BCH_MEMBER_ERROR_read, submit_time, ret.is_ok());
        ret
    }

    fn try_read_node(&mut self, offset: u64) -> Result<(), BchError> {
        let (fs, ca) = (self.fs, self.ca);

        if let Err(e) = self.read(offset, fs.opts.block_size.get().into()) {
            bch_err_dev_ratelimited!(ca, "IO error in try_read_btree_node() at {}: {}", offset, e);
            return Ok(());
        }

        let bn = self.buf.node().contents.get_mut().data();
        if bn.magic.get() != fs.bset_magic() {
            return Ok(());
        }

        let csum_type = c::bch_csum_type(bn.keys.csum_type() as u32);
        if csum_type.is_encryption() {
            if fs.encryption_key().is_none() {
                return Ok(());
            }

            let nonce = bn.keys.nonce(0);
            let (flags, keys) = (offset_of!(Head<c::btree_node>, flags), offset_of!(Head<c::btree_node>, keys));
            let _ = fs.encrypt(csum_type, nonce, &mut self.buf.bytes(keys)[flags..]);
        }

        let bn = self.buf.node().contents.get_mut().data();
        let id = bn.id_lo() | bn.id_hi() << 4;
        if btree_id_can_reconstruct(id as u32) ||
           bn.level() >= c::BTREE_MAX_DEPTH as u64 ||
           id >= c::BTREE_ID_NR_MAX as u64 {
            return Ok(());
        }

        let ptr = c::bch_extent_ptr::new()
            .with_offset(offset)
            .with_dev(ca.dev_idx)
            .with_generation(ca.bucket_gen(ca.sector_to_bucket(offset), &read_lock()).unwrap_or(u8::MAX));
        let mut n = FoundBtreeNode {
            btree:           Btree(id as u32),
            level:           bn.level() as u8,
            range_updated:   false,
            sectors_written: 0,
            seq:             bn.seq() as u32,
            journal_seq:     0,
            cookie:          bn.keys.seq.get(),
            min_key:         bn.min_key,
            max_key:         bn.max_key,
            ptrs:            ArrayVec::from_iter([ptr]),
        };

        if let Err(e) = self.read(offset, fs.opts.btree_node_size.get() as usize) {
            bch_err_dev_ratelimited!(ca, "IO error in try_read_btree_node() at {} reading the whole node: {}",
                                     offset, e);
        }

        let b = self.buf.node();
        b.contents.get_mut().set_key(n.to_key().as_bkey_i());

        let mut err = Printbuf::new();
        let ret = unsafe {
            bch2_btree_node_read_done(fs.as_ptr(), ca.as_ptr(), b, core::ptr::null_mut(), err.as_raw())
        };
        if ret != 0 {
            return Ok(());
        }

        /* read_done will swap out b->data for another buffer */
        let bn = b.contents.get_mut().data();
        /*
         * Grab journal_seq here because we want the max journal_seq of
         * any bset; read_done sorts down to a single set and picks the
         * max journal_seq
         */
        n.journal_seq     = bn.keys.journal_seq.get();
        n.sectors_written = b.written.get();

        let mut nodes = fs.btree.node_scan.lock();
        if bn.keys.big_endian() != cfg!(target_endian = "big") {
            bch_err!(fs, "try_read_btree_node() can't handle endian conversion");
            return Err(c::EINVAL.into());
        }
        nodes.push(n)?;
        Ok(())
    }

    fn run(&mut self) -> Result<(), BchError> {
        let (fs, ca) = (self.fs, self.ca);
        let mut last_print = jiffies();

        let mi = ca.mi.get();
        let bitmap_on = fs.version_upgrade_complete().0 >=
            c::bcachefs_metadata_version::bcachefs_metadata_version_mi_btree_bitmap.0;
        let buckets = || (u64::from(mi.first_bucket)..mi.nbuckets).filter(|&bucket| {
            !bitmap_on || ca.btree_bitmap_marked_sectors_any(ca.bucket_to_sector(bucket), mi.bucket_size.into())
        });
        let buckets_to_scan = buckets().count();
        let (bucket_size, btree_sectors) = (u32::from(mi.bucket_size), fs.btree_sectors());

        let mut ret = Ok(());
        for (scanned, bucket) in buckets().enumerate() {
            let start = ca.bucket_to_sector(bucket);
            let offsets = (0..).step_by(btree_sectors as usize).take_while(|o| o + btree_sectors <= bucket_size);
            for bucket_offset in offsets {
                ret = ret.and(self.try_read_node(start + u64::from(bucket_offset)));
            }

            if time_after(jiffies(), last_print + HZ * 30) {
                bch_info_dev!(ca, "read_btree_nodes_worker: {:2}% done", (scanned + 1) * 100 / buckets_to_scan);
                last_print = jiffies();
            }
        }
        ret
    }
}

fn read_btree_nodes(fs: &Fs) -> Result<(), BchError> {
    par::scope(Backend::Kthreads, |sc| {
        let mut msg = LogMsg::with_level(fs, LogLevel::NOTICE);
        prt!(&mut msg, "scanning for btree nodes on");

        for ca in fs.online_members().filter(|ca| ca.mi.get().data_allowed.contains(DataType::btree.into())) {
            let name = ca.name().to_str().unwrap_or("?");
            let job_ca = ca.clone();

            sc.spawn(format_args!("read_btree_nodes/{name}"), move || DevScan::new(fs, &job_ca)?.run())
                .inspect_err(|e| bch_err!(fs, "starting kthread: {e}"))?;
            prt!(&mut msg, " {name}");
        }
        Ok(())
    })
}

fn handle_overwrites(l: &mut FoundBtreeNode,
                     heap: &mut MinHeap<FoundBtreeNode, impl FnMut(&FoundBtreeNode, &FoundBtreeNode) -> Ordering>) {
    while let Some(mut r) = heap.peek_mut().filter(|r| l.overlaps(r)) {
        match l.time().cmp(&r.time()) {
            Ordering::Less => {
                assert!(l.min_key != r.min_key);

                l.max_key = r.min_key.predecessor();
                l.range_updated = true;
            }
            Ordering::Equal if r.level != 0 => {
                PeekMut::pop(r);
            }
            _ if l.max_key >= r.max_key => {
                PeekMut::pop(r);
            }
            _ => {
                r.min_key = l.max_key.successor();
                r.range_updated = true;
            }
        }

        cond_resched();
    }
}

fn scan_for_btree_nodes(fs: &Fs) -> Result<(), BchError> {
    let scan = &fs.btree.node_scan;

    if !scan.lock().is_empty() {
        return Ok(());
    }

    read_btree_nodes(fs)?;

    let mut nodes = scan.lock();

    if nodes.is_empty() {
        bch_err!(fs, "bch2_scan_for_btree_nodes: no btree nodes found");
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_node_scan_no_nodes);
    }

    dump_nodes(fs, "nodes found", &nodes);

    /*
     * Not every copy of a node is a replica of its current version: a replica
     * dropped from the key in place (write error, device removal) misses the bsets
     * written after it, and a misplaced write leaves a copy where the key never
     * pointed. Newest version first:
     */
    sort_nonatomic(&mut nodes, |l, r| (l.node_id(), Reverse(l.written())).cmp(&(r.node_id(), Reverse(r.written()))));

    let mut ignored: Option<Printbuf> = None;
    let kept = dedup_by(&mut nodes, |n, prev| {
        if n.node_id() != prev.node_id() {
            return false;
        }

        /* A key with two pointers to one device is invalid (ptr_to_duplicate_device): */
        if n.written() == prev.written() &&
           !prev.ptrs.is_full() &&
           !prev.ptrs.iter().any(|p| p.dev() == n.ptrs[0].dev()) {
            prev.ptrs.push(n.ptrs[0]);
        } else {
            let out = ignored.get_or_insert_with(|| {
                let mut out = Printbuf::new();
                prt!(&mut out, "bch2_scan_for_btree_nodes: \
                                ignoring stale, excess or same-device copies of btree nodes:\n");
                out.indent_add(2);
                out
            });
            n.to_text(out, fs);
        }
        true
    });
    nodes.truncate(kept);

    if let Some(mut ignored) = ignored {
        print_str(fs, LogLevel::INFO, &mut ignored);
    }

    sort_nonatomic(&mut nodes, |l, r| l.pos().cmp(&r.pos()));

    dump_nodes(fs, "nodes after merging replicas", &nodes);

    let mut heap = MinHeap::new(replace(&mut *nodes, KVVec::new()),
                                |l: &FoundBtreeNode, r: &FoundBtreeNode| l.pos().cmp(&r.pos()));
    while let Some(n) = heap.pop() {
        let i = nodes.len();
        nodes.push(n)?;
        handle_overwrites(&mut nodes[i], &mut heap);
    }

    assert!(nodes.windows(2).all(|w| !w[0].overlaps(&w[1])));

    if !dump_nodes(fs, "nodes found after overwrites", &nodes) {
        bch_info!(fs, "btree node scan found {} nodes after overwrites", nodes.len());
    }

    Eytzinger0::<usize>::sort_by(&mut nodes, |l, r| l.pos().cmp(&r.pos()));
    Ok(())
}

fn in_range(nodes: &[FoundBtreeNode], btree: Btree, level: u8, min: Bpos, max: Bpos)
    -> impl Iterator<Item = &FoundBtreeNode>
{
    let tree = Eytzinger0::<usize>::of(nodes);
    let mut idx = Eytzinger0::<usize>::find_gt_by(nodes, |n| {
        (n.btree, Reverse(n.level), n.max_key).cmp(&(btree, Reverse(level), min))
    });

    core::iter::from_fn(move || {
        let i = idx.take()?;
        let n = &nodes[i];
        (n.btree == btree && n.level == level && n.min_key < max).then(|| {
            idx = tree.next(i);
            n
        })
    })
}

#[c_export]
pub fn bch2_btree_node_is_stale(c: &Opaque<Fs>, b: &BtreeNode) -> bool {
    let nodes = Fs::from_c(c).btree.node_scan.lock();
    let contents = unsafe { b.contents() };
    let bn = contents.data();

    in_range(&nodes, Btree(b.c.btree_id.into()), b.c.level, bn.min_key, contents.key.k.p)
        .any(|n| u64::from(n.seq) > bn.seq())
}

fn run_scan(fs: &Fs, out: &mut Printbuf) -> Result<(), BchError> {
    passes::run_explicit(fs, out, c::bch_recovery_pass::BCH_RECOVERY_PASS_scan_for_btree_nodes,
                         c::bch_run_recovery_pass_flags::empty())
}

#[c_export]
pub fn bch2_btree_has_scanned_nodes(c: &Opaque<Fs>, btree: Btree, out: &mut Printbuf) -> c_int {
    let fs = Fs::from_c(c);

    match run_scan(fs, out) {
        Ok(())  => in_range(&fs.btree.node_scan.lock(), btree, 0, POS_MIN, SPOS_MAX).next().is_some().into(),
        Err(e)  => ret_to_c(Err(e)),
    }
}

fn get_scanned_nodes(fs: &Fs, btree: Btree, level: u32, node_min: Bpos, node_max: Bpos,
                     out: &mut Printbuf, nodes_found: &mut usize) -> Result<(), BchError> {
    if btree != Btree::alloc && btree_id_can_reconstruct(btree.0) {
        return Ok(());
    }

    run_scan(fs, out)?;

    if fs.opts.verbose.get() != 0 {
        bch_info!(fs, "bch2_get_scanned_nodes(): recovery btree={btree} level={level} {node_min} - {node_max}");
    }

    let nodes = fs.btree.node_scan.lock();
    for found in in_range(&nodes, btree, level as u8, node_min, node_max) {
        let n = FoundBtreeNode {
            range_updated: found.range_updated || found.min_key < node_min || found.max_key > node_max,
            min_key:       found.min_key.max(node_min),
            max_key:       found.max_key.min(node_max),
            ..found.clone()
        };
        let k = n.to_key();

        let mut from = c::bkey_validate_context { level: (level + 1) as u8, btree, ..Default::default() };
        from.bits.set_from(c::bkey_validate_from::BKEY_VALIDATE_btree_node);
        assert!(crate::btree::bkey_methods::bkey_validate(
            &BkeyValidate { fs, k: k.as_bkey_i().as_sc().into(), from: &from }).is_ok());

        if *nodes_found == 0 {
            prt!(out, "recovering from btree node scan at btree={btree} level={level}\n");
            out.indent_add(2);
        }

        *nodes_found += 1;

        match *nodes_found {
            ..10 => prt!(out, "{}\n", k.as_bkey_i().as_sc().to_text(fs)),
            10   => prt!(out, "<many>\n"),
            _    => {}
        }

        journal_overlay::insert(fs, btree, level + 1, k.as_bkey_i())?;
    }

    if *nodes_found != 0 {
        out.indent_sub(2);
    }
    Ok(())
}

#[c_export]
pub fn bch2_get_scanned_nodes(c: &Opaque<Fs>, btree: Btree, level: c_uint, node_min: Bpos, node_max: Bpos,
                              out: &mut Printbuf, nodes_found: &mut usize) -> c_int {
    ret_to_c(get_scanned_nodes(Fs::from_c(c), btree, level, node_min, node_max, out, nodes_found))
}

#[c_export]
pub fn bch2_scan_for_btree_nodes(c: &Opaque<Fs>) -> c_int {
    ret_to_c(scan_for_btree_nodes(Fs::from_c(c)))
}
