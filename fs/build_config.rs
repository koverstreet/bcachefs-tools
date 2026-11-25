// The userspace build's configuration, as Rust cfgs - what the kernel's
// rustc_cfg makes of a kernel build's .config: USERSPACE_CONFIG, and the -D/-U
// in EXTRA_CFLAGS that make adds to it (make debug's CONFIG_BCACHEFS_DEBUG, or
// the user's). fs/build.rs's.

/// What the userspace build sets.
const USERSPACE_CONFIG: &[&str] = &["CONFIG_DEBUG_FS", "CONFIG_STACKTRACE", "CONFIG_UNICODE"];

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
const PER_BUILD_CONFIG: &[&str] = &["CONFIG_BCACHEFS_DEBUG"];

/// The kernel's options the types and the kernel-only code test, which no
/// userspace build sets: declared too, as the kernel build has them.
const KERNEL_CONFIG: &[&str] = &[
    "CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS",
    "CONFIG_BCACHEFS_LOCK_TIME_STATS",
    "CONFIG_BCACHEFS_NO_LATENCY_ACCT",
    "CONFIG_BCACHEFS_PATH_TRACEPOINTS",
    "CONFIG_BCACHEFS_QUOTA",
    "CONFIG_BCACHEFS_TESTS",
    "CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE",
    "CONFIG_DEBUG_LOCK_ALLOC",
    "CONFIG_HAVE_ARCH_PREL32_RELOCATIONS",
    "CONFIG_HIGHMEM",
    "CONFIG_JUMP_LABEL",
    "CONFIG_LOCKDEP",
    "CONFIG_POWER_SUPPLY",
    "CONFIG_PROVE_LOCKING",
    "CONFIG_SMP",
    "CONFIG_TRACEPOINTS",
    "CONFIG_TRANSPARENT_HUGEPAGE",
    "MODULE",
];

/// The configuration's cfgs: every name USERSPACE_CONFIG or EXTRA_CFLAGS
/// mentions is declared, set or not, and so are PER_BUILD_CONFIG and
/// KERNEL_CONFIG.
fn emit_userspace_config_cfgs() {
    println!("cargo:rerun-if-env-changed=EXTRA_CFLAGS");

    let mut set: Vec<String> = USERSPACE_CONFIG.iter().map(|n| n.to_string()).collect();
    let mut names = set.clone();
    names.extend(PER_BUILD_CONFIG.iter().chain(KERNEL_CONFIG).map(|n| n.to_string()));

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
