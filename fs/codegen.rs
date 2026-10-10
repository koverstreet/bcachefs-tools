// SPDX-License-Identifier: GPL-2.0
// Bindings codegen for bcachefs — shared logic, no `main`.
//
// This is the bindgen half of what the old `build.rs` did, but driven via the
// `bindgen` CLI rather than the bindgen *library* — no extra crates to vendor,
// and no `target/` litter in the kernel/distro build tree. Two entry points
// `include!` this file so both emit byte-identical bindings:
//   - `build.rs`        — userspace cargo build (computes args, then cc's the
//                         static-inline wrappers)
//   - `codegen_main.rs` — the standalone tool the kernel/DKMS Makefile runs
//
// The split is: this file turns headers + clang args into Rust; the callers
// own *where the clang args come from* (userspace computes them; Kbuild passes
// them) and *what happens to `extern.c`* (cc + link vs. a normal C object).
//
// This file is `include!`d, never compiled as its own crate root — hence plain
// `//` comments here, not `//!`.

use std::process::Command;

include!("build_config.rs");

const HEADERS: &[&str] = &[
    "bcachefs.h", "opts.h",
    "btree/cache.h", "btree/interior.h", "btree/iter.h", "btree/read.h", "btree/write_buffer.h",
    "alloc/accounting.h", "alloc/background.h", "alloc/buckets.h", "alloc/disk_groups.h",
    "data/checksum.h", "data/ec/trigger.h", "data/extents.h", "data/io_misc.h", "data/move.h", "data/read.h", "data/update.h", "data/write.h",
    "debug/debug.h",
    "init/damage.h", "init/dev.h", "init/error.h", "init/fs.h", "init/passes.h", "init/recovery.h",
    "fs/acl.h", "fs/check.h", "fs/dirent.h", "fs/inode.h", "fs/inode_opts.h", "fs/namei.h", "fs/xattr.h",
    "data/reconcile/work.h", "vfs/rust.h",
    "journal/init.h", "journal/read.h", "journal/reclaim.h", "journal/seq_blacklist.h", "journal/validate.h",
    "sb/io.h", "sb/members.h",
    "util/varint.h",
];

/// The kernel API headers for the CRCs the string hash uses: bound from the
/// kernel's own in the kernel build (KERNEL_UNBLOCKLISTED). In userspace they
/// stay blocklisted, and bcachefs-shim binds them.
const CRC_HEADERS: &[&str] = &["linux/crc32c.h", "linux/crc64.h"];

// Translated 1:1 from the bindgen builder calls in build.rs.
const ALLOWLIST_FUNCTION: &[&str] = &[
    // rust_* are C shims that exist solely for Rust to call (e.g. util/locking.h
    // wraps the memalloc_flags_* static inlines, which don't reach Rust). Same
    // convention as bch_bindgen's allowlist.
    ".*bch2_.*", "rust_.*", "block_bytes", "match_string", "printbuf.*", "_bch2_err_matches",
    "bpos_.*", "bkey_init", "bkey_.*_init", "bkey_i_to_s", "bkey_i_to_s_c",
    "btree_iter_path", "extent_entry_u64s", "enumerated_ref_tryget", "enumerated_ref_put",
    "journal_cur_seq",
    "prt_bytes",
    "bkey_extent_is_allocation", "bkey_extent_is_reservation", "crc_is_encoded",
    "crc32c", "crc64_be", "SipHash_.*",
    // crypto helpers for the dump sanitize path (static inlines, not
    // bch2_-prefixed): nonce constructors + bset_encrypt, driven from Rust
    // over the already-wrapped bch2_checksum / bch2_encrypt.
    "journal_nonce", "btree_nonce", "bset_encrypt",
    // hand-rolled error-entry field accessors (sb/errors_format.h): the
    // damage command unpacks BCHFS_IOC_GET_DAMAGE entries through them.
    "BCH_SB_ERROR_ENTRY_V2_.*",
];
const BLOCKLIST_FUNCTION: &[&str] = &[
    "bch2_prt_vprintf",
    // A static inline in fs/vendor/closure.h that .*bch2_.* catches: the
    // closures are bcachefs-shim's, which wraps it too - two
    // bch2_closure_debug_create__extern don't link.
    "bch2_closure_debug_create",
];
const BLOCKLIST_TYPE: &[&str] = &["bch_ioctl_data_event", "bch_replicas_padded__bindgen_ty_.*"];
const BLOCKLIST_ITEM: &[&str] = &["bch2_bkey_ops"];
const ALLOWLIST_VAR: &[&str] = &["BCH_.*", "BTREE_MAX_DEPTH", "BTREE_TRANS_MEM_MAX", "KEY_FORMAT_CURRENT", "KEY_SPEC_.*", "bch.*", "__bch2.*", "__BTREE_ITER.*", "BTREE_ITER.*",
    // bcachefs's own dirent type, alongside the kernel's DT_*:
    "DT_SUBVOL",
    // errnos, for bch2_err_matches() against a bare errno, or returning one
    // as C does (darray_push()'s -ENOMEM); add as needed:
    "ENOENT", "ENOMEM", "EINVAL", "ENAMETOOLONG", "ERANGE", "ENODATA", "E2BIG", "EACCES",
    "ECHILD", "EROFS", "EXDEV", "ENOTDIR", "EIO", "ENOTEMPTY",
    "BCACHEFS_ROOT_SUBVOL", "BCACHEFS_ROOT_INO",
    "BLOCKDEV_INODE_MAX", "INODEv3_FIELDS_START_INITIAL",
    "KEY_TYPE_XATTR_INDEX_.*"];
