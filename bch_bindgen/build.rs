/// Parse an x-macro from a C header file.
///
/// Finds `#define {macro_name}(...)` and extracts all `x(...)` invocations,
/// returning each as a vec of trimmed argument strings.  Handles nested
/// parentheses in arguments (e.g. `BIT_ULL(KEY_TYPE_foo)`).
fn parse_xmacro(header: &str, macro_name: &str) -> Vec<Vec<String>> {
    let define_prefix = format!("#define {}", macro_name);
    let mut in_macro = false;
    let mut macro_text = String::new();

    for line in header.lines() {
        let trimmed = line.trim();
        if !in_macro {
            if trimmed.starts_with(&define_prefix) {
                in_macro = true;
                // grab any content after the macro signature on this line
                if let Some(pos) = trimmed.find(&define_prefix) {
                    let after = &trimmed[pos + define_prefix.len()..];
                    // skip past optional parameter list
                    let after = if let Some(i) = after.find(')') {
                        &after[i + 1..]
                    } else {
                        after
                    };
                    macro_text.push_str(after.trim_end_matches('\\').trim());
                    macro_text.push(' ');
                }
                if !trimmed.ends_with('\\') {
                    break;
                }
            }
        } else {
            macro_text.push_str(trimmed.trim_end_matches('\\').trim());
            macro_text.push(' ');
            if !trimmed.ends_with('\\') {
                break;
            }
        }
    }

    // extract x(...) calls, respecting nested parens
    let mut entries = Vec::new();
    let bytes = macro_text.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        let Some(start) = macro_text[pos..].find("x(") else { break };
        let open = pos + start + 2;
        let mut depth = 1usize;
        let mut i = open;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            if depth > 0 { i += 1; }
        }
        if depth == 0 {
            entries.push(split_xmacro_args(&macro_text[open..i]));
            pos = i + 1;
        } else {
            break;
        }
    }
    entries
}

/// Split a comma-separated argument list, respecting nested parentheses.
fn split_xmacro_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut depth = 0;
    let mut current = String::new();

    for ch in s.chars() {
        match ch {
            '(' => { depth += 1; current.push(ch); }
            ')' => { depth -= 1; current.push(ch); }
            ',' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() {
        args.push(tail);
    }
    args
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
        out.push_str(&format!(
            "unsafe impl SbField for c::bch_sb_field_{name} {{\n\
             \x20   const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type::BCH_SB_FIELD_{name};\n\
             }}\n\n"
        ));
    }

    out
}

fn generate_str_table(name: &str, entries: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated — do not edit\n\n");
    out.push_str(&format!("pub const {name}: &[&str] = &[\n"));
    for e in entries {
        out.push_str(&format!("    \"{}\",\n", e[0]));
    }
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
        let flags = &e[2];
        let is_sectors = flags.contains("TYPE_SECTORS");
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
    out.push_str("    use std::mem::size_of;\n");
    out.push_str("    Some(match ty {\n");
    for e in entries {
        let name = &e[0];
        let n = &e[1];
        out.push_str(&format!(
            "        {n} => size_of::<c::bch_extent_{name}>() / 8,\n"
        ));
    }
    out.push_str("        _ => return None,\n");
    out.push_str("    })\n");
    out.push_str("}\n");
    out
}

