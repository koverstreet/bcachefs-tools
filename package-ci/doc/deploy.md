# Deploying package-ci

## Why this is a document and not just a script

On 2026-08-09 v1.39.1 published into the release suite with a snapshot version
(`1:1.39.0~20260809230746.gbp9dc9769fe3d4`), which sorts *below* `1:1.39.0`, so
nobody already on 1.39.0 was offered it. Two separate failures had to line up:

1. a time-of-check race in the code (`build-source.sh:38` decides the version at
   build start; `main.rs:733` decides the suite at publish start; the tag landed
   between them), and
2. **the deployment was four components at four vintages** —

   | component | live that evening |
   |---|---|
   | orchestrator binary on disk | installed 18:26 that day |
   | orchestrator **process** | June 18 — never restarted |
   | `build-source.sh` | current |
   | `generate-status-html.sh` | March 6 |
   | `post-receive` | July 28 |

   plus uncommitted local edits to `post-receive` and `publish.sh` in the deploy
   checkout, and a `post-receive` in the git hooks dir that was a *separate
   root-owned copy* rather than a symlink.

The status-page patch written after the *previous* release incident,
specifically so this class of failure would be visible, had never been deployed.
So the failure was invisible for the same reason it happened.

Nix fixes (2), not (1). But (2) is what made (1) survive.

## Shape

`flake.nix` here builds one closure containing the orchestrator, the scripts,
the systemd unit and an activation script. `deploy-rs` installs it and runs the
activation. The target (angband) is **Debian**, not NixOS — it has nix with a
running daemon, which is all that's needed; `deploy-rs` pushes closures over
ssh to anything with a store.

There is no `systemd.services.*` here, and nix deliberately has **no on-target
hooks** — `postInstall`/`installPhase` are build-time and sandboxed, and
`nix profile` has no activation concept. Anything touching `/etc` or systemd
belongs to the deployment layer, which is why activation is
`deploy-rs.lib.<system>.activate.custom` and not something clever in the
derivation.

### The profile is root-owned

Deploying is root's job. The *service* still runs unprivileged via `User=` in
the unit. Putting the profile under the service account instead just means the
activation script can't write `/etc/systemd/system`, which was a difficulty I
invented for myself.

### The unit points at the profile, not at a store path

```ini
ExecStart=/nix/var/nix/profiles/per-user/root/package-ci/bin/bcachefs-package-ci
```

`deploy-rs` flips that symlink, so the unit only changes when we change it —
a deploy can't half-apply by updating the binary and leaving the unit stale.

**That path is a deploy-rs convention, not a free choice.** Only the profile
literally named `system` lives at `/nix/var/nix/profiles/<name>`; every other
profile goes under `per-user/<user>/`. Getting it wrong doesn't fail the
deploy — activation succeeds, and systemd then fails `203/EXEC` every 30s with
the service down. Hence:

### Activation checks ExecStart before restarting

Activation is the one moment we hold *both* the old running process and the new
path. Spending that on a restart into a binary we never checked exists is how a
successful deploy leaves a stopped service. So the script stats the ExecStart
target first and refuses to restart if it isn't executable — a wrong path costs
a failed deploy instead of an outage.

### It keeps the original, not the previous generation

Activation `cmp`s the unit and replaces on difference, saving what was there to
`.pre-nix` — silently overwriting something hand-edited is how you lose a fix
nobody wrote down.

The subtlety: it has to tell *its own previous output* from a human's, and for
the unit "is it a regular file" is true either way. The first version couldn't,
so its second run overwrote the hand-written original with the previous nix
version — the guard destroyed exactly what it existed to protect. Provenance is
the test instead: a symlink into the store is ours, and so is anything carrying
the `# Managed by nix` marker line we stamp into what we generate.

### Activation includes the restart

This is the whole point. A deploy that installs a binary and leaves the old
process running is exactly what happened on 2026-08-09.

## Deploying

```sh
cd package-ci
nix run github:serokell/deploy-rs -- .#angband.package-ci
```

`nix flake check` validates the node against deploy-rs's schema. To rehearse
without touching anything, `--dry-activate` copies the closure and skips the
switch; to rehearse the *activation logic*, run the activate script directly
with `PACKAGE_CI_PREFIX` pointed at a scratch root and `PACKAGE_CI_DRY_RUN=1`.

## Wiring it into farm-nixos

The node is defined in this flake (`deploy.nodes.angband`) rather than in
`~/farm-nixos`, because angband is a Debian host running exactly one thing and
that thing is this flake — keeping the definition next to what it deploys is
what makes "deployed" and "committed" the same fact.

`~/farm-nixos` can still drive everything from one place by merging these nodes
into its own. Note its `deploy.nodes` is built with `lib.mapAttrs ... farms`, so
this needs `//` rather than being dropped inside that expression:

```nix
# flake.nix inputs:
bcachefs-package-ci.url = "git+https://evilpiepirate.org/git/bcachefs-tools.git?dir=package-ci";

deploy.nodes = (lib.mapAttrs ... farms) // bcachefs-package-ci.deploy.nodes;
```

That requires the flake to be pushed; as of 2026-08-09 the package-ci commits
are on `testing` only.

## Runtime dependencies are deliberately NOT pinned

The scripts shell out to `git`, `dpkg-buildpackage`, `gbp`, `dch`, `aptly` and
`podman`, resolved from Debian's PATH. Wrapping them from nixpkgs would be purer
but changes the behaviour of a working pipeline for no benefit we need — the
per-distro builds already run inside podman, so the reproducibility that matters
is containerised. Revisit if the host's toolchain starts drifting.

## What the first deploy found

Deployed for real on 2026-08-09. Everything above works against Debian: nix
2.34 with a running daemon, closure copies over `ssh://` in ~30s, `nix profile`
and `activate.custom` behave normally. It also found three bugs in itself, one
of which took the build server down for 91 seconds:

1. **Wrong profile path** — `ExecStart` pointed at
   `/nix/var/nix/profiles/package-ci`, which never exists. Fixed, plus the
   pre-restart check above, which turns this class of mistake into a failed
   deploy.
2. **The backup guard clobbered its own backup** on the second run. Fixed with
   the provenance test above.
3. **A dropped `Environment=` line.** The unit here was retyped from
   `package-ci/bcachefs-package-ci.service` rather than derived from it, and
   lost `XDG_RUNTIME_DIR=/run/user/1034` — which rootless podman resolves its
   runtime state through, on a host where every binary build is a podman
   container. That duplicate unit file has been deleted; this one is now the
   only one.

The pattern: (1) and (3) were both invisible to review and immediately obvious
to execution. The dry-run flags (`PACKAGE_CI_DRY_RUN`, `PACKAGE_CI_PREFIX`)
caught an earlier one before it ever ran, and are worth keeping for that
reason.

## Still outside the closure

Things activation depends on but does not carry — each one a place drift can
start again:

- `/usr/local/bin/publish-poo`, invoked by `post-receive` to rebuild the
  Principles of Operation on every master push. Root-owned, hand-installed,
  not in any repo.
- `$STATE_DIR/config` — GPG signing fingerprint and aptly root. Correctly
  outside (it's host state, not code), but nothing validates it.
- `/home/aptbcachefsorg/uploads/aptly`, the build state under
  `$STATE_DIR/builds/`, and the GPG keyring.
- The Debian toolchain the scripts shell out to (see below).

## If you outgrow this

`system-manager` (numtide) gives you real NixOS modules — including
`systemd.services.*` — on non-NixOS hosts, and composes with deploy-rs through
the same `activate.custom` seam. Worth it once there is more than one unit to
manage; hand-rolling past that point is reimplementing it.
