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

const HEADERS: &[&str] = &[
    "bcachefs.h", "opts.h",
    "btree/cache.h", "btree/interior.h", "btree/iter.h", "btree/read.h",
    "alloc/accounting.h", "alloc/background.h", "alloc/buckets.h", "alloc/disk_groups.h",
    "data/checksum.h", "data/extents.h", "data/io_misc.h", "data/move.h", "data/read.h", "data/update.h", "data/write.h",
    "debug/debug.h",
    "init/dev.h", "init/error.h", "init/fs.h", "init/passes.h",
    "fs/check.h", "fs/dirent.h", "fs/inode.h", "fs/namei.h", "fs/xattr.h",
    "journal/init.h", "journal/read.h", "journal/reclaim.h", "journal/seq_blacklist.h", "journal/validate.h",
    "sb/io.h", "sb/members.h",
];

// Translated 1:1 from the bindgen builder calls in build.rs.
const ALLOWLIST_FUNCTION: &[&str] = &[
    // rust_* are C shims that exist solely for Rust to call (e.g. util/locking.h
    // wraps the memalloc_flags_* static inlines, which don't reach Rust). Same
    // convention as bch_bindgen's allowlist.
    ".*bch2_.*", "rust_.*", "block_bytes", "match_string", "printbuf.*", "_bch2_err_matches",
    "bpos_.*", "bkey_init", "bkey_.*_init", "bkey_i_to_s", "bkey_i_to_s_c",
    "btree_iter_path", "extent_entry_u64s", "enumerated_ref_put",
    // crypto helpers for the dump sanitize path (static inlines, not
    // bch2_-prefixed): nonce constructors + bset_encrypt, driven from Rust
    // over the already-wrapped bch2_checksum / bch2_encrypt.
    "journal_nonce", "btree_nonce", "bset_encrypt",
];
const BLOCKLIST_FUNCTION: &[&str] = &["bch2_prt_vprintf", ".*bch2_snapshot_id_state"];
const BLOCKLIST_TYPE: &[&str] = &["bch_ioctl_data_event", "bch_replicas_padded__bindgen_ty_.*"];
const BLOCKLIST_ITEM: &[&str] = &["bch2_bkey_ops"];
const ALLOWLIST_VAR: &[&str] = &["BCH_.*", "BTREE_MAX_DEPTH", "KEY_SPEC_.*", "Fix753_.*", "bch.*", "__bch2.*", "__BTREE_ITER.*", "BTREE_ITER.*"];
const ALLOWLIST_TYPE: &[&str] = &["bch_.*", "bkey_i_.*", "bkey_s_c_.*", "bkey_s_.*", "btree_flags", "disk_accounting_type", "fsck_err_opts", "nonce", "sb_names",
    // genradix: kernel::bindings doesn't bind it, so we emit it ourselves from a
    // build-time copy of the kernel header (see run_bindgen + fs/Makefile).
    "genradix.*", "__genradix.*"];
const BITFIELD_ENUM: &[&str] = &[
    "btree_iter_update_trigger_flags",
    "bch_reservation_flags",
    "bch_trans_commit_flags",
    "bch_write_flags",
];
const RUSTIFIED_ENUM: &[&str] = &["fsck_err_opts", "bch_key_types"];
const NEWTYPE_ENUM: &[&str] = &[
    "bcachefs_metadata_version",
    "bch_bkey_type",
    "bch_compression_type",
    "bch_data_type",
    "bch_jset_entry_type",
    "bch_kdf_types",
    "bch_opt_id",
    "bch_reconcile_accounting_type",
    "bch_sb_field_type",
    "disk_accounting_type",
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
    "bch_ioctl_snapshot_node",
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

    let bindgen = std::env::var_os("BINDGEN").unwrap_or_else(|| "bindgen".into());
    let result = Command::new(bindgen).args(&a).output().expect("run bindgen");
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
    let extern_c = format!("{out}/extern.c");
    let fixed = std::fs::read_to_string(&extern_c).expect("read extern.c")
        .replace(&format!("\"{wrapper}\""), "\"codegen-wrapper.h\"");
    std::fs::write(&extern_c, fixed).expect("write extern.c");
}

