{
  # package-ci as a deployable closure.
  #
  # Why this exists: the orchestrator binary, the shell scripts, the git
  # post-receive hook and the systemd unit were four separately-installed
  # things at four different vintages, with no mechanism to notice. On
  # 2026-08-09 that shipped a release into the wrong version namespace: a
  # binary was `cargo install`ed mid-build and never restarted, and a status
  # page patch written specifically to make the failure visible had never been
  # deployed at all. This makes them one closure that moves together or not at
  # all, and makes activation include the restart.
  #
  # The target (angband) is Debian, not NixOS, so there is no
  # `systemd.services.*`. deploy-rs's activate.custom is the seam - nix itself
  # deliberately has no on-target hooks, so anything touching /etc or systemd
  # belongs to the deployment layer. See ./doc/deploy.md.

  description = "bcachefs-tools .deb build orchestrator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    deploy-rs.url = "github:serokell/deploy-rs";
    deploy-rs.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, flake-utils, deploy-rs }:
    (flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # The service account on angband. The *profile* is root-owned -
        # deploying is root's job - but the daemon drops to this user.
        serviceUser = "aptbcachefsorg";

        # Stable path the unit's ExecStart points at. deploy-rs flips this
        # symlink; the unit itself then never needs rewriting, so a deploy
        # can't half-apply by updating the binary and leaving the unit stale.
        #
        # This is deploy-rs's convention, not a free choice: only the profile
        # literally named "system" lives at /nix/var/nix/profiles/<name>;
        # everything else goes under per-user/<user>/. Getting this wrong
        # points ExecStart at a path that never appears, which is a 203/EXEC
        # loop rather than a deploy failure - hence the check in `activate`.
        profile = "/nix/var/nix/profiles/per-user/root/package-ci";

        orchestrator = pkgs.rustPlatform.buildRustPackage {
          pname = "bcachefs-package-ci";
          version = "0.1.0";
          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
            # ci-dashboard is a git dependency, so nix needs its hash pinned.
            # Bump this when Cargo.lock's ci-dashboard rev changes; the build
            # reports the correct value on mismatch.
            outputHashes = {
              "ci-dashboard-0.1.0" = "sha256-StMbRH5YBT82lbuyfROIRNRH3gbxU4Y+PWUe1PAGCBg=";
            };
          };

          # No tests in-tree; the orchestrator is exercised by running it.
          doCheck = false;

          meta.mainProgram = "bcachefs-package-ci";
        };

        # uid of serviceUser on the target. Only needed for XDG_RUNTIME_DIR
        # below; if it ever changes, the deploy is the thing that notices.
        serviceUid = 1034;

        # Deliberately NOT wrapped with a nix PATH. The scripts shell out to
        # git, dpkg-buildpackage, gbp, dch, aptly and podman, which come from
        # Debian on the target; the per-distro builds already run inside
        # podman, so the reproducibility that matters is containerised. Pinning
        # these to nixpkgs would change the behaviour of a working pipeline for
        # no benefit we need today.
        #
        # This supersedes the hand-installed ../bcachefs-package-ci.service.
        # Keeping both would be two sources of truth for one file, and that is
        # not hypothetical: the first version of this unit was retyped from
        # that one rather than derived from it, and silently lost
        # XDG_RUNTIME_DIR in the process.
        unit = pkgs.writeText "bcachefs-package-ci.service" ''
          # Managed by nix - see package-ci/flake.nix. Do not edit in place.
          [Unit]
          Description=bcachefs-tools .deb build orchestrator
          After=network.target

          [Service]
          Type=simple
          User=${serviceUser}
          ExecStart=${profile}/bin/bcachefs-package-ci
          Restart=on-failure
          RestartSec=30

          StandardOutput=journal
          StandardError=journal
          SyslogIdentifier=bcachefs-package-ci

          Environment=RUST_LOG=info
          Environment=HOME=/home/${serviceUser}

          # Rootless podman resolves its runtime state through this. The user
          # has lingering enabled so /run/user/${toString serviceUid} exists
          # independently of any login session.
          Environment=XDG_RUNTIME_DIR=/run/user/${toString serviceUid}

          # No extra sandboxing - rootless podman manages its own namespaces
          # and the service already runs unprivileged.

          [Install]
          WantedBy=multi-user.target
        '';

        # Runs as root on the target, after deploy-rs has installed the
        # profile. Everything here is idempotent.
        #
        # Everything it touches lives *outside* the closure, which makes it the
        # one untestable part of a deployment unless the paths are variables.
        # So they are:
        #
        #   PACKAGE_CI_PREFIX   run against a scratch root instead of /
        #   PACKAGE_CI_DRY_RUN  report every action, take none
        #
        # A deployment script you can only exercise in production is how you
        # end up with four vintages of one service and no way to notice.
        activate = pkgs.writeShellScript "activate-package-ci" ''
          set -euo pipefail

          # coreutils and diffutils are pinned: they're pure utilities, and
          # activation shouldn't depend on the target's PATH being sane.
          #
          # systemctl deliberately is NOT. It is a client for one specific
          # running PID 1, not a utility - pointing a nixpkgs systemctl at
          # Debian's systemd is a version-compatibility bet for no gain, and it
          # would put 182MB of systemd (plus libbpf) into a closure whose actual
          # payload is one binary and five shell scripts.
          PATH=${pkgs.lib.makeBinPath [ pkgs.coreutils pkgs.diffutils ]}:$PATH:/usr/bin:/bin

          prefix="''${PACKAGE_CI_PREFIX:-}"
          dry="''${PACKAGE_CI_DRY_RUN:-0}"

          unit_src=${unit}
          unit_dst="$prefix/etc/systemd/system/bcachefs-package-ci.service"
          scripts_link="$prefix/home/${serviceUser}/package-ci/scripts"
          hook="$prefix/var/www/git/bcachefs-tools.git/hooks/post-receive"

          # Where this closure actually is. placeholder "out" would name the
          # writeShellScript derivation, not the package - which is a single
          # file with no scripts/ in it, so both symlinks below would have
          # pointed at nothing. Resolve through the profile symlink instead:
          # $0 is $PROFILE/bin/activate, so the closure root is two up.
          closure="$(dirname "$(dirname "$(readlink -f "$0")")")"

          run() {
            if [ "$dry" = 1 ]; then
              printf 'would: %s\n' "$*"
            else
              "$@"
            fi
          }

          [ "$dry" = 1 ] && echo "package-ci: DRY RUN, nothing will be changed"

          # Preserve what was on the box before nix took over a path - ONCE.
          # The point is to keep the hand-installed original, which only exists
          # before the first deploy; a later deploy that changes the file must
          # not overwrite that with a previous nix-generated version. The first
          # draft of this did exactly that and destroyed the very thing it was
          # written to protect, on its second run.
          #
          # So we have to be able to tell our own output from a human's, and
          # "is it a regular file" can't: the unit is one either way. A symlink
          # into the store is ours; so is anything carrying the marker line we
          # stamp into what we generate. Everything else was put there by hand.
          keep_original() {
            path=$1
            [ -e "$path" ] || return 0            # nothing there yet
            [ -L "$path" ] && return 0            # a store symlink: ours
            case "$(head -1 "$path" 2>/dev/null)" in
              *"Managed by nix"*) return 0 ;;     # we wrote it: ours
            esac
            [ -e "$path.pre-nix" ] && return 0    # original already kept
            echo "package-ci: $path was installed by hand; keeping it at $path.pre-nix" >&2
            run cp -a "$path" "$path.pre-nix"
          }

          # The unit is a bootstrap artifact: it points at the profile symlink,
          # so it only changes when we change it. Install on difference rather
          # than unconditionally, and say so.
          if ! cmp -s "$unit_src" "$unit_dst" 2>/dev/null; then
            keep_original "$unit_dst"
            echo "package-ci: installing unit from this closure"
            run install -Dm0644 "$unit_src" "$unit_dst"
            run systemctl daemon-reload
            # Idempotent, and it means a fresh host needs no separate step to
            # survive a reboot - the closure carries its own enablement.
            run systemctl enable bcachefs-package-ci
          else
            echo "package-ci: unit unchanged"
          fi

          # Scripts: repoint the symlink the orchestrator reads.
          run mkdir -p "$(dirname "$scripts_link")"
          run ln -sfn "$closure/scripts" "$scripts_link"

          # The git hook lives outside any profile, so it has to be reached out
          # and updated. A symlink into the store keeps it in step with the
          # rest of the closure; git doesn't care that it isn't a regular file.
          #
          # It arrives as a root-owned regular copy, for the same reasons the
          # unit did - so it can have been hand-edited too, and gets the same
          # treatment. Checking by hand once before deploying doesn't count;
          # the next person won't.
          run mkdir -p "$(dirname "$hook")"
          keep_original "$hook"
          run ln -sfn "$closure/scripts/post-receive" "$hook"

          # Never restart into a unit we haven't checked can start. ExecStart
          # names the profile path, which is a deploy-rs convention rather than
          # something this closure controls - and getting it wrong doesn't fail
          # the deploy, it stops the daemon and leaves systemd retrying 203/EXEC
          # every 30s. Deploying is the moment we have both the old process
          # still running and the new path in hand; check before spending that.
          if [ ! -x "${profile}/bin/bcachefs-package-ci" ]; then
            echo "package-ci: ${profile}/bin/bcachefs-package-ci is missing or not executable." >&2
            echo "package-ci: refusing to restart - the running daemon is left alone." >&2
            [ "$dry" = 1 ] || exit 1
          fi

          # The whole point: activation includes the restart. A deploy that
          # installs a binary and leaves the old process running is the failure
          # this closure exists to prevent.
          run systemctl restart bcachefs-package-ci
          if [ "$dry" != 1 ]; then
            systemctl is-active --quiet bcachefs-package-ci
            echo "package-ci: restarted, running $(readlink -f ${profile})"
          fi
        '';
      in {
        packages.default = pkgs.stdenv.mkDerivation {
          pname = "bcachefs-package-ci-deploy";
          version = "0.1.0";
          dontUnpack = true;

          installPhase = ''
            mkdir -p $out/bin $out/scripts $out/lib/systemd/system

            cp ${orchestrator}/bin/bcachefs-package-ci $out/bin/
            cp -r ${./scripts}/* $out/scripts/
            chmod +x $out/scripts/*.sh $out/scripts/post-receive
            cp ${unit} $out/lib/systemd/system/bcachefs-package-ci.service
            cp ${activate} $out/bin/activate
          '';
        };

        packages.orchestrator = orchestrator;

        devShells.default = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.shellcheck ];
        };
      })) // {
      # The deploy node lives here rather than in ~/farm-nixos because angband
      # is a Debian host running exactly one thing, and that thing is this
      # flake - keeping the definition next to what it deploys is what makes
      # "deployed" and "committed" the same fact. farm-nixos can still drive it
      # from one place by merging these nodes into its own:
      #
      #   deploy.nodes = (lib.mapAttrs ... farms) // package-ci.deploy.nodes;
      deploy.nodes.angband = {
        hostname = "evilpiepirate.org";
        profiles.package-ci = {
          sshUser = "root";
          user    = "root";
          path    = deploy-rs.lib.x86_64-linux.activate.custom
                      self.packages.x86_64-linux.default "$PROFILE/bin/activate";

          # No network change here, so there is no route to lose - and the
          # fleet has already seen magicRollback spuriously fail a healthy
          # deploy (farm1, 2026-07-25). autoRollback still covers a failed
          # activation.
          magicRollback = false;
          autoRollback  = true;
        };
      };

      checks = builtins.mapAttrs
        (_: deployLib: deployLib.deployChecks self.deploy) deploy-rs.lib;
    };
}