const ALLOWLIST_TYPE: &[&str] = &["bch_.*", "bkey_i_.*", "bkey_s_c_.*", "bkey_s_.*", "btree_flags", "disk_accounting_type", "fsck_err_opts", "nonce", "sb_names",
    // the logged_ops btree's reserved inode numbers - inode alloc cursors
    // live at one:
    "logged_ops_inums",
    // xattr lookups take it through a void *, so nothing else pulls it in:
    "xattr_search_key",
    // genradix: kernel::bindings doesn't bind it, so we emit it ourselves from a
    // build-time copy of the kernel header (see run_bindgen + fs/Makefile).
    "genradix.*", "__genradix.*"];
const BITFIELD_ENUM: &[&str] = &[
    "bch_fsck_flags",
    "btree_iter_update_trigger_flags",
    "bch_reservation_flags",
    "bch_run_recovery_pass_flags",
    "bch_trans_commit_flags",
    "bch_validate_flags",
    "bch_write_flags",
];
const RUSTIFIED_ENUM: &[&str] = &["fsck_err_opts", "bch_key_types"];
// The open enums - a value can come from disk, an ioctl or userspace, so any
// integer is one: a newtype with a const for each value, as the conversion
// makes them (#[open]). The rest are Rust enums, the default style.
const NEWTYPE_ENUM: &[&str] = &[
    "__bch_inode_flags",
    "bcachefs_metadata_version",
    "bch_bkey_type",
    "bch_compression_opts",
    "bch_compression_type",
    "bch_csum_opt",
    "bch_csum_type",
    "bch_data_event",
    "bch_data_ops",
    "bch_data_type",
    "bch_degraded_actions",
    "bch_errcode",
    "bch_error_actions",
    "bch_extent_entry_type",
    "bch_extent_flags_e",
    "bch_fs_usage_type",
    "bch_ioctl_data_event_ret",
    "bch_iops_measurement",
    "bch_jset_entry_type",
    "bch_kdf_types",
    "bch_key_type_errors",
    "bch_lru_type",
    "bch_member_error_type",
    "bch_member_initialized",
    "bch_member_state",
    "bch_opt_id",
    "bch_persistent_counters_stable",
    "bch_progress_units",
    "bch_reconcile_accounting_type",
    "bch_reconcile_opts",
    "bch_recovery_pass_stable",
    "bch_sb_compat",
    "bch_sb_error_id",
    "bch_sb_feature",
    "bch_sb_field_type",
    "bch_scrub_journal_opts",
    "bch_snapshot_state",
    "bch_str_hash_opts",
    "bch_str_hash_type",
    "bch_subvolume_state",
    "bch_version_upgrade_opts",
    "bch_write_degraded_actions",
    "btree_id",
    "data_progress_data_type_special",
    "disk_accounting_type",
    "inode_opt_id",
    "logged_op_finsert_state",
    "logged_ops_inums",
    "quota_counters",
    "quota_types",
    "reconcile_work_id",
];
const OPAQUE_TYPE: &[&str] = &["gendisk", "gc_stripe", "open_bucket.*", "replicas_delta_list", "bch_replicas_padded"];
const NO_DEBUG: &[&str] = &["bch_replicas_padded", "jset", "bch_replicas_entry_cpu"];
const NO_COPY: &[&str] = &["btree_trans", "printbuf", "bch_sb_handle"];
const NO_PARTIALEQ: &[&str] = &["bkey", "bpos"];

// The format structs the Rust code (fs/ and the tools binary) handles *by
// value* — copying, defaulting, comparing them. bindgen drops Copy/Clone/Debug
// from any struct with a blocklisted-primitive member (it can't see across the
// blocklist that `u64`/`__le64`/… are Copy), so re-add them here. Every struct
// listed has all-primitive fields, so this doesn't cascade — unlike the extent
// structs, whose union members are reached via `.as_ref()` instead (see
// post_process and fs/data/extents.rs). `Default`, where needed, already comes
// from bindgen's manual `impl Default`, so we add only Debug/Copy/Clone.
//
// This is the explicit Rust↔C value interface; it grows only when Rust starts
// handling a new format struct by value (you'll get an E0382/E0507 if so).
const DERIVE_READD: &[&str] = &[
    "bpos", "bbpos",
    "subvol_inum", "bch_opts",
    "bch_inode_unpacked", "u96",
    "bch_hash_desc",
    "bch_ioctl_snapshot_node",
    "bch_ioctl_snapshot_node_v2",
];