/// Clang args for the userspace (tools) build: target + liburcu includes + the
/// bcachefs -I/-D set. The kernel build supplies its own (Kbuild computes them).
pub fn userspace_clang_args(src: &str, target: &str) -> Vec<String> {
    let root = parent(src);
    let include_dir = format!("{root}/include");
    let mut a = vec![format!("--target={target}")];
    a.extend(pkg_config_includes("liburcu"));
    for d in [&root, &src.to_string(), &format!("{root}/c_src"), &include_dir] {
        a.push(format!("-I{d}"));
    }
    for f in ["-DZSTD_STATIC_LINKING_ONLY", "-DNO_BCACHEFS_FS", "-D_GNU_SOURCE",
              "-DRUST_BINDGEN", "-fkeep-inline-functions"] {
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
/// `include/` shim and from `/usr` are resolved through bcachefs-shim, not
/// re-emitted. The kernel build passes its own header trees instead.
pub fn default_blocklist(src: &str) -> Vec<String> {
    let include_dir = format!("{}/include", parent(src));
    vec![format!("{}/.*", regex_escape(&include_dir)), ".*/usr/.*".to_string()]
}

/// Generate the x-macro-derived *_gen.rs files from the *_format.h headers.
pub fn gen_xmacros(src: &str, out: &str) {
    let format_h = std::fs::read_to_string(format!("{src}/bcachefs_format.h"))
        .expect("reading bcachefs_format.h");

    let errcode_h = std::fs::read_to_string(format!("{src}/errcode.h"))
        .expect("reading errcode.h");
    let errcodes = parse_xmacro(&errcode_h, "BCH_ERRCODES");
    assert!(!errcodes.is_empty(), "failed to parse BCH_ERRCODES()");
    std::fs::write(format!("{out}/errcodes_gen.rs"), generate_errcodes(&errcodes))
        .expect("write errcodes_gen.rs");

    let bkey_types = parse_xmacro(&format_h, "BCH_BKEY_TYPES");
    assert!(!bkey_types.is_empty(), "failed to parse BCH_BKEY_TYPES()");
    std::fs::write(format!("{out}/bkey_types_gen.rs"), generate_bkey_types(&bkey_types))
        .expect("write bkey_types_gen.rs");

    let bitmasks = parse_bitmasks(src);
    assert!(!bitmasks.is_empty(), "failed to parse any BITMASK() declarations");
    std::fs::write(format!("{out}/typeinfo_gen.rs"),
                   generate_bkey_typeinfo(&bkey_types)
                       + &generate_bitmask_table(&bitmasks)
                       + &generate_bitmask_accessors(&bitmasks, out))
        .expect("write typeinfo_gen.rs");

    let sb_fields = parse_xmacro(&format_h, "BCH_SB_FIELDS");
    assert!(!sb_fields.is_empty(), "failed to parse BCH_SB_FIELDS()");
    std::fs::write(format!("{out}/sb_field_types_gen.rs"), generate_sb_field_impls(&sb_fields))
        .expect("write sb_field_types_gen.rs");

    let members_h = std::fs::read_to_string(format!("{src}/sb/members_format.h"))
        .expect("reading members_format.h");
    let member_states = parse_xmacro(&members_h, "BCH_MEMBER_STATES");
    assert!(!member_states.is_empty(), "failed to parse BCH_MEMBER_STATES()");
    std::fs::write(format!("{out}/member_states_gen.rs"),
                   generate_str_table("MEMBER_STATE_NAMES", &member_states))
        .expect("write member_states_gen.rs");

    let snapshots_h = std::fs::read_to_string(format!("{src}/snapshots/format.h"))
        .expect("reading snapshots/format.h");
    let snapshot_states = parse_xmacro(&snapshots_h, "BCH_SNAPSHOT_STATES");
    assert!(!snapshot_states.is_empty(), "failed to parse BCH_SNAPSHOT_STATES()");
    let subvolume_states = parse_xmacro(&snapshots_h, "BCH_SUBVOLUME_STATES");
    assert!(!subvolume_states.is_empty(), "failed to parse BCH_SUBVOLUME_STATES()");
    std::fs::write(format!("{out}/snapshot_states_gen.rs"),
                   "// Auto-generated — do not edit\n\n".to_string() +
                   &generate_value_table("SNAPSHOT_STATE_VALUES", &snapshot_states) +
                   &generate_value_table("SUBVOLUME_STATE_VALUES", &subvolume_states))
        .expect("write snapshot_states_gen.rs");

    let counters_h = std::fs::read_to_string(format!("{src}/sb/counters_format.h"))
        .expect("reading counters_format.h");
    let counters = parse_xmacro(&counters_h, "BCH_PERSISTENT_COUNTERS");
    assert!(!counters.is_empty(), "failed to parse BCH_PERSISTENT_COUNTERS()");
    std::fs::write(format!("{out}/counters_gen.rs"), generate_counter_table(&counters))
        .expect("write counters_gen.rs");

    let extents_h = std::fs::read_to_string(format!("{src}/data/extents_format.h"))
        .expect("reading extents_format.h");
    let extent_entry_types = parse_xmacro(&extents_h, "BCH_EXTENT_ENTRY_TYPES");
    assert!(!extent_entry_types.is_empty(), "failed to parse BCH_EXTENT_ENTRY_TYPES()");
    std::fs::write(format!("{out}/extent_entry_types_gen.rs"),
                   generate_extent_entry_u64s(&extent_entry_types))
        .expect("write extent_entry_types_gen.rs");

    let accounting_h = std::fs::read_to_string(format!("{src}/alloc/accounting_format.h"))
        .expect("reading accounting_format.h");
    let disk_accounting_types = parse_xmacro(&accounting_h, "BCH_DISK_ACCOUNTING_TYPES");
    assert!(!disk_accounting_types.is_empty(), "failed to parse BCH_DISK_ACCOUNTING_TYPES()");

    let reconcile_h = std::fs::read_to_string(format!("{src}/data/reconcile/format.h"))
        .expect("reading reconcile/format.h");
    let reconcile_accounting_types = parse_xmacro(&reconcile_h, "BCH_RECONCILE_ACCOUNTING");
    assert!(!reconcile_accounting_types.is_empty(), "failed to parse BCH_RECONCILE_ACCOUNTING()");

    let data_types = parse_xmacro(&accounting_h, "BCH_DATA_TYPES");
    assert!(!data_types.is_empty(), "failed to parse BCH_DATA_TYPES()");

    let metadata_versions = parse_xmacro(&format_h, "BCH_METADATA_VERSIONS");
    assert!(!metadata_versions.is_empty(), "failed to parse BCH_METADATA_VERSIONS()");

    let compression_types = parse_xmacro(&format_h, "BCH_COMPRESSION_TYPES");
    assert!(!compression_types.is_empty(), "failed to parse BCH_COMPRESSION_TYPES()");

    let jset_entry_types = parse_xmacro(&format_h, "BCH_JSET_ENTRY_TYPES");
    assert!(!jset_entry_types.is_empty(), "failed to parse BCH_JSET_ENTRY_TYPES()");

    let btree_ids = parse_xmacro(&format_h, "BCH_BTREE_IDS");
    assert!(!btree_ids.is_empty(), "failed to parse BCH_BTREE_IDS()");

    let opts_h = std::fs::read_to_string(format!("{src}/opts.h"))
        .expect("reading opts.h");
    let opts = parse_xmacro(&opts_h, "BCH_OPTS");
    assert!(!opts.is_empty(), "failed to parse BCH_OPTS()");

    std::fs::write(
        format!("{out}/newtype_enum_aliases_gen.rs"),
        [
            generate_newtype_enum_aliases(
                "bcachefs_metadata_version",
                "bcachefs_metadata_version",
                &metadata_versions,
                "max",
                "bcachefs_metadata_version_max",
            ),
            generate_newtype_enum_aliases(
                "bch_opt_id",
                "Opt",
                &opts,
                "nr",
                "bch2_opts_nr",
            ),
            generate_newtype_enum_aliases(
                "btree_id",
                "BTREE_ID",
                &btree_ids,
                "nr",
                "BTREE_ID_NR",
            ),
            generate_newtype_enum_aliases(
                "bch_bkey_type",
                "KEY_TYPE",
                &bkey_types,
                "nr",
                "KEY_TYPE_MAX",
            ),
            generate_newtype_enum_aliases(
                "bch_data_type",
                "BCH_DATA",
                &data_types,
                "nr",
                "BCH_DATA_NR",
            ),
            generate_newtype_enum_aliases(
                "bch_compression_type",
                "BCH_COMPRESSION_TYPE",
                &compression_types,
                "nr",
                "BCH_COMPRESSION_TYPE_NR",
            ),
            generate_newtype_enum_aliases(
                "bch_jset_entry_type",
                "BCH_JSET_ENTRY",
                &jset_entry_types,
                "nr",
                "BCH_JSET_ENTRY_NR",
            ),
            generate_newtype_enum_aliases(
                "bch_sb_field_type",
                "BCH_SB_FIELD",
                &sb_fields,
                "nr",
                "BCH_SB_FIELD_NR",
            ),
            generate_newtype_enum_aliases(
                "disk_accounting_type",
                "BCH_DISK_ACCOUNTING",
                &disk_accounting_types,
                "nr",
                "BCH_DISK_ACCOUNTING_TYPE_NR",
            ),
            generate_newtype_enum_aliases(
                "bch_reconcile_accounting_type",
                "BCH_RECONCILE_ACCOUNTING",
                &reconcile_accounting_types,
                "nr",
                "BCH_RECONCILE_ACCOUNTING_NR",
            ),
        ].join("\n"),
    )
    .expect("write newtype_enum_aliases_gen.rs");

    std::fs::write(
        format!("{out}/btree_ids_gen.rs"),
        generate_btree_ids_known(&btree_ids),
    )
    .expect("write btree_ids_gen.rs");
}

// ---------------------------------------------------------------------------
// x-macro parsing + generators — verbatim from build.rs.

/// Parse an x-macro from a C header: find `#define {macro}(...)` and extract all
/// `x(...)` invocations, each as a vec of trimmed argument strings.
fn parse_xmacro(header: &str, macro_name: &str) -> Vec<Vec<String>> {
    let define_prefix = format!("#define {}", macro_name);
    let mut in_macro = false;
    let mut macro_text = String::new();

    for line in header.lines() {
        let trimmed = line.trim();
        if !in_macro {
            if trimmed.starts_with(&define_prefix) {
                in_macro = true;
                if let Some(pos) = trimmed.find(&define_prefix) {
                    let after = &trimmed[pos + define_prefix.len()..];
                    let after = if let Some(i) = after.find(')') { &after[i + 1..] } else { after };
                    macro_text.push_str(after.trim_end_matches('\\').trim());
                    macro_text.push(' ');
                }
                if !trimmed.ends_with('\\') { break; }
            }
        } else {
            macro_text.push_str(trimmed.trim_end_matches('\\').trim());
            macro_text.push(' ');
            if !trimmed.ends_with('\\') { break; }
        }
    }

    let mut entries = Vec::new();
    let bytes = macro_text.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        let Some(start) = macro_text[pos..].find("x(") else { break };
        let open = pos + start + 2;
        let mut depth = 1usize;
        let mut i = open;
        while i < bytes.len() && depth > 0 {
            match bytes[i] { b'(' => depth += 1, b')' => depth -= 1, _ => {} }
            if depth > 0 { i += 1; }
        }
        if depth == 0 {
            entries.push(split_xmacro_args(&macro_text[open..i]));
            pos = i + 1;
        } else { break; }
    }
    entries
}

fn split_xmacro_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for ch in s.chars() {
        match ch {
            '(' => { depth += 1; current.push(ch); }
            ')' => { depth -= 1; current.push(ch); }
            ',' if depth == 0 => { args.push(current.trim().to_string()); current.clear(); }
            _ => current.push(ch),
        }
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() { args.push(tail); }
    args
}

fn rust_ident(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c == '_' || c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();

    if !out
        .chars()
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
    {
        out.insert(0, '_');
    }

    out
}

fn generate_sb_field_impls(entries: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated from BCH_SB_FIELDS() — do not edit\n\n");
    out.push_str("/// Marker trait connecting an sb field struct to its field type enum.\n");
    out.push_str("///\n");
    out.push_str("/// # Safety\n");
    out.push_str("/// Implementors must ensure FIELD_TYPE matches the struct type,\n");
    out.push_str("/// and that `field` is the first member (offset 0).\n");
    out.push_str("pub unsafe trait SbField: Sized {\n");
    out.push_str("    const FIELD_TYPE: c::bch_sb_field_type;\n");
    out.push_str("}\n\n");
    for e in entries {
        let name = &e[0];
        let ident = rust_ident(name);
        out.push_str(&format!(
            "unsafe impl SbField for c::bch_sb_field_{name} {{\n\
             \x20   const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::{ident};\n\
             }}\n\n"
        ));
    }
    out
}

fn generate_str_table(name: &str, entries: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated — do not edit\n\n");
    out.push_str(&format!("pub const {name}: &[&str] = &[\n"));
    for e in entries { out.push_str(&format!("    \"{}\",\n", e[0])); }
    out.push_str("];\n");
    out
}

/// Name <-> value table for an x-macro enum with explicit values: x(name, val)
fn generate_value_table(name: &str, entries: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str(&format!("pub const {name}: &[(&str, u64)] = &[\n"));
    for e in entries { out.push_str(&format!("    (\"{}\", {}),\n", e[0], e[1])); }
    out.push_str("];\n");
    out
}

fn generate_counter_table(entries: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated from BCH_PERSISTENT_COUNTERS() — do not edit\n\n");
    out.push_str("pub struct CounterInfo {\n");
    out.push_str("    pub name: &'static str,\n");
    out.push_str("    pub stable_id: u16,\n");
    out.push_str("    pub is_sectors: bool,\n");
    out.push_str("}\n\n");
    out.push_str("pub const COUNTERS: &[CounterInfo] = &[\n");
    for e in entries {
        let name = &e[0];
        let stable_id = &e[1];
        let is_sectors = e[2].contains("TYPE_SECTORS");
        out.push_str(&format!(
            "    CounterInfo {{ name: \"{name}\", stable_id: {stable_id}, is_sectors: {is_sectors} }},\n"
        ));
    }
    out.push_str("];\n");
    out
}

fn generate_extent_entry_u64s(entries: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated from BCH_EXTENT_ENTRY_TYPES() — do not edit\n\n");
    out.push_str("/// Size in u64s for each known extent entry type.\n");
    out.push_str("pub fn extent_entry_type_u64s(ty: u32) -> Option<usize> {\n");
    out.push_str("    use core::mem::size_of;\n");
    out.push_str("    Some(match ty {\n");
    for e in entries {
        out.push_str(&format!("        {} => size_of::<c::bch_extent_{}>() / 8,\n", e[1], e[0]));
    }
    out.push_str("        _ => return None,\n");
    out.push_str("    })\n");
    out.push_str("}\n");
    out
}

fn generate_errcodes(entries: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated from BCH_ERRCODES() — do not edit\n\n");

    for e in entries {
        if e.len() < 2 {
            continue;
        }

        let name = rust_ident(&e[1]);
        out.push_str(&format!(
            "#[allow(non_upper_case_globals)]\n\
             pub const {name}: bch_errcode = bch_errcode::BCH_ERR_{name};\n"
        ));
    }

    out
}

fn generate_newtype_enum_aliases(
    ty: &str,
    prefix: &str,
    entries: &[Vec<String>],
    nr_alias: &str,
    nr_const: &str,
) -> String {
    let mut out = String::new();

    out.push_str("// Auto-generated from x-macros - do not edit\n\n");
    out.push_str(&format!("impl c::{ty} {{\n"));
    for e in entries {
        let name = &e[0];
        let alias = rust_ident(name);
        out.push_str(&format!(
            "    #[allow(non_upper_case_globals)]\n    pub const {alias}: Self = Self::{prefix}_{name};\n"
        ));
    }
    out.push_str(&format!(
        "    #[allow(non_upper_case_globals)]\n    pub const {nr_alias}: Self = Self::{nr_const};\n"
    ));
    out.push_str("}\n");

    out
}

fn generate_bkey_typeinfo(entries: &[Vec<String>]) -> String {
    let mut out = String::new();

    out.push_str("// Auto-generated from BCH_BKEY_TYPES() — do not edit\n\n");
    out.push_str(
        "/// A bkey type together with its val struct's type info.\n\
         pub struct BkeyTypeInfo {\n\
         \x20   pub name: &'static str,\n\
         \x20   pub type_: u32,\n\
         \x20   pub info: &'static StructInfo,\n\
         }\n\n",
    );

    out.push_str("pub static BKEY_TYPE_INFO: &[BkeyTypeInfo] = &[\n");
    for e in entries {
        out.push_str(&format!(
            "    BkeyTypeInfo {{ name: \"{0}\", type_: {1}, \
                 info: <crate::c::bch_{0} as TypeInfo>::INFO }},\n",
            e[0], e[1]
        ));
    }
    out.push_str("];\n\n");

    out.push_str(
        "pub fn bkey_val_info(type_: u32) -> Option<&'static StructInfo> {\n\
         \x20   BKEY_TYPE_INFO.iter().find(|t| t.type_ == type_).map(|t| t.info)\n\
         }\n\n\
         pub fn bkey_type_info_by_name(name: &str) -> Option<&'static BkeyTypeInfo> {\n\
         \x20   BKEY_TYPE_INFO.iter().find(|t| t.name == name)\n\
         }\n",
    );

    out
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

fn generate_btree_ids_known(entries: &[Vec<String>]) -> String {
    let mut out = String::new();

    out.push_str("// Auto-generated from BCH_BTREE_IDS() - do not edit\n\n");
    out.push_str("pub const BTREE_IDS_KNOWN: &[c::btree_id] = &[\n");
    for e in entries {
        let name = rust_ident(&e[0]);
        out.push_str(&format!("    c::btree_id::{name},\n"));
    }
    out.push_str("];\n");

    let mut mask: u64 = 0;
    for e in entries {
        let nr: u32 = e[1].trim().parse().expect("BCH_BTREE_IDS: numeric id");
        if e[2].contains("BTREE_IS_snapshots") {
            mask |= 1 << nr;
        }
    }
    out.push_str(&format!(
        "\n/// Btrees whose keys are snapshotted; mirrors the C btree_has_snapshots_mask\n\
         /// (a static const bindgen can't see).\n\
         pub const BTREE_HAS_SNAPSHOTS_MASK: u64 = {mask:#x};\n"
    ));

    out
}

fn generate_bkey_types(entries: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated from BCH_BKEY_TYPES() — do not edit\n\n");

    for e in entries {
        let name = &e[0];
        let type_name = format!("Bkey{}", snake_to_pascal(name));
        out.push_str(&format!("pub type {type_name} = Bkey<c::bkey_i_{name}>;\n"));
    }
    out.push('\n');

    for e in entries {
        let name = &e[0];
        out.push_str(&format!(
            "impl c::bkey_i_{name} {{\n\
             \x20   pub fn k(&self) -> &c::bkey {{ unsafe {{ self.__bindgen_anon_1.k.as_ref() }} }}\n\
             \x20   pub fn k_mut(&mut self) -> &mut c::bkey {{ unsafe {{ self.__bindgen_anon_1.k.as_mut() }} }}\n\
             \x20   pub fn k_i(&self) -> &c::bkey_i {{ unsafe {{ self.__bindgen_anon_1.k_i.as_ref() }} }}\n\
             \x20   pub fn k_i_mut(&mut self) -> &mut c::bkey_i {{ unsafe {{ self.__bindgen_anon_1.k_i.as_mut() }} }}\n\
             }}\n\n"
        ));
    }

    out.push_str("pub trait BkeyInit: Default {\n");
    out.push_str("    fn init(&mut self);\n");
    out.push_str("    fn k(&self) -> &c::bkey;\n");
    out.push_str("    fn k_mut(&mut self) -> &mut c::bkey;\n");
    out.push_str("    fn k_i(&self) -> &c::bkey_i;\n");
    out.push_str("    fn k_i_mut(&mut self) -> &mut c::bkey_i;\n");
    out.push_str("}\n\n");

    for e in entries {
        let name = &e[0];
        out.push_str(&format!(
            "impl BkeyInit for c::bkey_i_{name} {{\n\
             \x20   fn init(&mut self) {{ unsafe {{ c::bkey_{name}_init(self.k_i_mut()) }}; }}\n\
             \x20   fn k(&self) -> &c::bkey {{ c::bkey_i_{name}::k(self) }}\n\
             \x20   fn k_mut(&mut self) -> &mut c::bkey {{ c::bkey_i_{name}::k_mut(self) }}\n\
             \x20   fn k_i(&self) -> &c::bkey_i {{ c::bkey_i_{name}::k_i(self) }}\n\
             \x20   fn k_i_mut(&mut self) -> &mut c::bkey_i {{ c::bkey_i_{name}::k_i_mut(self) }}\n\
             }}\n\n"
        ));
    }

    out.push_str("/// Typed dispatch for inline bkeys (`bkey_i`).\n");
    out.push_str("pub enum BkeyValI<'a> {\n");
    for e in entries { out.push_str(&format!("    {}(&'a c::bkey_i_{}),\n", e[0], e[0])); }
    out.push_str("    unknown(&'a c::bkey_i),\n}\n\n");
    out.push_str("impl<'a> BkeyValI<'a> {\n");
    out.push_str("    #[allow(clippy::missing_transmute_annotations)]\n");
    out.push_str("    pub fn from_bkey_i(k: &'a c::bkey_i) -> Self {\n");
    out.push_str("        match k.k.type_ as u32 {\n");
    for e in entries { out.push_str(&format!("            {} => BkeyValI::{}(unsafe {{ core::mem::transmute(k) }}),\n", e[1], e[0])); }
    out.push_str("            _ => BkeyValI::unknown(k),\n        }\n    }\n}\n\n");

    out.push_str("/// Typed dispatch for mutable inline bkeys (`bkey_i`).\n");
    out.push_str("pub enum BkeyValIMut<'a> {\n");
    for e in entries { out.push_str(&format!("    {}(&'a mut c::bkey_i_{}),\n", e[0], e[0])); }
    out.push_str("    unknown(&'a mut c::bkey_i),\n}\n\n");
    out.push_str("impl<'a> BkeyValIMut<'a> {\n");
    out.push_str("    #[allow(clippy::missing_transmute_annotations)]\n");
    out.push_str("    pub fn from_bkey_i(k: &'a mut c::bkey_i) -> Self {\n");
    out.push_str("        let type_ = k.k.type_;\n");
    out.push_str("        match type_ as u32 {\n");
    for e in entries { out.push_str(&format!("            {} => BkeyValIMut::{}(unsafe {{ core::mem::transmute(k) }}),\n", e[1], e[0])); }
    out.push_str("            _ => BkeyValIMut::unknown(k),\n        }\n    }\n}\n\n");

    out.push_str("/// Typed dispatch for split-const bkey references.\n");
    out.push_str("pub enum BkeyValSC<'a> {\n");
    for e in entries { out.push_str(&format!("    {}(&'a c::bkey, &'a c::bch_{}),\n", e[0], e[0])); }
    out.push_str("    unknown(&'a c::bkey, u8),\n}\n\n");
    out.push_str("impl<'a> BkeyValSC<'a> {\n");
    out.push_str("    #[allow(clippy::missing_transmute_annotations)]\n");
    out.push_str("    pub fn from_bkey_i(k: &'a c::bkey_i) -> Self {\n");
    out.push_str("        match k.k.type_ as u32 {\n");
    for e in entries { out.push_str(&format!("            {} => BkeyValSC::{}(&k.k, unsafe {{ core::mem::transmute(&k.v) }}),\n", e[1], e[0])); }
    out.push_str("            _ => BkeyValSC::unknown(&k.k, k.k.type_),\n        }\n    }\n\n");
    out.push_str("    /// Construct from raw key and value references.\n");
    out.push_str("    ///\n    /// # Safety\n");
    out.push_str("    /// `val` must point to valid data for the bkey type indicated by `k.type_`.\n");
    out.push_str("    #[allow(clippy::missing_transmute_annotations)]\n");
    out.push_str("    pub unsafe fn from_raw(k: &'a c::bkey, val: &'a c::bch_val) -> Self {\n");
    out.push_str("        match k.type_ as u32 {\n");
    for e in entries { out.push_str(&format!("            {} => BkeyValSC::{}(k, unsafe {{ core::mem::transmute(val) }}),\n", e[1], e[0])); }
    out.push_str("            _ => BkeyValSC::unknown(k, k.type_),\n        }\n    }\n}\n\n");

    out.push_str("/// Typed dispatch for split-mutable bkey references.\n");
    out.push_str("pub enum BkeyValS<'a> {\n");
    for e in entries { out.push_str(&format!("    {}(&'a mut c::bkey, &'a mut c::bch_{}),\n", e[0], e[0])); }
    out.push_str("    unknown(&'a mut c::bkey, u8),\n}\n\n");
    out.push_str("impl<'a> BkeyValS<'a> {\n");
    out.push_str("    #[allow(clippy::missing_transmute_annotations)]\n");
    out.push_str("    pub fn from_bkey_i(k: &'a mut c::bkey_i) -> Self {\n");
    out.push_str("        let type_ = k.k.type_;\n");
    out.push_str("        match type_ as u32 {\n");
    for e in entries { out.push_str(&format!("            {} => BkeyValS::{}(&mut k.k, unsafe {{ core::mem::transmute(&mut k.v) }}),\n", e[1], e[0])); }
    out.push_str("            _ => BkeyValS::unknown(&mut k.k, type_),\n        }\n    }\n}\n");
    out
}

fn snake_to_pascal(s: &str) -> String {
    let mut out = String::new();

    for word in s.split('_') {
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            out.extend(c.to_uppercase());
            out.push_str(chars.as_str());
        }
    }

    out
}

/// Replaces the two bindgen-library callbacks (Fix753 item-name strip +
/// blocklisted_type_implements_trait) plus packed_and_align_fix, as pure
/// post-processing on the generated text.
fn post_process(src: String, ptr_width: &str) -> String {
    // Fix753: the headers wrap bindgen-issue-753 items as `Fix753_X`; the
    // library callback strips the prefix. Do it textually.
    let src = src.replace("Fix753_", "");

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

    packed_and_align_fix(src, ptr_width)
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
    const ALIGN8_32BIT: &[&str] =
        &["btree_node__bindgen_ty_1", "btree_node_entry__bindgen_ty_1",
          "bch_ioctl_query_accounting"];

    let mut lines: Vec<String> = bindings.lines().map(str::to_owned).collect();

    for i in 0..lines.len() {
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
