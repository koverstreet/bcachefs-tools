// SPDX-License-Identifier: GPL-2.0
// The kernel build's bindgen: what fs/'s Rust uses of the kernel that
// kernel::bindings doesn't bind - the kernel-side counterpart of bcachefs-shim.
// What's bound is kernel headers, carved out of the include trees' blocklist
// one by one, vendored library code (the closures), and the headers of fs/'s
// C shims.
//
// codegen_main.rs, which the kernel/DKMS Makefile builds and runs, include!s
// this file - so plain // comments. It drives the bindgen CLI: no crates to
// vendor, no target/ in the kernel build tree.

use std::process::Command;

/// The kernel headers bound: each must be in UNBLOCKLISTED too. The CRCs are
/// static inlines in 6.16, the oldest kernel the module builds against, so
/// they're wrapped (extern.c); from 6.17, exported functions.
const KERNEL_HEADERS: &[&str] = &[
    "linux/crc32.h",			// crc32c(), a string hash
    "linux/crc64.h",			// crc64_be(), a string hash
    "linux/generic-radix-tree.h",	// GenRadix<T>'s __genradix
    "linux/xattr.h",			// xattr_handler, which fs.h only declares
    "linux/posix_acl_xattr.h",		// nop_posix_acl_*, xattr.rs's ACL handlers
    "linux/posix_acl.h",		// struct posix_acl, which fs.h only declares: fs/acl.rs's PosixAcl
    "linux/uidgid.h",			// make_kuid() and the rest, an ACL entry's ids
    "linux/quota.h",			// make_kqid(), quota_send_warning(): quota.rs's
    "crypto/sha2.h",			// sha256(), siphash_old's string hash key
    "linux/anon_inodes.h",		// anon_inode_getfile(), util/file's NewFile
    "linux/hash.h",			// GOLDEN_RATIO_64, util/hash.rs's hash_64()
    "linux/mm.h",			// totalram_pages(), a backpointer scan's batch limit;
					// si_mem_available(), the node cache's memory pressure
    "linux/mount.h",			// mnt_want_write_file(), vfs/ioctl.rs's
    "linux/statfs.h",			// struct kstatfs, uuid_to_fsid(): vfs/super_block.rs's statfs
    "linux/fileattr.h",			// struct file_kattr, fileattr_fill_xflags(): vfs/attr.rs's
    "linux/compat.h",			// in_compat_syscall(): vfs/dir.rs's 32-bit readdir
    "linux/exportfs.h",			// struct export_operations, enum fid_type: vfs/export.rs's
    "linux/fiemap.h",			// fiemap_prep(), fiemap_fill_next_extent(): vfs/fiemap.rs's
    "linux/namei.h",			// the path lookups: vfs/ioctl.rs's subvolume create and destroy
    "linux/fsnotify.h",			// fsnotify_mkdir(), fsnotify_rmdir(): the same
    "linux/security.h",			// security_path_mkdir(): subvolume create; security_inode_init_security(): create
    "linux/fs_struct.h",		// current_umask(): subvolume create
    "linux/freezer.h",			// set_freezable(), freezing(), try_to_freeze(): a freezable kthread's
    "linux/power_supply.h",		// power_supply_reg_notifier(), reconcile on battery
    "linux/raid/pq.h",			// raid6_gen_syndrome(), a stripe's Q
    "linux/raid/xor.h",			// xor_gen(), a stripe's P
    "linux/lz4.h",			// data/compress.rs's lz4
    "linux/zlib.h",			// its gzip
    "linux/zstd.h",			// its zstd
    "linux/xxhash.h",			// data/checksum.rs's xxhash
    "crypto/chacha.h",			// its encryption
    "crypto/poly1305.h",		// and MACs
    "linux/fs_context.h",		// struct fs_context, logfc(): os/linux_kernel/fs_context.rs's - the kernel crate's is opaque
    "linux/percpu-refcount.h",		// percpu_ref_get(), _put(): a DevRef's ca->ref, static inlines
    "linux/idr.h",			// ida_init(), ida_find_first(): fast_list.rs's slots - static inlines
    "linux/console.h",			// console_trylock(): util/log.rs holds the console across a message
    "linux/bio.h",			// bio_set_dev(), util/bio.rs's; bio_iov_vecs_to_alloc(),
					// os/linux_kernel/iov_iter.rs's; bio_release_pages(),
					// os/linux_kernel/user_pages.rs's - static inlines
    "linux/task_io_accounting_ops.h",	// task_io_account_write(): vfs/direct_write.rs's
    "linux/kthread.h",			// kthread_use_mm(): an async DIO write's continuation
    "linux/swap.h",			// mm_account_reclaimed_pages(): btree/cache.rs's node buffers
    "linux/vmalloc.h",			// __vmalloc_noprof(): the same
    "linux/trace_events.h",		// struct trace_event_call and the rest: debug/trace.rs's
					// tracepoints' events, and what their probes call
];