// bch_key/bch_encrypted_key hold key material: they get a hand-written Clone
// and a zeroize-on-drop (see mod.rs), so they must NOT derive Copy (a Drop type
// can't be Copy) and we don't want a derived Debug leaking key bytes. Kept out
// of DERIVE_READD deliberately.

/// Run the bindgen CLI over the fs/ headers and write `bcachefs.rs` + `extern.c`
/// into `out`. The caller supplies `clang_args` and `blocklist_dirs` — userspace
/// computes them via [`userspace_clang_args`]/[`default_blocklist`], the kernel
/// build passes Kbuild's set.
pub fn run_bindgen(out: &str, clang_args: &[String], blocklist_dirs: &[String], ptr_width: &str) {
    std::fs::create_dir_all(out).expect("create out dir");

    // bindgen CLI takes one header; emit a wrapper that #includes the fs/ set.
    let wrapper = format!("{out}/codegen-wrapper.h");
    let mut body = String::new();
    // Kernel build only: the kernel's generic-radix-tree.h is copied into the
    // *build* dir ({out}) by a make rule because kernel::bindings doesn't bind
    // genradix. Include it *first* so its include-guard wins over the bcachefs
    // headers' blocklisted <linux/generic-radix-tree.h> — that makes genradix
    // emit from this non-blocklisted copy. The wrapper lives in {out}, so the
    // quote-include resolves to it there. Absent in userspace (genradix comes
    // from the shim), so the check is false and it's skipped.
    if std::path::Path::new(&format!("{out}/generic-radix-tree.h")).exists() {
        body.push_str("#include \"generic-radix-tree.h\"\n");
    }
    body.push_str(&HEADERS.iter().map(|h| format!("#include \"{h}\"\n")).collect::<String>());
    body.push_str(&CRC_HEADERS.iter().map(|h| format!("#include <{h}>\n")).collect::<String>());
    std::fs::write(&wrapper, body).expect("write wrapper header");

    let mut a: Vec<String> = vec![wrapper.clone()];
    macro_rules! flag { ($f:expr, $v:expr) => {{ a.push($f.into()); a.push(String::from($v)); }}; }

    a.push("--formatter".into()); a.push("prettyplease".into());
    a.push("--with-derive-default".into());
    a.push("--use-core".into());
    flag!("--default-enum-style", "rust_non_exhaustive");
    a.push("--generate-inline-functions".into());
    a.push("--wrap-static-fns".into());
    // Key material: bch_key/bch_encrypted_key get a hand-written zeroize-on-drop
    // (see mod.rs), so they must not derive Copy (a Drop type can't be Copy).
    flag!("--no-copy", "bch_key");
    flag!("--no-copy", "bch_encrypted_key");
    flag!("--wrap-static-fns-path", format!("{out}/extern.c"));
    // Runtime type information (fs/typeinfo.rs): inject #[derive(TypeInfo)] on
    // the bch_* family so field names/offsets/endianness are available at
    // runtime — the btree REPL's field-level get/set. Unions and enums degrade
    // to size-only entries, but must still be derived: they appear as field
    // types inside derived structs. The regex must stay in sync with
    // derives_type_info() in typeinfo-macros/src/lib.rs.
    for f in ["--with-derive-custom-struct",
              "--with-derive-custom-union",
              "--with-derive-custom-enum"] {
        flag!(f, "(bch_.*|bpos|bkey|bversion)=TypeInfo");
    }

    for x in BITFIELD_ENUM      { flag!("--bitfield-enum", *x); }
    for x in RUSTIFIED_ENUM     { flag!("--rustified-enum", *x); }
    for x in NEWTYPE_ENUM       { flag!("--newtype-enum", *x); }
    for x in OPAQUE_TYPE        { flag!("--opaque-type", *x); }
    for x in ALLOWLIST_FUNCTION { flag!("--allowlist-function", *x); }
    for x in BLOCKLIST_FUNCTION { flag!("--blocklist-function", *x); }
    for x in ALLOWLIST_VAR      { flag!("--allowlist-var", *x); }
    for x in ALLOWLIST_TYPE     { flag!("--allowlist-type", *x); }
    for x in BLOCKLIST_TYPE     { flag!("--blocklist-type", *x); }
    for x in BLOCKLIST_ITEM     { flag!("--blocklist-item", *x); }
    for x in NO_DEBUG           { flag!("--no-debug", *x); }
    for x in NO_COPY            { flag!("--no-copy", *x); }
    for x in NO_PARTIALEQ       { flag!("--no-partialeq", *x); }
    for d in blocklist_dirs     { flag!("--blocklist-file", d.as_str()); }

    a.push("--".into());
    a.extend(clang_args.iter().cloned());

    // The one build dependency cargo can't fetch for us, so a missing bindgen is
    // what someone building from source actually hits - and the bare ENOENT reads
    // like one of the headers we just pointed it at rather than the binary itself.
    let bindgen = std::env::var_os("BINDGEN").unwrap_or_else(|| "bindgen".into());
    let result = Command::new(&bindgen).args(&a).output().unwrap_or_else(|e| {
        panic!("could not run {bindgen:?}: {e}\n\
                bindgen generates the Rust bindings to bcachefs's C code: install it \
                with `cargo install bindgen-cli`, or set $BINDGEN to its path")
    });
    if !result.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&result.stderr));
        std::process::exit(1);
    }

    let bindings = String::from_utf8(result.stdout).expect("bindgen output utf8");
    let bindings = post_process(bindings, ptr_width);
    std::fs::write(format!("{out}/bcachefs.rs"), bindings).expect("write bcachefs.rs");

    // bindgen bakes the wrapper's path into extern.c's `#include`; strip it to
    // the bare name so the C compile finds it right next to extern.c (same dir)
    // — no -I, no build-location-specific absolute path.
    //
    // And bindgen's wrapper serializer (codegen/serialize.rs) writes "const "
    // for a const ResolvedTypeRef and again for the const struct it resolves
    // to: "const const struct bkey_ops *" in the kernel build, which gcc warns
    // about (-Wduplicate-decl-specifier).
    let extern_c = format!("{out}/extern.c");
    let fixed = std::fs::read_to_string(&extern_c).expect("read extern.c")
        .replace(&format!("\"{wrapper}\""), "\"codegen-wrapper.h\"")
        .replace("const const ", "const ");
    std::fs::write(&extern_c, fixed).expect("write extern.c");
}

