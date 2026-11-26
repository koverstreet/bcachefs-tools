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

- `make` gives cargo the git version, as BCACHEFS_VERSION; a plain
  `cargo build`'s `bcachefs version` is Cargo.toml's.