/// Headers under fs/ bound, by path - which the clang args have as an -I:
/// vendored library code, and codegen.h's wrappers of kernel macros.
const VENDORED_HEADERS: &[&str] = &["vendor/closure.h", "vendor/bio_iov_iter.h",
                                    "locking/shim.h", "util/shim.h", "codegen.h"];

/// Under the kernel include trees, what isn't blocklisted: uapi/ (see
/// blocklist_dir()), and KERNEL_HEADERS.
const UNBLOCKLISTED: &[&str] = &[
    "uapi/",
    "linux/crc32.h",
    "linux/crc64.h",
    "linux/generic-radix-tree.h",
    "linux/xattr.h",
    "linux/posix_acl_xattr.h",
    "linux/posix_acl.h",
    "linux/uidgid.h",
    "linux/quota.h",
    "crypto/sha2.h",
    "linux/anon_inodes.h",
    "linux/hash.h",
    "linux/mm.h",
    "linux/fs.h",
    "linux/fs/super.h",			// sb_start_pagefault(), from 7.1
    "linux/mount.h",
    "linux/statfs.h",
    "linux/fileattr.h",
    "linux/compat.h",
    "linux/exportfs.h",
    "linux/fiemap.h",
    "linux/namei.h",
    "linux/fsnotify.h",
    "linux/security.h",
    "linux/fs_struct.h",
    "asm/compat.h",			// in_compat_syscall(), the arch's own
    "linux/freezer.h",
    "linux/power_supply.h",
    "linux/raid/pq.h",
    "linux/raid/xor.h",
    "linux/lz4.h",
    "linux/zlib.h",
    "linux/zconf.h",			// zlib.h's: MAX_WBITS, DEF_MEM_LEVEL
    "linux/zstd.h",
    "linux/zstd_lib.h",			// zstd.h's types: zstd_parameters is ZSTD_parameters
    "linux/zstd_errors.h",		// and zstd_error_code ZSTD_ErrorCode
    "linux/xxhash.h",
    "crypto/chacha.h",
    "crypto/poly1305.h",
    "linux/percpu-refcount.h",
    "linux/fs_context.h",
    "linux/idr.h",
    "linux/console.h",
    "linux/bio.h",
    "linux/task_io_accounting_ops.h",
    "linux/kthread.h",
    "linux/swap.h",
    "linux/vmalloc.h",
    "linux/writeback.h",
    "linux/pagemap.h",
    "linux/pagevec.h",			// struct folio_batch, before 7.1
    "linux/folio_batch.h",
    "linux/page-flags.h",
    "linux/xarray.h",
    "linux/highmem.h",			// folio_zero_segment(), folio_zero_range(), folio_zero_segments()
    "linux/cacheflush.h",		// flush_dcache_folio(): a no-op, or the arch's
    "asm/cacheflush.h",
    "linux/rmap.h",			// folio_mkclean()
    "linux/mm_types.h",			// folio_get_private()
    "linux/key.h",			// request_key()
    "keys/user-type.h",			// key_type_user
    "linux/srcu.h",			// srcu_read_lock(), srcu_read_unlock()
    "linux/srcutree.h",			// synchronize_srcu_expedited(): an export,
    "linux/srcutiny.h",			// or with TINY_SRCU a static inline
    // os/linux_kernel/kernel.rs, kthread.rs, workqueue.rs and sched.rs: exports and
    // static inlines, the shim's functions in userspace
    "linux/alloc_tag.h",		// mem_alloc_profiling_enabled()
    "linux/sched/clock.h",		// local_clock(), sched_clock()
    "linux/delay.h",			// msleep()
    "linux/timekeeping.h",		// ktime_get_ns()
    "linux/random.h",			// get_random_u64()
    "linux/workqueue.h",		// queue_work(), queue_delayed_work()
    "linux/cpumask.h",			// cpumask_local_spread()
    "linux/numa.h",			// NUMA_NO_NODE
    "linux/trace_events.h",
    "linux/trace_seq.h",		// trace_events.h's struct trace_iterator holds them
    "linux/seq_buf.h",
    "linux/ring_buffer.h",		// and its structs point to these
];

