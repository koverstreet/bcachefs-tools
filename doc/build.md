# Building bcachefs-tools

## Quick reference

```bash
# Rust only (fast iteration on Rust changes)
cargo build

# Full build (needed after C changes)
make -j -k

# Kernel build (from kernel tree)
make -k -C ../.. O=.linux EXTRA_CFLAGS=-Werror W=1 fs/bcachefs/
```

## Notes

- `cargo build` only rebuilds Rust code. C code requires `make`.
- C `.o` files are ephemeral: `make` compiles `.c` to `.o`, archives
  into `libbcachefs.a`, then `.o` may be deleted. After editing C files,
  use `touch c_src/foo.c && make -j` to force recompilation.
- To verify a symbol was compiled correctly:
  `objdump -t libbcachefs.a | grep symbol_name`