/// Clang args for the userspace (tools) build: target + liburcu includes + the
/// bcachefs -I set + the build configuration, as the Makefile's C compiles
/// have it (build_config.rs). The kernel build supplies its own (Kbuild
/// computes them).
pub fn userspace_clang_args(src: &str, target: &str) -> Vec<String> {
    let root = parent(src);
    let include_dir = format!("{root}/include");
    let mut a = vec![format!("--target={target}")];
    a.extend(pkg_config_includes("liburcu"));
    // build/: generated headers (rust_types_gen.h), as for the C compiles
    for d in [&root, &src.to_string(), &format!("{root}/c_src"), &include_dir,
              &format!("{root}/build")] {
        a.push(format!("-I{d}"));
    }
    a.extend(userspace_config_args(std::path::Path::new(&root)));
    for f in ["-DRUST_BINDGEN", "-fkeep-inline-functions"] {
        a.push(f.to_string());
    }
    a
}

/// Escape regex metacharacters so a literal directory path can be used as a
/// bindgen `--blocklist-file` regex. Without this, a path like
/// `linux-headers-7.0.13+deb14-common` is interpreted as a regex — the `+`
/// quantifies the preceding `3` — and silently fails to match, so the kernel
/// headers under it never get blocklisted and their types leak into the
/// generated bindings (e.g. a second `struct mutex` distinct from the kernel's).
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

/// Default blocklist dirs for the userspace build: types from the kernel-compat
/// `include/` shim are resolved through bcachefs-shim, not re-emitted. The
/// kernel build passes its own header trees instead.
///
/// Why path regexes and not something structural: bindgen has no notion of
/// "system header" (nothing keyed off -isystem), only file-path patterns. And
/// --allowlist-file can't replace these: allowlisting makes every item in a
/// matching file a *root*, so --wrap-static-fns then wraps every static inline
/// in the tree - which breaks on functions whose signatures use
/// structs-defined-within-structs (bindgen's mangled names for those aren't
/// real C types, so the generated extern.c doesn't compile). Curated
/// allowlists pick the roots; this pattern prunes dependencies back to the
/// shim.
///
/// System headers are deliberately *not* blocklisted. They used to be
/// (`.*/usr/include/.*` and friends), which meant the bindings differed by
/// distro: on NixOS the patterns never matched anything, so `__u32`/`__le64`
/// were emitted normally, while everywhere else they were pruned - and bindgen
/// can't see across a blocklist that a pruned type is Copy, so it dropped
/// Copy/Clone/Debug from every struct holding one and demoted their containing
/// unions to the `__BindgenUnionField` fallback. Harmless until bindgen 0.73
/// gave that fallback's storage a `#[repr(align)]`, which a `#[repr(packed)]`
/// parent rejects outright (E0588 on bch_dirent, bch_ioctl_data).
///
/// Emitting them costs nothing: the allowlists bound what gets generated, so
/// only the system types our own structs actually reference come through, and
/// the primitives are type aliases, which are transparent - `__u32` here and
/// `__u32` from the shim are the same type. Verified by generating on Arch and
/// NixOS: identical type surfaces.
pub fn default_blocklist(src: &str) -> Vec<String> {
    vec![blocklist_dir(&format!("{}/include", parent(src)), &["uapi/"])]
}