fn generate_bkey_types(entries: &[Vec<String>]) -> String {
    let mut out = String::new();

    out.push_str("// Auto-generated from BCH_BKEY_TYPES() — do not edit\n\n");

    // ---- accessor methods on bkey_i_* types ----
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

    // ---- BkeyValI: inline typed bkey dispatch ----
    out.push_str("/// Typed dispatch for inline bkeys (`bkey_i`).\n");
    out.push_str("pub enum BkeyValI<'a> {\n");
    for e in entries {
        out.push_str(&format!("    {}(&'a c::bkey_i_{}),\n", e[0], e[0]));
    }
    out.push_str("    unknown(&'a c::bkey_i),\n");
    out.push_str("}\n\n");

    out.push_str("impl<'a> BkeyValI<'a> {\n");
    out.push_str("    #[allow(clippy::missing_transmute_annotations)]\n");
    out.push_str("    pub fn from_bkey_i(k: &'a c::bkey_i) -> Self {\n");
    out.push_str("        match k.k.type_ as u32 {\n");
    for e in entries {
        out.push_str(&format!(
            "            {} => BkeyValI::{}(unsafe {{ std::mem::transmute(k) }}),\n",
            e[1], e[0]
        ));
    }
    out.push_str("            _ => BkeyValI::unknown(k),\n");
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    // ---- BkeyValIMut: mutable inline typed bkey dispatch ----
    out.push_str("/// Typed dispatch for mutable inline bkeys (`bkey_i`).\n");
    out.push_str("pub enum BkeyValIMut<'a> {\n");
    for e in entries {
        out.push_str(&format!("    {}(&'a mut c::bkey_i_{}),\n", e[0], e[0]));
    }
    out.push_str("    unknown(&'a mut c::bkey_i),\n");
    out.push_str("}\n\n");

    out.push_str("impl<'a> BkeyValIMut<'a> {\n");
    out.push_str("    #[allow(clippy::missing_transmute_annotations)]\n");
    out.push_str("    pub fn from_bkey_i(k: &'a mut c::bkey_i) -> Self {\n");
    out.push_str("        let type_ = k.k.type_;\n");
    out.push_str("        match type_ as u32 {\n");
    for e in entries {
        out.push_str(&format!(
            "            {} => BkeyValIMut::{}(unsafe {{ std::mem::transmute(k) }}),\n",
            e[1], e[0]
        ));
    }
    out.push_str("            _ => BkeyValIMut::unknown(k),\n");
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    // ---- BkeyValSC: split const typed dispatch ----
    out.push_str("/// Typed dispatch for split-const bkey references.\n");
    out.push_str("pub enum BkeyValSC<'a> {\n");
    for e in entries {
        out.push_str(&format!("    {}(&'a c::bkey, &'a c::bch_{}),\n", e[0], e[0]));
    }
    out.push_str("    unknown(&'a c::bkey, u8),\n");
    out.push_str("}\n\n");

    out.push_str("impl<'a> BkeyValSC<'a> {\n");
    out.push_str("    #[allow(clippy::missing_transmute_annotations)]\n");
    out.push_str("    pub fn from_bkey_i(k: &'a c::bkey_i) -> Self {\n");
    out.push_str("        match k.k.type_ as u32 {\n");
    for e in entries {
        out.push_str(&format!(
            "            {} => BkeyValSC::{}(&k.k, unsafe {{ std::mem::transmute(&k.v) }}),\n",
            e[1], e[0]
        ));
    }
    out.push_str("            _ => BkeyValSC::unknown(&k.k, k.k.type_),\n");
    out.push_str("        }\n");
    out.push_str("    }\n\n");

    // from raw (k, v) pointers — used by BkeySC and btree iteration
    out.push_str("    /// Construct from raw key and value references.\n");
    out.push_str("    ///\n");
    out.push_str("    /// # Safety\n");
    out.push_str("    /// `val` must point to valid data for the bkey type indicated by `k.type_`.\n");
    out.push_str("    #[allow(clippy::missing_transmute_annotations)]\n");
    out.push_str("    pub unsafe fn from_raw(k: &'a c::bkey, val: &'a c::bch_val) -> Self {\n");
    out.push_str("        match k.type_ as u32 {\n");
    for e in entries {
        out.push_str(&format!(
            "            {} => BkeyValSC::{}(k, std::mem::transmute(val)),\n",
            e[1], e[0]
        ));
    }
    out.push_str("            _ => BkeyValSC::unknown(k, k.type_),\n");
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    // ---- BkeyValS: split mutable typed dispatch ----
    out.push_str("/// Typed dispatch for split-mutable bkey references.\n");
    out.push_str("pub enum BkeyValS<'a> {\n");
    for e in entries {
        out.push_str(&format!("    {}(&'a mut c::bkey, &'a mut c::bch_{}),\n", e[0], e[0]));
    }
    out.push_str("    unknown(&'a mut c::bkey, u8),\n");
    out.push_str("}\n\n");

    out.push_str("impl<'a> BkeyValS<'a> {\n");
    out.push_str("    #[allow(clippy::missing_transmute_annotations)]\n");
    out.push_str("    pub fn from_bkey_i(k: &'a mut c::bkey_i) -> Self {\n");
    out.push_str("        let type_ = k.k.type_;\n");
    out.push_str("        match type_ as u32 {\n");
    for e in entries {
        out.push_str(&format!(
            "            {} => BkeyValS::{}(&mut k.k, unsafe {{ std::mem::transmute(&mut k.v) }}),\n",
            e[1], e[0]
        ));
    }
    out.push_str("            _ => BkeyValS::unknown(&mut k.k, type_),\n");
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n");

    out
}