const ALLOWLIST_FUNCTION: &[&str] = &[
    "crc32c", "crc64_be", "sha256", "anon_inode_getfile",
    "mnt_want_write_file", "mnt_drop_write_file",
    "__genradix_ptr", "__genradix_ptr_alloc", "__genradix_iter_peek", "__genradix_iter_peek_prev", "__genradix_free",
    "closure_get", "closure_get_not_zero", "closure_put", "closure_init", "closure_init_stack", "closure_sync", "closure_call",
    "closure_init_stack_release", "bch2_closure_return_sync",
    "closure_sync_timeout",
    "bch2_closure_wait", "closure_wake_up", "closure_nr_remaining",
    "set_closure_fn", "closure_queue", "bch2_closure_sub", "closure_debug_destroy",
    "closure_set_waiting", "closure_val_checks",
    "rust_page_folio", "rust_folio_page_idx", "folio_size",
    // vfs/folio.rs: folios, mappings and readahead
    "folio_get_private", "rust_filemap_alloc_folio", "folio_pos", "folio_nr_pages",
    "folio_get", "folio_put", "folio_detach_private", "folio_test_uptodate", "folio_test_dirty",
    "folio_mark_uptodate", "folio_end_read", "filemap_add_folio", "readahead_gfp_mask",
    "mapping_min_folio_order", "mapping_max_folio_order", "__readahead_folio", "xa_load", "xa_is_value",
    "bio_add_folio_nofail", "folio_unlock", "filemap_lock_folio",
    "folio_zero_segment", "folio_zero_range", "folio_zero_segments", "flush_dcache_folio",
    "mapping_writably_mapped", "mapping_gfp_mask",
    "folio_mkclean", "filemap_dirty_folio",
    "folio_lock", "folio_trylock", "filemap_get_folios",
    "folio_batch_init", "folio_batch_count", "folio_batch_release",
    // vfs/aops.rs: the kernel's, in an inode's table
    "filemap_migrate_folio", "generic_error_remove_folio",
    // os/linux_kernel/user_pages.rs
    "folio_mark_dirty",
    // vfs/pagecache.rs
    "__filemap_get_folio", "folio_attach_private", "folio_test_writeback", "folio_wait_stable",
    "rust_folio_mapping", "folio_test_locked", "invalidate_inode_pages2_range",
    "sb_start_pagefault", "sb_end_pagefault",
    "rust_page_highmem", "rust_page_address", "rust_is_sync",
    "fault_in_writeable",
    "fault_in_readable", "filemap_write_and_wait_range", "filemap_flush",
    "totalram_pages", "si_mem_available", "set_freezable", "freezing", "try_to_freeze",
    "bio_set_dev", "bio_iov_vecs_to_alloc", "bio_release_pages",
    // util/bio/owned.rs
    "bio_advance", "bio_inc_remaining",
    // vfs/direct_write.rs: static inlines, and the VFS's write checks
    "inode_dio_begin", "inode_dio_end", "iocb_is_dsync", "generic_write_checks", "generic_write_sync",
    "file_remove_privs",
    "task_io_account_write", "kthread_use_mm", "kthread_unuse_mm",
    // vfs/writeback.rs
    "writeback_iter", "wbc_to_write_flags", "rust_folio_start_writeback", "folio_end_writeback",
    "mapping_set_error",
    "i_uid_write", "i_gid_write", "i_size_read", "i_size_write", "uuid_to_fsid",
    "fileattr_fill_xflags", "in_compat_syscall", "dir_emit_dots", "finish_open_simple", "file_accessed",
    "i_user_ns", "inode_get_atime", "inode_get_mtime", "inode_get_ctime",
    "inode_set_atime_to_ts", "inode_set_mtime_to_ts", "inode_set_ctime_to_ts",
    "posix_acl_alloc", "posix_acl_dup", "posix_acl_release", "__posix_acl_chmod",
    "posix_acl_update_mode", "set_cached_acl", "posix_acl_create",
    // vfs/namei.rs's create
    "security_inode_init_security",
    // from_kuid() the kernel crate has
    "make_kuid", "make_kgid", "from_kgid",
    // a static inline; and a static inline without CONFIG_QUOTA_NETLINK_INTERFACE
    "make_kqid", "quota_send_warning",
    // the path lookups, as each kernel has them: before 6.18, from 6.18, from 7.2
    "user_path_at", "user_path_create", "done_path_create", "user_path_locked_at",
    "start_creating_user_path", "end_creating_path", "start_removing_user_path_at",
    "end_removing_path", "start_removing_dentry",
    "fsnotify_mkdir", "fsnotify_rmdir", "security_path_mkdir", "current_umask",
    "kuid_has_mapping", "kgid_has_mapping", "init_sync_kiocb", "inode_lock", "inode_unlock",
    "rust_compat_ptr", "fiemap_prep", "fiemap_fill_next_extent",
    "power_supply_reg_notifier", "power_supply_unreg_notifier", "power_supply_is_system_supplied",
    // the raid6 and xor APIs on both sides of 7.1's and 7.2's renames - each
    // kernel has one set: data/ec/io.rs picks by cfg(linux_ge)
    "raid6_gen_syndrome", "raid6_recov_2data", "raid6_recov_datap", "xor_gen", "xor_blocks",
    "LZ4_decompress_safe_partial", "LZ4_compress_destSize", "LZ4_compress_HC",
    "zlib_inflate_workspacesize", "zlib_inflateInit2", "zlib_inflate", "zlib_inflateEnd",
    "zlib_deflate_workspacesize", "zlib_deflateInit2", "zlib_deflate", "zlib_deflateEnd",
    "zstd_max_clevel", "zstd_get_params", "zstd_is_error", "zstd_get_error_code",
    "zstd_cctx_workspace_bound", "zstd_init_cctx", "zstd_compress_cctx",
    "zstd_dctx_workspace_bound", "zstd_init_dctx", "zstd_decompress_dctx",
    "rust_vmap", "vunmap",
    "xxh64_reset", "xxh64_update", "xxh64_digest",
    "chacha_init", "chacha20_crypt",
    "poly1305_init", "poly1305_update", "poly1305_final",
    "request_key", "rust_user_key_data",
    // os/linux_kernel/fs_context.rs and super_block.rs: static inlines, and the
    // log behind errorfc() and invalf()
    "logfc", "super_set_uuid", "super_set_sysfs_name_uuid",
    // static inlines: util/refcount/percpu.rs's PercpuRef
    "percpu_ref_get", "percpu_ref_put", "percpu_ref_tryget", "percpu_ref_tryget_live", "percpu_ref_kill",
    "percpu_ref_reinit", "percpu_ref_is_zero",
    // data_structures/fast_list.rs: static inlines; the rest of the ida the kernel crate binds
    "ida_init", "ida_find_first",
    // util/log.rs: a long message printk()ed in pieces, the console held across them
    "console_trylock", "console_unlock",
    // util/rcu.rs's Srcu: static inlines, and synchronize_srcu_expedited() is with TINY_SRCU
    "srcu_read_lock", "srcu_read_unlock", "synchronize_srcu_expedited",
    // os/linux_kernel/kernel.rs, kthread.rs, workqueue.rs and sched.rs
    "mem_alloc_profiling_enabled", "local_clock", "msleep", "ktime_get_ns", "get_random_u64",
    "queue_work", "queue_delayed_work", "kthread_stop", "sched_clock", "cpumask_local_spread",
    // vfs/vfs_inode.rs, for the inode cache: static inlines, inode_state_*() from 6.19
    "inode_state_read", "__iget", "inode_fake_hash", "inode_wake_up_bit",
    "inode_sb_list_add", "inode_bit_waitqueue",
    // vfs/vfs_inode.rs, a new inode's: a static inline, and the exports
    "mapping_set_folio_min_order", "inode_init_once", "inode_init_always_gfp", "make_bad_inode",
    "init_special_inode", "inode_nohighmem", "igrab", "write_inode_now",
    // btree/cache.rs's node buffers
    "mm_account_reclaimed_pages", "__vmalloc_noprof",
    // vfs/vfs_inode.rs, an evicted inode's: a static inline from 6.19, and the exports
    "inode_state_read_once", "is_bad_inode", "truncate_inode_pages_final", "d_mark_dontcache",
    "clear_inode",
    // vfs/buffered_write.rs: a static inline
    "fgf_set_order",
    // debug/trace.rs's tracepoints: exports, static inlines, and codegen.h's -
    // srcu's from 7.0
    "trace_event_reg", "trace_event_raw_init", "trace_trigger_soft_disabled",
    "trace_event_buffer_reserve", "trace_event_buffer_commit",
    "trace_raw_output_prep", "trace_event_printf", "trace_handle_return",
    "perf_trace_buf_alloc", "perf_trace_run_bpf_submit", "bpf_prog_array_valid", "bpf_trace_run2",
    "rust_srcu_read_lock_fast_notrace", "rust_srcu_read_unlock_fast_notrace",
    "rust_preempt_disable_notrace", "rust_preempt_enable_notrace", "rust_perf_fetch_caller_regs",
    // fs/'s C shims: locking.c, rcu.c, util.c, vendor/bio_iov_iter.c
    "rust_.*", "bch2_.*",
];
const ALLOWLIST_TYPE: &[&str] = &["closure", "closure_waitlist", "closure_state",
                                  "__genradix", "genradix_iter",
                                  "xattr_handler", "kstatfs", "export_operations", "fid_type",
                                  "xxh64_state", "chacha_state", "poly1305_desc_ctx",
                                  // the kernel crate's fs_context and fs_parameter are opaque
                                  "fs_context", "fs_parameter", "fs_context_operations",
                                  "readahead_control", "folio_batch",
                                  // debug/trace.rs's statics
                                  "trace_event_call", "trace_event_class", "trace_event_fields",
                                  "trace_event_functions", "trace_entry", "trace_event_buffer"];
