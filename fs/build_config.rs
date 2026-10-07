// The userspace build's configuration, for build scripts: c_src/autoconf.h,
// and the -D/-U in EXTRA_CFLAGS that make adds to it (make debug's
// CONFIG_BCACHEFS_DEBUG, or the user's). include!d by every build script that
// runs bindgen or compiles C against the fs/ headers, so they all see what the
// Makefile's C compiles see. See c_src/autoconf.h.
//
// Not every includer needs every function - codegen_main.rs, a standalone
// tool, wants only the args - hence the dead_code allows.

/// The clang args that configure a compile like the Makefile's: the header,
/// force-included, then EXTRA_CFLAGS's defines.
fn userspace_config_args(root: &std::path::Path) -> Vec<String> {
    let header = root.join("c_src/autoconf.h");
    let mut args = vec!["-include".to_string(), header.display().to_string()];
    args.extend(extra_cflags_defines());
    args
}

/// For a build script using userspace_config_args(): rerun when the
/// configuration changes.
#[allow(dead_code)]
fn rerun_if_userspace_config_changed(root: &std::path::Path) {
    println!("cargo:rerun-if-changed={}", root.join("c_src/autoconf.h").display());
    println!("cargo:rerun-if-env-changed=EXTRA_CFLAGS");
}

/// The -DFOO[=val] and -UFOO in EXTRA_CFLAGS, which make exports - not set
/// when cargo is run directly, which builds the plain configuration.
fn extra_cflags_defines() -> Vec<String> {
    std::env::var("EXTRA_CFLAGS")
        .unwrap_or_default()
        .split_whitespace()
        .filter(|f| f.starts_with("-D") || f.starts_with("-U"))
        .map(String::from)
        .collect()
}

/// The CONFIG_* names a build can add through EXTRA_CFLAGS - make debug's -
/// declared as cfgs whether or not this one does, so code conditional on
/// them builds without them.
const PER_BUILD_CONFIG: &[&str] = &["CONFIG_BCACHEFS_DEBUG", "CONFIG_VALGRIND"];

/// Make the configuration's CONFIG_* defines Rust cfgs, as the kernel's
/// rustc_cfg does for a kernel build: cfg(CONFIG_UNICODE) where C has
/// CONFIG_UNICODE. Every name autoconf.h or EXTRA_CFLAGS mentions is
/// declared, set or not, and so is PER_BUILD_CONFIG.
#[allow(dead_code)]
fn emit_userspace_config_cfgs(root: &std::path::Path) {
    let header = root.join("c_src/autoconf.h");
    let text = std::fs::read_to_string(&header)
        .unwrap_or_else(|e| panic!("reading {}: {e}", header.display()));

    let mut set: Vec<String> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("#define"))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| n.starts_with("CONFIG_"))
        .map(String::from)
        .collect();
    let mut names = set.clone();
    names.extend(PER_BUILD_CONFIG.iter().map(|n| n.to_string()));

    for f in extra_cflags_defines() {
        let (undef, def) = match f.strip_prefix("-U") {
            Some(n) => (true, n),
            None    => (false, &f[2..]),
        };
        let name = def.split('=').next().unwrap_or(def).to_string();
        if !name.starts_with("CONFIG_") {
            continue;
        }
        names.push(name.clone());
        set.retain(|n| *n != name);
        if !undef {
            set.push(name);
        }
    }

    for n in &names {
        println!("cargo::rustc-check-cfg=cfg({n})");
    }
    for n in &set {
        println!("cargo::rustc-cfg={n}");
    }
}
