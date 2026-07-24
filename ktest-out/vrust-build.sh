#!/bin/sh
# SCRATCH driver: prove the full vendored rust stack builds into $obj (writable),
# reading the RUST=n kernel store entry READ-ONLY. Mirrors the proven `make rust/`
# commands (from ~/linux-6.18/rust/.*.cmd), output redirected to $obj. Once green,
# the exact commands port into fs/Makefile rules. NOT the final artifact.
set -e

bt=/home/kent/.ktest/kernels/upstream/stable_618-default/x86_64/6.18.37/lib/modules/6.18.37-ktest/build
vendor=/home/kent/ktest/tests/fs/bcachefs/bcachefs-tools/fs/vendor/kernel-rust
obj=/home/kent/ktest/tests/fs/bcachefs/bcachefs-tools/ktest-out/vrust-proof
RUSTC=/nix/store/p9dz0875g5v4x87hrmpsfpq9aq1wwcfv-rust-minimal-1.96.0/bin/rustc
LIBSRC=/nix/store/p9dz0875g5v4x87hrmpsfpq9aq1wwcfv-rust-minimal-1.96.0/lib/rustlib/src/rust/library
BINDGEN=/nix/store/7av2pli48lhqvdwzvvxv7sdlgrmz04l4-rust-bindgen-0.72.1/bin/bindgen
export RUSTC_BOOTSTRAP=1
mkdir -p "$obj"

# Kernel target flags (= the populated KBUILD_RUSTFLAGS on the RUST=n tree) + rustc_cfg (read-only).
TFLAGS="-Cpanic=abort -Cembed-bitcode=n -Clto=n -Cforce-unwind-tables=n -Ccodegen-units=1 \
 -Csymbol-mangling-version=v0 -Crelocation-model=static -Zfunction-sections=n \
 --target=$bt/scripts/target.json \
 -Ctarget-feature=-sse,-sse2,-sse3,-ssse3,-sse4.1,-sse4.2,-avx,-avx2 \
 -Zcf-protection=branch -Cjump-tables=n -Ctarget-cpu=x86-64 -Ztune-cpu=generic \
 -Cno-redzone=y -Ccode-model=kernel -Copt-level=2 -Awarnings -Zunstable-options \
 @$bt/include/generated/rustc_cfg --sysroot=/dev/null -L$obj"

# Host proc-macro flags (run on the host; use the host std sysroot, NOT /dev/null).
PFLAGS="-Awarnings -Zunstable-options -L$obj @$bt/include/generated/rustc_cfg --extern proc_macro --crate-type proc-macro"
HFLAGS="-Awarnings -Zunstable-options -L$obj --crate-type rlib"

lib()  { # name edition src extra...
  n=$1 ed=$2 src=$3; shift 3
  echo "  RUSTC L $n"
  $RUSTC $TFLAGS --edition=$ed --crate-type rlib --crate-name "$n" \
    --emit=metadata=$obj/lib$n.rmeta --emit=obj=$obj/$n.o "$@" "$src"
}
host() { # name edition src extra...
  n=$1 ed=$2 src=$3; shift 3
  echo "  RUSTC H $n"
  $RUSTC $HFLAGS --edition=$ed --crate-name "$n" \
    --emit=metadata=$obj/lib$n.rmeta --emit=link=$obj/lib$n.rlib "$@" "$src"
}
pmac() { # name edition src extra...
  n=$1 ed=$2 src=$3; shift 3
  echo "  RUSTC P $n"
  $RUSTC $PFLAGS --edition=$ed --crate-name "$n" \
    --emit=link=$obj/lib$n.so "$@" "$src"
}

echo "=== host crates + proc-macros ==="
host proc_macro2 2021 "$vendor/proc-macro2/lib.rs" \
  --cfg 'feature="proc-macro"' --cfg wrap_proc_macro --cfg proc_macro_span_file --cfg proc_macro_span_location
host quote 2018 "$vendor/quote/lib.rs" --cfg 'feature="proc-macro"' --extern proc_macro2=$obj/libproc_macro2.rlib
host syn 2021 "$vendor/syn/lib.rs" \
  --cfg 'feature="clone-impls"' --cfg 'feature="derive"' --cfg 'feature="full"' --cfg 'feature="parsing"' \
  --cfg 'feature="printing"' --cfg 'feature="proc-macro"' --cfg 'feature="visit"' --cfg 'feature="visit-mut"' \
  --extern proc_macro2=$obj/libproc_macro2.rlib --extern quote=$obj/libquote.rlib
PME="--extern proc_macro2=$obj/libproc_macro2.rlib --extern quote=$obj/libquote.rlib --extern syn=$obj/libsyn.rlib"
pmac macros 2021 "$vendor/macros/lib.rs" $PME
pmac pin_init_internal 2021 "$vendor/pin-init/internal/src/lib.rs" --cfg kernel --cfg USE_RUSTC_FEATURES $PME
pmac zerocopy_derive 2021 "$vendor/zerocopy-derive/lib.rs" $PME

echo "=== core + compiler_builtins (the no-std base) ==="
lib core 2024 "$LIBSRC/core/src/lib.rs" --cfg no_fp_fmt_parse
# redirect-intrinsics: route core's soft-float/u128 intrinsics to __rust* (stubbed by compiler_builtins)
objcopy $(for s in __addsf3 __eqsf2 __extendsfdf2 __gesf2 __lesf2 __ltsf2 __mulsf3 __nesf2 __truncdfsf2 \
  __unordsf2 __adddf3 __eqdf2 __ledf2 __ltdf2 __muldf3 __unorddf2 __muloti4 __multi3 __udivmodti4 \
  __udivti3 __umodti3; do printf -- "--redefine-sym %s=__rust%s " "$s" "$s"; done) "$obj/core.o"
lib compiler_builtins 2021 "$vendor/compiler_builtins.rs" --extern core=$obj/libcore.rmeta
objcopy -w -W '__*' "$obj/compiler_builtins.o"

echo "=== leaf crates ==="
CE="--extern core=$obj/libcore.rmeta --extern compiler_builtins=$obj/libcompiler_builtins.rmeta"
lib ffi 2021 "$vendor/ffi.rs" $CE
lib build_error 2021 "$vendor/build_error.rs" $CE
CARGO_PKG_VERSION=0.8.50 \
lib zerocopy 2021 "$vendor/zerocopy/src/lib.rs" $CE --cap-lints=allow --extern zerocopy_derive=$obj/libzerocopy_derive.so
lib pin_init 2021 "$vendor/pin-init/src/lib.rs" $CE --cfg kernel --cfg USE_RUSTC_FEATURES \
  --extern pin_init_internal=$obj/libpin_init_internal.so --extern macros=$obj/libmacros.so

echo "=== DONE (foundation). bindings/uapi/kernel need bindgen + RSCPP — next phase. ==="
ls -1 "$obj"/*.rmeta | sed 's#.*/##'