/// Structs in an unblocklisted header - defined there, or only declared, as
/// uidgid.h declares user_namespace - that kernel::bindings binds: a second
/// copy would be a different type - and bindgen, stopped at one, doesn't
/// follow its fields into the rest of the header.
const BLOCKLIST_TYPE: &[&str] = &["inode", "file", "dir_context", "user_namespace", "kqid",
                                     "fiemap_extent_info", "address_space", "vm_fault",
                                     "vm_operations_struct", "vm_area_struct", "vm_area_desc", "kiocb",
                                     "super_block", "super_operations", "percpu_ref", "ida",
                                     "blk_holder_ops", "bio_set", "xarray", "writeback_control",
                                     "folio", "page", "mm_struct", "key", "key_type", "srcu_struct",
                                     "workqueue_struct", "work_struct", "delayed_work",
                                     // declared by numa.h's and mm.h's externs from 7.3
                                     "attribute_group",
                                     // trace_iterator's and trace_event_file's: declared in a
                                     // header bound here or in one blocklisted, as the kernel
                                     // includes them - which, before 7.0, kernel::bindings
                                     // doesn't bind. mod.rs's own, opaque
                                     "trace_array"];
/// Structs only ever had by pointer, their fields not bound: exportfs.h's
/// fid has a member named gen, a keyword in our edition that bindgen before
/// 0.71 doesn't escape.
const OPAQUE_TYPE: &[&str] = &["fid"];
const ALLOWLIST_VAR: &[&str] = &["GENRADIX_NODE_SIZE", "nop_posix_acl_access", "nop_posix_acl_default",
                                 "ACL_USER", "ACL_GROUP", "LOOKUP_FOLLOW", "FIEMAP_EXTENT_.*", "FALLOC_FL_.*",
                                 "GOLDEN_RATIO_64", "RUST_CONST_HELPER_.*", "key_type_user", "NUMA_NO_NODE",
                                 "PERCPU_REF_INIT_DEAD", "TRACE_EVENT_FL_TRACEPOINT", "FILTER_OTHER",
                                 // before 7.1/7.2, as above
                                 "raid6_call", "raid6_2data_recov", "raid6_datap_recov", "MAX_XOR_BLOCKS",
                                 "LZ4HC_MEM_COMPRESS", "LZ4HC_MIN_CLEVEL",
                                 "MAX_WBITS", "DEF_MEM_LEVEL", "Z_OK", "Z_STREAM_END", "Z_FINISH",
                                 "Z_DEFLATED", "Z_DEFAULT_STRATEGY", "Z_DEFAULT_COMPRESSION",
                                 "Z_BEST_SPEED", "Z_BEST_COMPRESSION"];