/// What the kernel build binds from the kernel's own header trees, which
/// otherwise stay blocklisted: `uapi/`, and the CRCs the string hash uses,
/// which `kernel::bindings` doesn't have. Userspace takes the CRCs from
/// bcachefs-shim.
const KERNEL_UNBLOCKLISTED: &[&str] = &["uapi/", "linux/crc32.h", "linux/crc64.h"];

/// The kernel build's `--blocklist-file` regex for header tree `dir`.
pub fn kernel_blocklist_dir(dir: &str) -> String {
    blocklist_dir(dir, KERNEL_UNBLOCKLISTED)
}

/// A `--blocklist-file` regex for everything under `dir` — except what's under
/// it in `unblocklisted`. That's always its `uapi/` subtree, which must stay
/// visible for the reason above.
///
/// The kernel build can't simply drop its blocklist the way userspace did: the
/// trees it names hold the kernel's own structs — `inode`, `super_block`, `bio`
/// — and `kernel::bindings` already binds those, so a second copy would be a
/// different type and nothing would link. But what bcachefs reaches of them is
/// `uapi/`, where it's scalar typedefs, which are transparent - and the few
/// functions in KERNEL_UNBLOCKLISTED. So the tree stays blocklisted and those
/// are carved out.
fn blocklist_dir(dir: &str, unblocklisted: &[&str]) -> String {
    format!("{}/{}", regex_escape(dir), not_prefixed(unblocklisted))
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

/// The BITMASK() bit ranges, scanned out of the headers, and their accessors:
/// typeinfo_gen.rs. The x-macro lists, Rust reads itself (c_xmacro_from_c!).
pub fn gen_bitmasks(src: &str, out: &str) {
    let bitmasks = parse_bitmasks(src);
    assert!(!bitmasks.is_empty(), "failed to parse any BITMASK() declarations");
    std::fs::write(format!("{out}/typeinfo_gen.rs"),
                   generate_bitmask_table(&bitmasks) + &generate_bitmask_accessors(&bitmasks, out))
        .expect("write typeinfo_gen.rs");
}

// ── BITMASK / LE*_BITMASK bit-range fields ──────────────────────────────────
//
// Bit ranges within flags fields are declared as freestanding macro
// invocations (`LE32_BITMASK(BCH_SNAPSHOT_NO_KEYS, struct bch_snapshot,
// flags, 3, 4)`), separate from the struct definition — so the TypeInfo
// derive can never see them. We scan the headers for the invocations and
// emit two faces from the one parse: a runtime table (kvdb's named-bit
// access, decoded flags display) and typed accessors on the bindgen structs
// (the native replacement for the C SET_* macros).

struct Bitmask {
    name: String,        // stripped + lowercased: "no_keys"
    constant: String,    // as declared: BCH_SNAPSHOT_NO_KEYS
    struct_name: String, // bch_snapshot
    field: String,       // field within the struct: "flags", "flags[0]"
    lo: u8,
    hi: u8,
    le_bits: Option<u8>, // Some(16|32|64) for LE*_BITMASK, None for native BITMASK
}

/// Accessor/table name for a bitmask constant: strip the struct-derived
/// prefix (`BCH_SNAPSHOT_NO_KEYS` on `bch_snapshot` → `no_keys`), falling
/// back to the full constant lowercased when the naming doesn't follow the
/// convention (`INODEv1_STR_HASH` → `inodev1_str_hash`).
fn bitmask_name(constant: &str, struct_name: &str) -> String {
    let upper = struct_name.to_uppercase();
    for prefix in [format!("{upper}_"),
                   format!("{}_", upper.trim_start_matches("BCH_"))] {
        if let Some(rest) = constant.strip_prefix(prefix.as_str()) {
            return rest.to_lowercase();
        }
    }
    constant.to_lowercase()
}

/// Drop `#if 0 ... #endif` regions: those declarations never compile, so we
/// must not emit accessors for them. (An `#else` inside `#if 0` isn't
/// handled - the live half would be dropped too - but the format headers
/// don't do that; the scanner's job is the common shape, not cpp.)
fn strip_if0(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if depth > 0 {
            if t.starts_with("#if") {
                depth += 1;
            } else if t.starts_with("#endif") {
                depth -= 1;
            }
            continue;
        }
        if t.starts_with("#if 0") {
            depth = 1;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn parse_bitmasks_in(text: &str, out: &mut Vec<Bitmask>) {
    for (token, le_bits) in [("LE16_BITMASK(", Some(16u8)),
                             ("LE32_BITMASK(", Some(32)),
                             ("LE64_BITMASK(", Some(64)),
                             ("BITMASK(", None)] {
        let mut pos = 0;
        while let Some(i) = text[pos..].find(token) {
            let start = pos + i;
            pos = start + token.len();

            // Must begin its line: skips the macro #defines, the LE_BITMASK()
            // dispatcher, and "BITMASK(" matching inside "LE64_BITMASK(".
            let line_start = text[..start].rfind('\n').map_or(0, |n| n + 1);
            if !text[line_start..start].trim().is_empty() {
                continue;
            }

            // Args never contain parens; invocations may span lines.
            let Some(close) = text[pos..].find(')') else { continue };
            let args: Vec<&str> = text[pos..pos + close].split(',').map(str::trim).collect();
            let [constant, struct_arg, field, lo, hi] = args[..] else { continue };
            let Some(struct_name) = struct_arg.strip_prefix("struct ") else { continue };
            let (Ok(lo), Ok(hi)) = (lo.parse::<u8>(), hi.parse::<u8>()) else { continue };

            out.push(Bitmask {
                name: bitmask_name(constant, struct_name),
                constant: constant.to_string(),
                struct_name: struct_name.to_string(),
                field: field.to_string(),
                lo,
                hi,
                le_bits,
            });
        }
    }
}

fn parse_bitmasks(src: &str) -> Vec<Bitmask> {
    fn walk(dir: &std::path::Path, headers: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).expect("read_dir") {
            let p = e.expect("dir entry").path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n != "vendor") {
                    walk(&p, headers);
                }
            } else if p.extension().is_some_and(|x| x == "h") {
                headers.push(p);
            }
        }
    }

    let mut headers = Vec::new();
    walk(std::path::Path::new(src), &mut headers);
    headers.sort(); // read_dir order is fs-dependent; output must be stable

    let mut bms = Vec::new();
    for h in headers {
        let text = strip_if0(&std::fs::read_to_string(&h).expect("read header"));
        parse_bitmasks_in(&text, &mut bms);
    }
    bms.sort_by(|a, b| (&a.struct_name, &a.field, a.lo).cmp(&(&b.struct_name, &b.field, b.lo)));

    // A bit name that collides with another bit of the same struct would be
    // unresolvable; fail the build so the naming heuristic gets fixed.
    // (Colliding with a *field* name is fine: fields win bare-name lookup,
    // the bit stays reachable as "<field>.<name>".)
    for w in bms.windows(2) {
        assert!(
            w[0].struct_name != w[1].struct_name || w[0].name != w[1].name,
            "bitmask name collision in {}: '{}' ({} vs {}) - fix bitmask_name()",
            w[0].struct_name, w[0].name, w[0].constant, w[1].constant
        );
    }

    bms
}

