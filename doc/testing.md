# Testing bcachefs

## ktest

Tests live in `~/ktest/tests/fs/bcachefs/` (e.g. `subvol.ktest`).

```bash
# Run a test
btk run -IP ~/ktest/tests/fs/bcachefs/<file>.ktest <test_name>
```

- Test functions in the file are named `test_*`; drop the `test_` prefix
  when running (e.g. `test_foo` in the file becomes `btk run ... foo`).
- Prefix a test function with `d_` to disable it.
- Source `bcachefs-test-libs.sh` for helpers (`config-scratch-devs`,
  `bcachefs_antagonist`, etc.).
- End tests with `bcachefs_test_end_checks` for fsck.
- ktest uses its own build output dir (`ktest-out/kernel_build.$arch`),
  not `.linux`.
- With `-I` (interactive), the VM stays running after the test
  completes — must `C-c` to kill before running another.