/// Run the bindgen CLI and write `bindings.rs`, and `extern.c` - C wrappers for
/// whatever of it is a static inline - into `out`. Kbuild supplies
/// `clang_args`, the C build's own, and `blocklist_dirs`, the kernel include
/// trees.
pub fn run_bindgen(out: &str, clang_args: &[String], blocklist_dirs: &[String]) {
    std::fs::create_dir_all(out).expect("create out dir");

    // bindgen CLI takes one header: a wrapper that #includes the set.
    let wrapper = format!("{out}/codegen-wrapper.h");
    let body: String = KERNEL_HEADERS.iter().map(|h| format!("#include <{h}>\n"))
        .chain(VENDORED_HEADERS.iter().map(|h| format!("#include \"{h}\"\n")))
        .collect();
    std::fs::write(&wrapper, body).expect("write wrapper header");

    // bindgen writes extern.c only when there's a static inline to wrap - which
    // depends on the kernel - so a previous build's goes first, or it's what
    // gets compiled.
    let extern_c = format!("{out}/extern.c");
    if let Err(e) = std::fs::remove_file(&extern_c) {
        if e.kind() != std::io::ErrorKind::NotFound {
            panic!("could not remove the previous build's {extern_c}: {e}");
        }
    }

    let mut a: Vec<String> = vec![wrapper.clone()];
    macro_rules! flag { ($f:expr, $v:expr) => {{ a.push($f.into()); a.push(String::from($v)); }}; }

    a.push("--formatter".into()); a.push("prettyplease".into());
    a.push("--with-derive-default".into());
    // Zeroable when every field is, as the kernel's own bindgen_parameters
    // and the shim's have it: a struct holding these - bch_fs, holding
    // closures - can start zeroed in a Rust initializer
    flag!("--with-derive-custom-struct", ".*=::pin_init::MaybeZeroable");
    flag!("--with-derive-custom-union", ".*=::pin_init::MaybeZeroable");
    a.push("--use-core".into());
    // C's char as the kernel compiles it, u8, as kernel::bindings has it -
    // not core's, i8 on x86: a fn pointer field taking char * has to take
    // what the Rust fns it points to do
    flag!("--ctypes-prefix", "crate::util::ffi");
    a.push("--generate-inline-functions".into());
    a.push("--wrap-static-fns".into());
    flag!("--wrap-static-fns-path", &extern_c);

    for x in ALLOWLIST_FUNCTION { flag!("--allowlist-function", *x); }
    for x in ALLOWLIST_TYPE     { flag!("--allowlist-type", *x); }
    for x in ALLOWLIST_VAR      { flag!("--allowlist-var", *x); }
    for x in BLOCKLIST_TYPE     { flag!("--blocklist-type", *x); }
    for x in OPAQUE_TYPE        { flag!("--opaque-type", *x); }
    for d in blocklist_dirs     { flag!("--blocklist-file", d.as_str()); }

    a.push("--".into());
    a.extend(clang_args.iter().cloned());

    let bindgen = std::env::var_os("BINDGEN").unwrap_or_else(|| "bindgen".into());
    let result = Command::new(&bindgen).args(&a).output().unwrap_or_else(|e| {
        panic!("could not run {bindgen:?}: {e}\n\
                bindgen generates the Rust bindings to the kernel APIs bcachefs uses \
                that the kernel's own don't cover: install it with `cargo install \
                bindgen-cli`, or set $BINDGEN to its path")
    });
    if !result.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&result.stderr));
        std::process::exit(1);
    }

    // codegen.h's consts, by the names of the macros they're of
    let mut bindings = String::from_utf8(result.stdout).expect("bindgen output utf8")
        .replace("pub const RUST_CONST_HELPER_", "pub const ");
    bindings += &opaque_arrays_zeroable(&bindings);
    bindings += bitfield_unit_zeroable(&bindings);
    std::fs::write(format!("{out}/bindings.rs"), bindings).expect("write bindings.rs");

    // None, or one with the wrapper's path baked into its #include: the bare
    // name, so the C compile finds it next to extern.c.
    let fixed = std::fs::read_to_string(&extern_c).unwrap_or_default()
        .replace(&format!("\"{wrapper}\""), "\"codegen-wrapper.h\"");
    std::fs::write(&extern_c, fixed).expect("write extern.c");
}

