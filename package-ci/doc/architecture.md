# package-ci: bcachefs-tools .deb build orchestrator

Self-hosted CI system that builds Debian/Ubuntu packages for
bcachefs-tools across a matrix of distributions and architectures.
Publishes to apt.bcachefs.org via aptly.

Runs as `aptbcachefsorg` on evilpiepirate.org.

## Design

Reconcile loop, not a queue. The orchestrator knows the desired state
(latest commit, packages for every distro×arch) and the current state
(filesystem). Each iteration fills the gap. Same pattern as ktest CI
and the filesystem's own reconcile pass.

No external CI service. Git push → post-receive hook writes desired
commit + SIGUSR1 → orchestrator wakes up → builds → publishes.

## Build matrix

| | amd64 | ppc64el | arm64 |
|---|---|---|---|
| **unstable** | local | cross (local) | remote (farm1) |
| **forky** | local | cross (local) | remote (farm1) |
| **trixie** | local | cross (local) | remote (farm1) |
| **questing** | local | — | remote (farm1) |
| **plucky** | local | — | remote (farm1) |

ppc64el is excluded on Ubuntu distros (cross-compile broken there).

## Three-phase pipeline

```
Phase 1: source package
  git clone → dpkg-buildpackage -S → .dsc + .orig.tar.xz
  (runs in podman container, debian:trixie-slim)

Phase 2: binary builds (matrix)
  dpkg-buildpackage -b per distro×arch
  local: podman container with cached deps image
  remote (arm64): scp to farm1, build there, scp results back
  cross (ppc64el): podman with qemu-user-static + cross-gcc
  max 2 local + 1 remote concurrent

Phase 3: publish
  sign .debs with debsigs → aptly repo add → aptly snapshot → aptly publish
  runs only when ALL builds are terminal (done or failed)
  publishes per-distro: only distros with ≥1 successful arch get published
```

## State directory layout

```
/home/aptbcachefsorg/package-ci/
├── desired              ← target commit (written by post-receive hook)
├── orchestrator.pid     ← PID of running orchestrator
├── config               ← GPG_SIGNING_SUBKEY_FINGERPRINT, APTLY_ROOT
├── scripts/             ← shell scripts (build-source.sh, etc.)
├── cache/               ← rustup, cargo, apt caches (persistent across builds)
│   ├── rustup/
│   ├── cargo/
│   └── apt/
└── builds/
    └── $COMMIT/
        ├── source/
        │   ├── status   ← pending|building|done|failed
        │   ├── pid      ← PID of build process
        │   ├── log      ← build log
        │   └── result/  ← .dsc, .orig.tar.xz, etc.
        ├── $distro-$arch/
        │   ├── status
        │   ├── pid
        │   ├── log
        │   └── result/  ← .deb files
        └── publish/
            ├── status
            ├── pid
            └── log
```

## Stale build recovery

When the orchestrator restarts, `self.running` is empty. On the next
reconcile, `effective_status()` checks any "building" jobs:

1. Is the job tracked in `self.running`? → genuinely running
2. Is the PID in the pid file alive (`kill -0`)? → process exists
3. Neither → stale build from crashed orchestrator, mark as **failed**

This means: if the orchestrator crashes and restarts, stuck builds
get auto-recovered on the next reconcile iteration.

## Signals

- **SIGUSR1**: wake up immediately (sent by post-receive hook on push)
- **SIGTERM/SIGINT**: clean shutdown (kill running builds, remove pid file)

## Cached build environments

Binary builds cache the container image after installing all
dependencies. Cache key: `ci-deps:$DISTRO-$ARCH-rust$VERSION-v$N`.
Bump `CACHE_VERSION` in build-binary.sh to force rebuild. Per-job
cache invalidation: `touch $CACHE_DIR/rebuild-$DISTRO-$ARCH`.

## Deployment