#[derive(Debug)]
pub struct Fix753 {}
impl bindgen::callbacks::ParseCallbacks for Fix753 {
    fn item_name(&self, original_item_name: &str) -> Option<String> {
        Some(original_item_name.trim_start_matches("Fix753_").to_owned())
    }
}

fn main() {
    use std::path::PathBuf;

    println!("cargo:rerun-if-changed=src/libbcachefs_wrapper.h");
    println!("cargo:rerun-if-changed=../libbcachefs/bcachefs_format.h");
    println!("cargo:rerun-if-changed=../libbcachefs/sb/members_format.h");
    println!("cargo:rerun-if-changed=../libbcachefs/data/extents_format.h");
    println!("cargo:rerun-if-changed=../libbcachefs/sb/counters_format.h");

    let out_dir: PathBuf = std::env::var_os("OUT_DIR")
        .expect("ENV Var 'OUT_DIR' Expected")
        .into();
    let top_dir: PathBuf = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("ENV Var 'CARGO_MANIFEST_DIR' Expected")
        .into();

    let urcu = pkg_config::probe_library("liburcu").expect("Failed to find urcu lib");
    let bindings = bindgen::builder()
        .formatter(bindgen::Formatter::Prettyplease)
        .header(
            top_dir
                .join("src")
                .join("libbcachefs_wrapper.h")
                .display()
                .to_string(),
        )
        .clang_args(
            urcu.include_paths
                .iter()
                .map(|p| format!("-I{}", p.display())),
        )
        .clang_arg("-I..")
        .clang_arg("-I../libbcachefs")
        .clang_arg("-I../c_src")
        .clang_arg("-I../include")
        .clang_arg("-DZSTD_STATIC_LINKING_ONLY")
        .clang_arg("-DNO_BCACHEFS_FS")
        .clang_arg("-D_GNU_SOURCE")
        .clang_arg("-DRUST_BINDGEN")
        .clang_arg("-fkeep-inline-functions")
        .derive_debug(true)
        .derive_default(true)
        .layout_tests(true)
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: true,
        })
        .bitfield_enum("btree_iter_update_trigger_flags")
        .allowlist_function("raid_init")
        .allowlist_function("linux_shrinkers_init")
        .allowlist_function("sysfs_.*")
        .allowlist_var("linux_page_size")
        .allowlist_function("cmd_.*")
        .allowlist_function(".*_cmds")
        .allowlist_function(".*bch2_.*")
        .allowlist_function("bio_.*")
        .allowlist_function("derive_passphrase")
        .allowlist_function("request_key")
        .allowlist_function("add_key")
        .allowlist_function("keyctl_search")
        .allowlist_function("match_string")
        .allowlist_function("printbuf.*")
        .allowlist_function("_bch2_err_matches")
        // tools-util functions for Rust command conversions
        .allowlist_function("open_for_format")
        .allowlist_function("ask_yn")
        .allowlist_function("read_file_str")
        .allowlist_function("read_file_u64")
        .allowlist_function("get_size")
        .allowlist_function("fd_to_dev_model")
        .allowlist_function("copy_fs")
        .allowlist_function("rust_fmt_build_fs")
        .allowlist_function("rust_strip_alloc_check")
        .allowlist_function("rust_strip_alloc_do")
        .allowlist_function("bch2_dev_resize")
        .allowlist_function("bch2_set_nr_journal_buckets")
        .allowlist_function("rust_csum_vstruct_sb")
        .allowlist_function("rust_collect_journal_entries")
        .allowlist_function("get_blocksize")
        .allowlist_function("bch_sb_crypt_init")
        .allowlist_function("bdev_nonrot")
        .allowlist_function("read_passphrase")
        .blocklist_function("bch2_prt_vprintf")
        .blocklist_type("rhash_lock_head")
        .blocklist_type("srcu_struct")
        .blocklist_type("bch_ioctl_data_event")
        .allowlist_var("BCH_.*")
        .allowlist_var("KEY_SPEC_.*")
        .allowlist_var("Fix753_.*")
        .allowlist_var("bch.*")
        .allowlist_var("__bch2.*")
        .allowlist_var("__BTREE_ITER.*")
        .allowlist_var("BTREE_ITER.*")
        .blocklist_item("bch2_bkey_ops")
        .allowlist_type("bch_.*")
        .allowlist_type("bkey_i_.*")
        .allowlist_type("bkey_s_c_.*")
        .allowlist_type("bkey_s_.*")
        .allowlist_type("disk_accounting_type")
        .allowlist_type("fsck_err_opts")
        .rustified_enum("fsck_err_opts")
        .allowlist_type("nonce")
        .no_debug("bch_replicas_padded")
        .no_debug("jset")
        .no_debug("bch_replicas_entry_cpu")
        .newtype_enum("bch_kdf_types")
        .rustified_enum("bch_key_types")
        .opaque_type("gendisk")
        .opaque_type("gc_stripe")
        .opaque_type("open_bucket.*")
        .opaque_type("replicas_delta_list")
        .allowlist_type("sb_names")
        .no_copy("btree_trans")
        .no_copy("printbuf")
        .no_copy("bch_sb_handle")
        .no_partialeq("bkey")
        .no_partialeq("bpos")
        .generate_inline_functions(true)
        .parse_callbacks(Box::new(Fix753 {}))
        .generate()
        .expect("BindGen Generation Failiure: [libbcachefs_wrapper]");

    std::fs::write(
        out_dir.join("bcachefs.rs"),
        packed_and_align_fix(bindings.to_string()),
    )
    .expect("Writing to output file failed for: `bcachefs.rs`");

    // Generate from x-macros in bcachefs_format.h
    let format_h = std::fs::read_to_string(top_dir.join("../libbcachefs/bcachefs_format.h"))
        .expect("reading bcachefs_format.h");

    let bkey_types = parse_xmacro(&format_h, "BCH_BKEY_TYPES");
    assert!(!bkey_types.is_empty(), "failed to parse BCH_BKEY_TYPES()");
    std::fs::write(
        out_dir.join("bkey_types_gen.rs"),
        generate_bkey_types(&bkey_types),
    )
    .expect("Writing bkey_types_gen.rs");

    let sb_fields = parse_xmacro(&format_h, "BCH_SB_FIELDS");
    assert!(!sb_fields.is_empty(), "failed to parse BCH_SB_FIELDS()");
    std::fs::write(
        out_dir.join("sb_field_types_gen.rs"),
        generate_sb_field_impls(&sb_fields),
    )
    .expect("Writing sb_field_types_gen.rs");

    let members_h = std::fs::read_to_string(top_dir.join("../libbcachefs/sb/members_format.h"))
        .expect("reading members_format.h");
    let member_states = parse_xmacro(&members_h, "BCH_MEMBER_STATES");
    assert!(!member_states.is_empty(), "failed to parse BCH_MEMBER_STATES()");
    std::fs::write(
        out_dir.join("member_states_gen.rs"),
        generate_str_table("MEMBER_STATE_NAMES", &member_states),
    )
    .expect("Writing member_states_gen.rs");

    let counters_h = std::fs::read_to_string(top_dir.join("../libbcachefs/sb/counters_format.h"))
        .expect("reading counters_format.h");
    let counters = parse_xmacro(&counters_h, "BCH_PERSISTENT_COUNTERS");
    assert!(!counters.is_empty(), "failed to parse BCH_PERSISTENT_COUNTERS()");
    std::fs::write(
        out_dir.join("counters_gen.rs"),
        generate_counter_table(&counters),
    )
    .expect("Writing counters_gen.rs");

    let extents_h = std::fs::read_to_string(top_dir.join("../libbcachefs/data/extents_format.h"))
        .expect("reading extents_format.h");
    let extent_entry_types = parse_xmacro(&extents_h, "BCH_EXTENT_ENTRY_TYPES");
    assert!(!extent_entry_types.is_empty(), "failed to parse BCH_EXTENT_ENTRY_TYPES()");
    std::fs::write(
        out_dir.join("extent_entry_types_gen.rs"),
        generate_extent_entry_u64s(&extent_entry_types),
    )
    .expect("Writing extent_entry_types_gen.rs");

    let keyutils = pkg_config::probe_library("libkeyutils").expect("Failed to find keyutils lib");
    let bindings = bindgen::builder()
        .header(
            top_dir
                .join("src")
                .join("keyutils_wrapper.h")
                .display()
                .to_string(),
        )
        .clang_args(
            keyutils
                .include_paths
                .iter()
                .map(|p| format!("-I{}", p.display())),
        )
        .generate()
        .expect("BindGen Generation Failiure: [Keyutils]");
    bindings
        .write_to_file(out_dir.join("keyutils.rs"))
        .expect("Writing to output file failed for: `keyutils.rs`");
}