fn generate_bitmask_table(bms: &[Bitmask]) -> String {
    let mut out = String::new();
    out.push_str("\n// Generated from BITMASK()/LE*_BITMASK() declarations — do not edit\n\n");
    out.push_str("pub static BITMASK_FIELDS: &[BitmaskField] = &[\n");
    for b in bms {
        out.push_str(&format!(
            "    BitmaskField {{ struct_name: \"{}\", field: \"{}\", name: \"{}\", lo: {}, hi: {} }},\n",
            b.struct_name, b.field, b.name, b.lo, b.hi
        ));
    }
    out.push_str("];\n");
    out
}

/// Typed accessors on the bindgen structs, mirroring the C macros'
/// semantics (`SET_*` masks the value rather than range-checking it).
/// Structs absent from the generated bindings are skipped.
fn generate_bitmask_accessors(bms: &[Bitmask], out_dir: &str) -> String {
    let bindings = std::fs::read_to_string(format!("{out_dir}/bcachefs.rs"))
        .expect("read generated bindings");

    let mut out = String::new();
    let mut i = 0;
    while i < bms.len() {
        let st = &bms[i].struct_name;
        let end = bms[i..].iter().position(|b| &b.struct_name != st)
            .map_or(bms.len(), |n| i + n);
        let group = &bms[i..end];
        i = end;

        if !bindings.contains(&format!("pub struct {st}")) {
            continue;
        }

        out.push_str(&format!("\nimpl crate::c::{st} {{\n"));
        for b in group {
            let width = b.hi - b.lo;
            let mask = if width >= 64 { !0u64 } else { (1u64 << width) - 1 };

            // Compute in u64 regardless of the field's width; the write-back
            // cast restores it. LE*_BITMASK names the field's width, so those
            // convert explicitly; native BITMASK is type-generic in C (the
            // field may be u8..u64), mirrored here by `as u64` / `as _`.
            let (read, write_back) = match b.le_bits {
                Some(64) => (format!("u64::from_le(self.{})", b.field),
                             "f.to_le()".to_string()),
                Some(n) => (format!("(u{n}::from_le(self.{}) as u64)", b.field),
                            format!("(f as u{n}).to_le()")),
                None => (format!("(self.{} as u64)", b.field),
                         "f as _".to_string()),
            };
            // "128_bit_macs" and friends: not a legal method name (the
            // setter is fine - its set_ prefix already de-digits it)
            let m = if b.name.starts_with(|c: char| c.is_ascii_digit()) {
                format!("_{}", b.name)
            } else {
                b.name.clone()
            };
            let set_m = format!("set_{}", b.name);

            out.push_str(&format!("    /// {}: bits {}..{} of {}\n",
                                  b.constant, b.lo, b.hi, b.field));
            if width == 1 {
                out.push_str(&format!(
                    "    #[inline]\n    pub fn {m}(&self) -> bool {{\n\
                     \x20       {read} >> {lo} & 1 != 0\n    }}\n",
                    lo = b.lo
                ));
                out.push_str(&format!(
                    "    #[inline]\n    pub fn {set_m}(&mut self, v: bool) {{\n\
                     \x20       let f = {read} & !(1 << {lo}) | ((v as u64) << {lo});\n\
                     \x20       self.{field} = {write_back};\n    }}\n",
                    lo = b.lo, field = b.field
                ));
            } else {
                out.push_str(&format!(
                    "    #[inline]\n    pub fn {m}(&self) -> u64 {{\n\
                     \x20       {read} >> {lo} & {mask:#x}\n    }}\n",
                    lo = b.lo
                ));
                out.push_str(&format!(
                    "    #[inline]\n    pub fn {set_m}(&mut self, v: u64) {{\n\
                     \x20       let f = {read} & !({mask:#x} << {lo}) | ((v & {mask:#x}) << {lo});\n\
                     \x20       self.{field} = {write_back};\n    }}\n",
                    lo = b.lo, field = b.field
                ));
            }
        }
        out.push_str("}\n");
    }
    out
}