```bash
# Build
cd package-ci && cargo build --release

# Deploy (as root or with appropriate permissions)
cp target/release/bcachefs-package-ci /home/aptbcachefsorg/package-ci/
cp scripts/*.sh /home/aptbcachefsorg/package-ci/scripts/

# Restart
systemctl restart bcachefs-package-ci
```

First-time setup: `scripts/setup-epp.sh` (run as root).

## Debugging

```bash
# Service status and recent logs
systemctl status bcachefs-package-ci
journalctl -u bcachefs-package-ci -n 50

# Build status for current desired commit
/home/aptbcachefsorg/package-ci/scripts/status.sh

# Per-job build log
cat /home/aptbcachefsorg/package-ci/builds/$COMMIT/$DISTRO-$ARCH/log

# Force retry of a failed build: delete its status file
rm /home/aptbcachefsorg/package-ci/builds/$COMMIT/$DISTRO-$ARCH/status

# Force retry of publish
rm /home/aptbcachefsorg/package-ci/builds/$COMMIT/publish/status

# Manual wake-up
kill -USR1 $(cat /home/aptbcachefsorg/package-ci/orchestrator.pid)

# Web status page (auto-refreshes every 30s)
# https://apt.bcachefs.org/ci.html
```

## Retriggering builds

To rerun a specific commit (e.g. to test whether a regression is from
a particular change):

```bash
# Option 1: force retry of a previous commit
# Write the commit hash into the desired file, then wake the orchestrator
echo "$COMMIT" > /home/aptbcachefsorg/package-ci/desired
kill -USR1 $(cat /home/aptbcachefsorg/package-ci/orchestrator.pid)

# Option 2: clear failed status for specific jobs on the current commit
# The orchestrator will re-attempt them on the next reconcile
rm /home/aptbcachefsorg/package-ci/builds/$COMMIT/$DISTRO-$ARCH/status
rm /home/aptbcachefsorg/package-ci/builds/$COMMIT/source/status  # if source failed
kill -USR1 $(cat /home/aptbcachefsorg/package-ci/orchestrator.pid)

# Option 3: nuke an entire commit's build state to start fresh
rm -rf /home/aptbcachefsorg/package-ci/builds/$COMMIT
kill -USR1 $(cat /home/aptbcachefsorg/package-ci/orchestrator.pid)
```

Note: the orchestrator only builds the commit in the `desired` file.
To test an older commit, you must update `desired`. Remember to set it
back afterwards.

## Deploying script changes

**Scripts run from the deployed location**, not from the repo being
built. Changes to `package-ci/scripts/*.sh` must be explicitly deployed:

```bash
scp scripts/*.sh aptbcachefsorg@apt.bcachefs.org:~/package-ci/scripts/
```

The orchestrator binary itself also needs explicit deployment (see
Deployment section above). The orchestrator does NOT need a restart
after deploying script changes — scripts are exec'd fresh each build.

## .cargo/config.toml and vendored sources

The repo's `.cargo/config.toml` (gitignored) contains vendored source
config that only works when `vendor/` exists (tagged release tarballs).
**Do not add `.cargo/config.toml` to git** — it will break non-vendor
builds because cargo will try to use a vendor directory that doesn't
exist.

Cross-linker config for ppc64el/arm64 is handled by `build-binary.sh`,
which creates `.cargo/config.toml` with the appropriate
`[target.*.linker]` settings if it doesn't already exist. This runs
after source extraction and before `dpkg-buildpackage`.

## Common failure modes

**Publish not running**: Publish requires `!still_running` — all
binary builds must be in a terminal state (done or failed). Check
`status.sh` for jobs stuck in "building". If the orchestrator is
running, stale builds should auto-recover. If not, restart the service.

**arm64 builds stuck**: Remote builds run over ssh to farm1. If the
ssh connection drops, the local ssh process dies, stale detection
catches it. If farm1 is unreachable, builds fail immediately.

**ppc64el cross-compile failures**: Known issue — the cross-compile
toolchain is fragile. These failures don't block publish (they end
up as "failed", and publish proceeds with partial results).