// rustc has a limitation where it does not allow structs with a "packed" attribute to contain a
// member with an "align(N)" attribute. There are a few types in bcachefs with this problem. We can
// "fix" these types by stripping off "packed" from the outer type, or "align(N)" from the inner
// type. For all of the affected types, stripping "packed" from the outer type happens to preserve
// the same layout in Rust as in C.
//
// Some types are only affected on attributes on architectures where the natural alignment of u64
// is 4 instead of 8, for example i686 or ppc64: struct bch_csum and struct bch_sb_layout have
// "align(8)" added on such architecutres. These types are included by several "packed" types:
//   - bch_extent_crc128
//   - jset
//   - btree_node_entry
//   - bch_sb
//
// TODO: find a way to conditionally include arch-specific modifications when compiling for that
// target arch. Regular conditional compilation won't work here since build scripts are always
// compiled for the host arch, not the target arch, so that won't work when cross-compiling.
fn packed_and_align_fix(bindings: std::string::String) -> std::string::String {
    bindings
        .replace(
            "#[repr(C, packed(8))]\npub struct btree_node {",
            "#[repr(C, align(8))]\npub struct btree_node {",
        )
        .replace(
            "#[repr(C, packed(8))]\n#[derive(Debug, Default, Copy, Clone)]\npub struct bch_extent_crc128 {",
            "#[repr(C, align(8))]\n#[derive(Debug, Default, Copy, Clone)]\npub struct bch_extent_crc128 {",
        )
        .replace(
            "#[repr(C, packed(8))]\npub struct jset {",
            "#[repr(C, align(8))]\npub struct jset {",
        )
        .replace(
            "#[repr(C, packed(8))]\npub struct btree_node_entry {",
            "#[repr(C, align(8))]\npub struct btree_node_entry {",
        )
        .replace(
            "#[repr(C, packed(8))]\npub struct bch_sb {",
            "#[repr(C, align(8))]\npub struct bch_sb {",
        )
}
