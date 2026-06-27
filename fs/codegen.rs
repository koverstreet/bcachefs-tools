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
    "btree/cache.h", "btree/interior.h", "btree/iter.h",
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
    ".*bch2_.*", "block_bytes", "match_string", "printbuf.*", "_bch2_err_matches",
    "bpos_.*", "bkey_.*_init", "bkey_i_to_s", "bkey_i_to_s_c",
    "btree_iter_path", "extent_entry_u64s", "enumerated_ref_put",
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
    "bch_key", "bch_encrypted_key",
    "bch_ioctl_snapshot_node",
];

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
    flag!("--wrap-static-fns-path", format!("{out}/extern.c"));

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

fn generate_btree_ids_known(entries: &[Vec<String>]) -> String {
    let mut out = String::new();

    out.push_str("// Auto-generated from BCH_BTREE_IDS() - do not edit\n\n");
    out.push_str("pub const BTREE_IDS_KNOWN: &[c::btree_id] = &[\n");
    for e in entries {
        let name = rust_ident(&e[0]);
        out.push_str(&format!("    c::btree_id::{name},\n"));
    }
    out.push_str("];\n");

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
    let mut out = String::with_capacity(src.len());
    let mut prev = "";

    for line in src.lines() {
        if let Some(name) = line.strip_prefix("pub struct ").and_then(|s| s.split_once(' ').map(|(name, _)| name)) {
            if DERIVE_READD.contains(&name) && !prev.starts_with("#[derive(") {
                out.push_str("#[derive(Debug, Copy, Clone)]\n");
            }
        }

        out.push_str(line);
        out.push('\n');
        prev = line;
    }

    out
}

// Verbatim from build.rs (with the env read replaced by a parameter).
fn packed_and_align_fix(bindings: String, ptr_width: &str) -> String {
    let bindings = bindings
        .replace("#[repr(C, packed(8))]\npub struct btree_node {",
                 "#[repr(C, align(8))]\npub struct btree_node {")
        .replace("#[repr(C, packed(8))]\n#[derive(Debug, Default, Copy, Clone)]\npub struct bch_extent_crc128 {",
                 "#[repr(C, align(8))]\n#[derive(Debug, Default, Copy, Clone)]\npub struct bch_extent_crc128 {")
        .replace("#[repr(C, packed(8))]\npub struct jset {",
                 "#[repr(C, align(8))]\npub struct jset {")
        .replace("#[repr(C, packed(8))]\npub struct btree_node_entry {",
                 "#[repr(C, align(8))]\npub struct btree_node_entry {")
        .replace("#[repr(C, packed(8))]\npub struct bch_sb {",
                 "#[repr(C, align(8))]\npub struct bch_sb {");

    if ptr_width != "32" {
        return bindings;
    }
    let mut result = String::with_capacity(bindings.len());
    let mut lines = bindings.lines().peekable();
    while let Some(line) = lines.next() {
        if line == "#[repr(C)]" {
            let needs_align8 = lines.peek().is_some_and(|n|
                n.contains("pub struct bkey_i_")
                || n.contains("pub struct btree_node__bindgen_ty_1")
                || n.contains("pub struct btree_node_entry__bindgen_ty_1")
                || n.contains("pub struct bch_ioctl_query_accounting"));
            result.push_str(if needs_align8 { "#[repr(C, align(8))]" } else { line });
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    result
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
