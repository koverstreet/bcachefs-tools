fn clang_target_for_rust_target(target: &str) -> &str {
    match target {
        "riscv64gc-unknown-linux-gnu" => "riscv64-unknown-linux-gnu",
        "riscv32gc-unknown-linux-gnu" => "riscv32-unknown-linux-gnu",
        _ => target,
    }
}
