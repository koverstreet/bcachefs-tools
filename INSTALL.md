Getting started
---------------

Build dependencies:

 * libaio
 * libblkid
 * libclang
 * libkeyutils
 * liblz4
 * libsodium
 * liburcu
 * libuuid
 * libzstd
 * pkg-config
 * valgrind
 * zlib1g

In addition a recent Rust toolchain is required (rustc, cargo), either by using
[rustup](https://rustup.rs/) or make sure to use a distribution where a recent
enough rustc is available. Please check `rust-version` in `Cargo.toml` to see
the minimum supported Rust version (MSRV).

``` shell
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
```

Debian (Bullseye or later) and Ubuntu (20.04 or later): you can install these with

``` shell
apt install -y pkg-config libaio-dev libblkid-dev libkeyutils-dev \
    liblz4-dev libsodium-dev liburcu-dev libzstd-dev \
    uuid-dev zlib1g-dev valgrind libudev-dev udev git build-essential \
    python3 python3-docutils libclang-dev debhelper dh-python
```

Starting from Debian Trixie and Ubuntu 23.10, you will additionally need:
```shell
apt install -y systemd-dev
```

Fedora: install build dependencies either with `dnf builddep bcachefs-tools` or with:
```shell
dnf install -y @c-development libaio-devel libsodium-devel \
    libblkid-devel libzstd-devel zlib-devel userspace-rcu-devel \
    lz4-devel libuuid-devel valgrind-devel keyutils-libs-devel \
    findutils systemd-devel clang-devel llvm-devel rust cargo
```

openSUSE: install build dependencies with:
```shell
zypper in -y libaio-devel libsodium-devel libblkid-devel liburcu-devel \
    libzstd-devel zlib-devel liblz4-devel libuuid-devel valgrind-devel \
    keyutils-devel findutils udev systemd-devel llvm-devel
```

Arch: install bcachefs-tools-git from the AUR.
Or to build from source, install build dependencies with
```shell
pacman -S base-devel libaio keyutils libsodium liburcu zstd valgrind llvm
```

Then, just `make && make install`

Nixos setup with flakes
-----------------------

To use snapshot version of bcachefs & bcachefs-tools, add the following to your `flake.nix`:
```nix
inputs = {
  nixpkgs = {
    url = "github:nixos/nixpkgs";
  };
  ...
  bcachefs-tools = {
    url = "github:koverstreet/bcachefs-tools";
    # Optional but recommended to limit the size of your system closure.
    inputs.nixpkgs.follows = "nixpkgs";
  };
...
outputs =
   {
     self,
     nixpkgs,
     ...
     bcachefs-tools,
     ...
   }@attrs:
   let
     eachSystem = nixpkgs.lib.genAttrs (import systems) (system: nixpkgs.legacyPackages.${system});
   in
   {
     nixosConfigurations."YourPC" = nixpkgs.lib.nixosSystem {
       ...
       modules = [
         ...
         bcachefs-tools.nixosModules.default
         ...
       ];
     };
```
Note that this not only pulls in the latest bcachefs-tools, but also replaces
your system's bcachefs kernel module with the snapshot version.

Experimental features
---------------------

Experimental fuse support is currently disabled by default. Fuse support is at
an early stage and may corrupt your filesystem, so it should only be used for
testing. To enable, you'll also need to add:

* libfuse3 >= 3.7

On Debian/Ubuntu (Bullseye/20.04 or later needed for libfuse >= 3.7):
```shell
apt install -y libfuse3-dev
```

On Fedora (32 or later needed for libfuse >= 3.7):
```shell
dnf install -y fuse3-devel
```

Arch:
```shell
pacman -S fuse3
```

openSUSE:
```shell
zypper in -y fuse3-devel
```

Then, make using the `BCACHEFS_FUSE` environment variable (make clean first if
previously built without fuse support):

```shell
BCACHEFS_FUSE=1 make && make install
```