/// bindgen 0.73 stores an opaque blob - the storage of a union it can't make a
/// Rust union, the closure's - as __BindgenOpaqueArrayN<T>, aligned as the C,
/// where 0.72 had a [u64; N]. It's bindgen's own helper, so --with-derive-custom
/// doesn't reach it, and without Zeroable neither is a closure, nor bch_fs
/// holding one. The impls, for the ones this bindgen defined: 0.72 defines
/// none, and an impl of a type that isn't there doesn't compile. The union's
/// members are __BindgenUnionField, made Zeroable in mod.rs.
fn opaque_arrays_zeroable(bindings: &str) -> String {
    bindings.lines()
        .filter_map(|l| l.strip_prefix("pub struct __BindgenOpaqueArray"))
        .filter_map(|rest| rest.split('<').next())
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .map(|n| format!("\n// SAFETY: repr(C, align({n})) over a T: zeroed bytes are a zeroed T\n\
                          unsafe impl<T: ::pin_init::Zeroable> ::pin_init::Zeroable \
                          for __BindgenOpaqueArray{n}<T> {{}}\n"))
        .collect()
}

/// The same for a struct's bitfields, __BindgenBitfieldUnit - debug/trace's
/// trace_event_fields: bindgen defines it only for bindings that have one.
fn bitfield_unit_zeroable(bindings: &str) -> &'static str {
    if bindings.contains("pub struct __BindgenBitfieldUnit<") {
        "\n// SAFETY: its storage, bytes: zeroed, every bitfield is 0\n\
         unsafe impl<S: ::pin_init::Zeroable> ::pin_init::Zeroable for __BindgenBitfieldUnit<S> {}\n"
    } else {
        ""
    }
}