/// Replaces the blocklisted_type_implements_trait bindgen-library callback plus
/// packed_and_align_fix, as pure post-processing on the generated text.
fn post_process(src: String, ptr_width: &str) -> String {
    // The blocklisted_type_implements_trait callback's *entire* effect is keeping
    // derives on bpos/bbpos (every other primitive-bearing struct is generated
    // but never derive-used). Re-add exactly those, with the same sets the
    // library produced.
    // The blocklisted-primitive Copy/Clone/Debug loss (the callback's job) only
    // matters for structs the Rust code derive-uses, plus the union members
    // whose non-Copy-ness flips their union to the __BindgenUnionField wrapper.
    // Default comes from bindgen's manual MaybeUninit impl, so re-add only
    // Debug/Copy/Clone.
    let src = readd_derives(src);
    let src = bitfield_ctor_shadowing_fix(src);

    packed_and_align_fix(src, ptr_width)
}

/// bindgen names a bitfield constructor's parameters for their members, and a
/// member named for its own type - enum btree_id btree_id:8 - is a parameter,
/// and a local, that shadows the type when it's a newtype, a tuple struct:
/// E0530. Such a name gets a trailing _, throughout the constructor.
fn bitfield_ctor_shadowing_fix(src: String) -> String {
    let mut out = String::with_capacity(src.len());
    let mut lines = src.lines();
    while let Some(line) = lines.next() {
        if !line.trim_start().starts_with("pub fn new_bitfield_") {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let close = format!("{}}}", &line[..line.len() - line.trim_start().len()]);
        let mut f = vec![line];
        for l in lines.by_ref() {
            f.push(l);
            if l == close {
                break;
            }
        }
        let mut f = f.join("\n");
        for name in NEWTYPE_ENUM {
            if f.contains(&format!("{name}: {name}")) {
                f = rename_ident(&f, name, &format!("{name}_"))
                    .replace(&format!("{name}_: {name}_"), &format!("{name}_: {name}"));
            }
        }
        out.push_str(&f);
        out.push('\n');
    }
    out
}

/// @s with every identifier @from - not a part of a longer one - as @to.
fn rename_ident(s: &str, from: &str, to: &str) -> String {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(from) {
        let whole = !rest[..i].chars().next_back().is_some_and(ident) &&
                    !rest[i + from.len()..].chars().next().is_some_and(ident);
        out.push_str(&rest[..i]);
        out.push_str(if whole { to } else { from });
        rest = &rest[i + from.len()..];
    }
    out.push_str(rest);
    out
}

fn readd_derives(src: String) -> String {
    let mut lines: Vec<String> = src.lines().map(str::to_owned).collect();
    let mut i = 0;
    while i < lines.len() {
        if let Some(name) = lines[i]
            .strip_prefix("pub struct ")
            .and_then(|s| s.split_once(' ').map(|(name, _)| name))
        {
            if DERIVE_READD.contains(&name) {
                if i > 0 && lines[i - 1].starts_with("#[derive(") {
                    // bindgen dropped the builtin derives (blocklisted field
                    // types) but still emitted the injected custom ones
                    // (TypeInfo); merge ours into that list.
                    if !lines[i - 1].contains("Debug") {
                        lines[i - 1] = lines[i - 1]
                            .replacen("#[derive(", "#[derive(Debug, Copy, Clone, ", 1);
                    }
                } else {
                    lines.insert(i, "#[derive(Debug, Copy, Clone)]".to_owned());
                    i += 1;
                }
            }
        }
        i += 1;
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

// Same fixups as bch_bindgen/build.rs's packed_and_align_fix, but structural:
// find the struct by name and rewrite its repr attribute, scanning back over
// any attribute lines in between. The old fixed-string matching broke silently
// whenever the attribute block changed shape (the injected TypeInfo derives
// did exactly that).
fn packed_and_align_fix(bindings: String, ptr_width: &str) -> String {
    const PACKED_TO_ALIGN8: &[&str] =
        &["btree_node", "bch_extent_crc128", "jset", "btree_node_entry", "bch_sb"];
    // bindgen emulates a union as a struct of PhantomData fields over a
    // `[u64; N]`, so its alignment is the array's: 8 on x86_64, but 4 on i386,
    // where the ABI aligns u64 to 4. Types whose C alignment is 8 there anyway
    // - because they say __aligned(8) - therefore fail bindgen's own alignment
    // assertion, and can only be fixed on the Rust side.
    const ALIGN8_32BIT: &[&str] =
        &["btree_node__bindgen_ty_1", "btree_node_entry__bindgen_ty_1",
          "bch_ioctl_query_accounting", "bch_extent_crc"];
    let mut lines: Vec<String> = bindings.lines().map(str::to_owned).collect();

    for i in 0..lines.len() {
        /*
         * bindgen >= 0.73 wraps padding in `__BindgenOpaqueArrayN<[u8; N]>`,
         * declared `#[repr(C, align(N))]`. Padding is emitted to reproduce the
         * C layout, so forcing alignment on it changes the layout it exists to
         * preserve - bch_fs_allocator gains 128 bytes, bch_fs_btree 64, and
         * bch_fs both. Unwrap to the bare array, which is what 0.72 emitted.
         *
         * Only padding: the same wrapper holds union storage and opaque blobs,
         * where the alignment is the C type's and has to stay.
         *
         * Textual, and safe to be: bindgen emits a size *and* an alignment
         * assertion per type, both computed from C, so a layout this gets wrong
         * fails to compile and names the type.
         */
        if let Some((field, ty)) = lines[i].split_once(": __BindgenOpaqueArray") {
            if field.trim_start().starts_with("pub __bindgen_padding_") {
                if let Some(inner) = ty.split_once('<').and_then(|(_, r)| r.rsplit_once('>')) {
                    lines[i] = format!("{field}: {}{}", inner.0, inner.1);
                    continue;
                }
            }
        }

        let Some(rest) = lines[i].strip_prefix("pub struct ") else { continue };
        let Some(name) = rest.split([' ', '{', '<']).next() else { continue };

        let fix_packed = PACKED_TO_ALIGN8.contains(&name);
        let fix_32bit = ptr_width == "32"
            && (ALIGN8_32BIT.contains(&name) || name.starts_with("bkey_i_"));
        if !fix_packed && !fix_32bit {
            continue;
        }

        let mut j = i;
        while j > 0 && lines[j - 1].starts_with("#[") {
            j -= 1;
            if fix_packed && lines[j] == "#[repr(C, packed(8))]" {
                lines[j] = "#[repr(C, align(8))]".into();
                break;
            }
            if fix_32bit && lines[j] == "#[repr(C)]" {
                lines[j] = "#[repr(C, align(8))]".into();
                break;
            }
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn parent(p: &str) -> String {
    std::path::Path::new(p).parent().expect("src has a parent").to_string_lossy().into_owned()
}

fn pkg_config_includes(lib: &str) -> Vec<String> {
    // Honor $PKG_CONFIG so cross builds use the target's pkg-config wrapper
    // (e.g. aarch64-unknown-linux-gnu-pkg-config); fall back to the plain name.
    let pkg_config = std::env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".into());
    let o = Command::new(pkg_config).args(["--cflags-only-I", lib]).output().expect("run pkg-config");
    String::from_utf8_lossy(&o.stdout).split_whitespace().map(String::from).collect()
}

// Only the standalone tool (codegen_main.rs) needs this; build.rs reads TARGET.
#[allow(dead_code)]
fn host_target() -> String {
    let o = Command::new("rustc").arg("-vV").output().expect("run rustc -vV");
    String::from_utf8_lossy(&o.stdout)
        .lines().find_map(|l| l.strip_prefix("host: ")).expect("rustc host").to_string()
}
