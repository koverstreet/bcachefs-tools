fn main() {
    let ac = autocfg::new();

    ac.emit_expression_cfg("1f64.total_cmp(&2f64)", "has_total_cmp"); // 1.62

    // bcachefs: set by the kernel build, whose core has no float parsing
    println!("cargo:rustc-check-cfg=cfg(no_fp_fmt_parse)");

    autocfg::rerun_path("build.rs");
}
