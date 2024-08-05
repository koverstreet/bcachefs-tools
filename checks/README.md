# NixOS tests

Any `*.sh` file in this directory will be run in a NixOS VM for basic
functionality testing as part of CI. To list all outputs, including the checks,
you can use this command:

```console
$ nix flake show
```

You can also run these tests locally by running `nix flake check`. To run one
specific test you can use `nix build` like this:

```console
$ nix build ".#checks.x86_64-linux.subvolume"
```

With the flag `-L`/`--print-build-logs` outputs are shown fully as checks are
executing. Additionally, if the specific check has already been run locally, you
can view the log for the check or force another run with the following:

```console
$ nix log .#checks.x86_64-linux.subvolume
$ nix build --rebuild .#checks.x86_64-linux.subvolume
```

If you need any more packages inside of the VM for a test, you can add them to
`environment.systemPackages` in `default.nix`. If you're unsure about the
package you need, [NixOS package search] may be able to help.

For more information about the NixOS testing library see the
[testing wiki article].

## Kernel version inside VM

By default `linuxPackages_latest` from nixpkgs is used in the testing VM. This
is the latest stable kernel version available in the nixpkgs revision. Updating
the nixpkgs flake input may update the used kernel. A custom-built kernel can be
used as well but with added build times in CI.

## Adding new tests

The easiest way to add new tests is of course to copy an existing test and adapt
it accordingly. Importantly, for nix to see a file as part of the sources, the
file needs to be in the git index. It doesn't have to be committed to the repo
just yet but you need to `git add` it. If `git ls-files` lists the file, nix
will also see it.

## Interactive debugging of tests

When writing a new test or experiencing a difficult to understand test failure,
an interactive login can be very handy. This can be achieved by building the
`driverInteractive` attribute of the check, for example like this:

```console
$ nix build .#checks.x86_64-linux.subvolume.driverInteractive
```

The `nix build` will create a symlink in your working directory called `result`
which leads to a script that launches the VM interactively:

```console
$ ./result/bin/nixos-test-driver
```

There is more information about this in the NixOS manual under
[running tests interactively].

[Linux wiki article]: https://wiki.nixos.org/wiki/Linux_kernel
[NixOS package search]: https://search.nixos.org
[running tests interactively]: https://nixos.org/manual/nixos/stable/#sec-running-nixos-tests-interactively
[testing wiki article]: https://wiki.nixos.org/wiki/NixOS_Testing_library
