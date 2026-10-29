// SPDX-License-Identifier: GPL-2.0
// The kernel build's bindgen: what fs/'s Rust uses of the kernel that
// kernel::bindings doesn't bind - the kernel-side counterpart of bcachefs-shim.
// Never bcachefs's own headers: they include the *_gen.h the Rust build
// generates, so binding them would make it depend on its own output. What's
// bound is kernel headers, carved out of the include trees' blocklist one by
// one, and vendored library code (the closures).
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
    "linux/mm.h",			// totalram_pages(), a backpointer scan's batch limit
    "linux/mount.h",			// mnt_want_write_file(), vfs/ioctl.rs's
    "linux/blkdev.h",			// bdev_freeze(), vfs/ioctl.rs's goingdown
    "linux/statfs.h",			// struct kstatfs, uuid_to_fsid(): vfs/super_block.rs's statfs
    "linux/fileattr.h",			// struct file_kattr, fileattr_fill_xflags(): vfs/attr.rs's
    "linux/compat.h",			// in_compat_syscall(): vfs/dir.rs's 32-bit readdir
    "linux/exportfs.h",			// struct export_operations, enum fid_type: vfs/export.rs's
    "linux/fiemap.h",			// fiemap_prep(), fiemap_fill_next_extent(): vfs/fiemap.rs's
    "linux/namei.h",			// the path lookups: vfs/ioctl.rs's subvolume create and destroy
    "linux/fsnotify.h",			// fsnotify_mkdir(), fsnotify_rmdir(): the same
    "linux/security.h",			// security_path_mkdir(): subvolume create
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
    "linux/fs_context.h",		// struct fs_context, logfc(): util/fs_context.rs's - the kernel crate's is opaque
];

/// Headers under fs/ bound, by path - which the clang args have as an -I:
/// vendored library code, and codegen.h's wrappers of kernel macros.
const VENDORED_HEADERS: &[&str] = &["vendor/closure.h", "codegen.h"];

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
    "linux/mount.h",
    "linux/blkdev.h",
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
    "linux/fs_context.h",
];

const ALLOWLIST_FUNCTION: &[&str] = &[
    "crc32c", "crc64_be", "sha256", "anon_inode_getfile",
    "mnt_want_write_file", "mnt_drop_write_file", "bdev_freeze", "bdev_thaw",
    "__genradix_ptr", "__genradix_ptr_alloc", "__genradix_iter_peek", "__genradix_free",
    "closure_get", "closure_get_not_zero", "closure_put", "closure_init_stack", "closure_sync", "closure_call",
    "closure_sync_timeout",
    "bch2_closure_wait", "closure_wake_up", "closure_nr_remaining",
    "rust_page_folio", "rust_folio_page_idx", "rust_folio_size",
    "rust_page_highmem", "rust_page_address",
    "rust_refcount_inc_not_zero", "rust_poll_wait", "rust_fault_in_writeable",
    "rust_fault_in_readable", "rust_filemap_write_and_wait_range", "rust_filemap_flush",
    "totalram_pages", "set_freezable", "freezing", "try_to_freeze",
    "i_uid_write", "i_gid_write", "i_size_read", "i_size_write", "uuid_to_fsid",
    "fileattr_fill_xflags", "in_compat_syscall", "dir_emit_dots", "finish_open_simple", "file_accessed",
    "i_user_ns", "inode_get_atime", "inode_get_mtime", "inode_get_ctime",
    "inode_set_atime_to_ts", "inode_set_mtime_to_ts", "inode_set_ctime_to_ts",
    "posix_acl_alloc", "posix_acl_dup", "posix_acl_release", "__posix_acl_chmod",
    "posix_acl_update_mode", "set_cached_acl",
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
    "rust_vmap", "rust_vunmap",
    "xxh64_reset", "xxh64_update", "xxh64_digest",
    "chacha_init", "chacha20_crypt", "chacha_zeroize_state",
    "poly1305_init", "poly1305_update", "poly1305_final",
    "rust_request_user_key", "rust_user_key_data",
    // util/fs_context.rs and util/super_block.rs: static inlines, and the
    // log behind errorfc() and invalf()
    "logfc", "super_set_uuid", "super_set_sysfs_name_uuid",
];
const ALLOWLIST_TYPE: &[&str] = &["closure", "closure_waitlist", "__genradix", "genradix_iter",
                                  "xattr_handler", "kstatfs", "export_operations", "fid_type",
                                  "xxh64_state", "chacha_state", "poly1305_desc_ctx",
                                  // the kernel crate's fs_context and fs_parameter are opaque
                                  "fs_context", "fs_parameter", "fs_context_operations"];
/// Structs in an unblocklisted header - defined there, or only declared, as
/// uidgid.h declares user_namespace - that kernel::bindings binds: a second
/// copy would be a different type - and bindgen, stopped at one, doesn't
/// follow its fields into the rest of the header.
const BLOCKLIST_TYPE: &[&str] = &["inode", "file", "dir_context", "user_namespace", "kqid",
                                     "fiemap_extent_info", "address_space", "vm_fault",
                                     "vm_operations_struct", "vm_area_struct", "vm_area_desc", "kiocb",
                                     "super_block", "super_operations"];
const ALLOWLIST_VAR: &[&str] = &["GENRADIX_NODE_SIZE", "nop_posix_acl_access", "nop_posix_acl_default",
                                 "ACL_USER", "ACL_GROUP", "LOOKUP_FOLLOW", "FIEMAP_EXTENT_.*",
                                 "GOLDEN_RATIO_64", "RUST_CONST_HELPER_.*",
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