/// Escape regex metacharacters in a path, for --blocklist-file: unescaped, the
/// + in linux-headers-7.0.13+deb14-common quantifies, the match silently fails,
/// and kernel types leak into the bindings - a second struct mutex.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A --blocklist-file regex for everything under kernel include tree @dir but
/// UNBLOCKLISTED. The trees hold the kernel's own structs, which
/// kernel::bindings binds: a second copy would be a different type. uapi/ stays
/// visible: bindgen can't see across a blocklist that a pruned scalar typedef
/// is Copy, and structs holding one would lose Copy/Clone/Debug.
///
/// Path regexes because bindgen has nothing structural, no notion of a system
/// header. And --allowlist-file can't replace them: it makes every item in a
/// file a root, so --wrap-static-fns wraps every static inline in the tree, and
/// those using nested structs don't compile in extern.c.
fn blocklist_dir(dir: &str) -> String {
    format!("{}/{}", regex_escape(dir), not_prefixed(UNBLOCKLISTED))
}

/// A regex for any string that doesn't start with one of @prefixes. bindgen
/// matches with the `regex` crate, which has no lookahead, so it's spelled out
/// a character at a time: a character no prefix continues with matches,
/// whatever follows; one that some do continues down those - unless it
/// completes one.
fn not_prefixed(prefixes: &[&str]) -> String {
    let mut next: Vec<char> = prefixes.iter().filter_map(|p| p.chars().next()).collect();
    next.sort();
    next.dedup();

    // '-' last in the class, where it's literal rather than a range
    let class: String = next.iter().filter(|&&c| c != '-').map(|c| regex_escape(&c.to_string()))
        .chain(next.contains(&'-').then(|| "-".to_string()))
        .collect();
    let mut alts = vec![format!("[^{class}].*")];
    for c in next {
        let rest: Vec<&str> = prefixes.iter().filter_map(|p| p.strip_prefix(c)).collect();
        if rest.iter().any(|r| r.is_empty()) {
            continue;
        }
        alts.push(format!("{}{}", regex_escape(&c.to_string()), not_prefixed(&rest)));
    }
    format!("(?:{})", alts.join("|"))
}
