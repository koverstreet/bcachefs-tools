#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# fetch-module.sh — download a prebuilt, signed bcachefs.ko for the kernel being
# built for, instead of compiling it locally. This is the client side of the
# bcachefs prebuilt-module pipeline: the build farm at module.bcachefs.org
# compiles a signed bcachefs.ko for every distro-kernel x bcachefs-version it
# sees, and the DKMS module build (via the fs/Makefile hook) runs this script
# before falling back to a local compile.
#
# CONTRACT (what the fs/Makefile hook relies on):
#   exit 0  — a matching module was fetched, verified, and written to <dest>.
#             DKMS treats it exactly as if `make` had produced it — no build.
#   exit !0 — nothing usable (offline, no matching build, wrong vermagic, ...).
#             DKMS falls through to a normal local compile. A miss is always
#             safe: it just means we build the module the way we always did.
#
# The lookup key is everything that uniquely identifies the kernel *binary* a
# module must match AND that a running system can reconstruct from its own
# package database:
#
#   <base>/<distro>/<arch>/<pkgver>/bcachefs-<ref>.ko
#
#   distro  os-release ID (debian, ubuntu, fedora, arch, ...)
#   arch    the distro's own arch name (amd64 vs x86_64) — asked of the package
#           manager, not uname, so it matches what the farm publishes
#   pkgver  the farm's storage key for this kernel build. It mirrors the
#           distro-kernel-fetcher: the package Version on Debian/Ubuntu (whose
#           uname-r — e.g. 7.0.13-amd64 — is NOT the package version 7.0.13-1),
#           and uname-r itself on Fedora/Arch (where it already carries the full
#           release and so already is the exact version). We reconstruct it the
#           same way per distro, so both sides agree by construction.
#   ref     the bcachefs version (git describe == bcachefs-tools VERSION ==
#           dkms.conf PACKAGE_VERSION) — which build of bcachefs we want.
#
# TRUST MODEL:
#   * Authenticity: we verify the module's appended PKCS#7 signature chains to
#     the bcachefs signing CA (bundled next to this script) before accepting it.
#     HTTPS only proves we reached the server; the signature proves the bytes are
#     a genuine bcachefs build, so a compromised mirror or TLS path can't slip us
#     a module that DKMS would then sign with the machine's MOK and load. The CA
#     is trusted because it ships in this (distro-signed) package. If we can't
#     verify for any reason (no openssl, no CA, bad signature) we build locally.
#   * ABI: vermagic must match this kernel exactly or modprobe rejects it; we
#     check it and build locally on mismatch.
#   * Secure Boot at load is DKMS's job, not ours: DKMS installs this module
#     unstripped (dkms.conf STRIP=no), so the farm signature is retained. Where
#     DKMS is configured for its own MOK signing (e.g. Ubuntu's shim-signed
#     default) it appends a signature the kernel verifies instead; where it is
#     not (default Debian) the retained farm signature loads it via the enrolled
#     bcachefs CA (see the debian/ MOK-enrollment flow). Either way a miss falls
#     back to a local build. (Analysis: .claude/2026-06-29-module-pipeline-trust-model.md.)

set -e

kernelver=${1:-$kernelver}
ref=${2:-$PACKAGE_VERSION}
dest=$3

base_url=${BCACHEFS_MODULE_URL:-https://module.bcachefs.org}

# Build the module locally instead, reporting exactly why we couldn't fetch one.
# The reason lands in the DKMS build log; the nonzero exit tells the fs/Makefile
# hook to compile locally instead.
fall_back()
{
	echo "bcachefs: no prebuilt module — building locally ($1)" >&2
	exit 1
}

[ -n "$kernelver" ] || fall_back "no kernel version"
[ -n "$ref" ]       || fall_back "no bcachefs version"
[ -n "$dest" ]      || fall_back "no destination path"
command -v modinfo >/dev/null 2>&1 || fall_back "no modinfo to verify the module"
command -v openssl >/dev/null 2>&1 || fall_back "no openssl to verify the module signature"

# The signing CA (public root) is bundled next to this script; an env override
# helps testing. Resolve it up front so we fail fast before downloading.
signing_ca=${BCACHEFS_SIGNING_CA:-}
if [ -z "$signing_ca" ]; then
	for c in "$(dirname "$0")/bcachefs-signing-ca.pem" \
		 /usr/share/bcachefs-tools/bcachefs-signing-ca.pem; do
		[ -f "$c" ] && { signing_ca=$c; break; }
	done
fi
[ -f "$signing_ca" ] || fall_back "no bcachefs signing CA to verify the module against"

# Fetch $1 to $2. Only a clean 200 counts as a hit; any error (404, offline,
# timeout) returns nonzero so we build. Time-bounded on purpose: this can run
# inside a kernel package's postinst, so a slow or stalled server must never
# wedge the install — it caps the total transfer and falls back to a local build.
download()
{
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --retry 1 --connect-timeout 10 --max-time 120 -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -q --tries=2 --timeout=15 -O "$2" "$1"
	else
		return 127
	fi
}

# Verify the farm's appended PKCS#7 signature chains to the bcachefs signing CA.
# HTTPS only proves we reached the server; this proves the bytes are a genuine
# bcachefs build, so a compromised mirror or TLS path can't slip us a module that
# DKMS would then sign with the machine's MOK and load. Any failure => fall back.
#
# The module ends with (see include/uapi/linux/module_signature.h):
#   [ signed module ][ PKCS#7 sig ][ struct module_signature, 12B ][ marker, 28B ]
# Split off the detached signature using the trailer's big-endian sig_len, then
# verify the chain to the CA (the leaf cert is embedded in the signature).
verify_signature()
{
	ko=$1
	sz=$(wc -c < "$ko" | tr -d ' ') || fall_back "cannot size the downloaded module"
	[ "$sz" -gt 40 ] || fall_back "downloaded module too small to be signed"

	printf '~Module signature appended~\n' > "$work/marker"
	tail -c 28 "$ko" | cmp -s "$work/marker" - ||
		fall_back "downloaded module carries no appended signature"

	# id_type (offset 2 of the 12-byte info block at sz-40) must be 2 (PKCS#7).
	idt=$(dd if="$ko" bs=1 skip=$((sz - 38)) count=1 2>/dev/null | od -An -tu1 | tr -d ' ')
	[ "$idt" = 2 ] || fall_back "module signature is not PKCS#7 (id_type=$idt)"

	# sig_len: big-endian u32, the last 4 bytes of the info block (sz-32).
	sl=$(dd if="$ko" bs=1 skip=$((sz - 32)) count=4 2>/dev/null | od -An -tu1 |
		awk '{print $1*16777216 + $2*65536 + $3*256 + $4}')
	{ [ -n "$sl" ] && [ "$sl" -gt 0 ] && [ "$sl" -lt $((sz - 40)) ]; } ||
		fall_back "module signature length implausible ($sl)"

	clen=$((sz - 40 - sl))
	head -c "$clen" "$ko" > "$work/content"
	dd if="$ko" bs=1 skip="$clen" count="$sl" of="$work/sig.p7" 2>/dev/null

	if ! openssl cms -verify -binary -inform DER -in "$work/sig.p7" \
		-content "$work/content" -CAfile "$signing_ca" -purpose any \
		-out /dev/null 2>"$work/openssl.err"; then
		sed 's/^/bcachefs:   openssl: /' "$work/openssl.err" >&2
		fall_back "module signature did not verify against the bcachefs CA — refusing it (tampering or mirror compromise?)"
	fi
}

# Resolve <distro>/<arch>/<pkgver> for the target kernel from the package db,
# mirroring the distro-kernel-fetcher's storage key per distro (see <pkgver>
# above). The package-manager query also gates on the kernel being distro-owned:
# a custom/self-built kernel won't resolve and falls back to a local build.
moddir=/lib/modules/$kernelver

. /etc/os-release 2>/dev/null || fall_back "no /etc/os-release to identify the distro"
distro=$ID
[ -n "$distro" ] || fall_back "no ID in /etc/os-release"

case "$distro" in
debian | ubuntu | linuxmint | pop | devuan | raspbian)
	command -v dpkg-query >/dev/null 2>&1 || fall_back "dpkg-query not found"
	arch=$(dpkg --print-architecture 2>/dev/null) ||
		fall_back "dpkg --print-architecture failed"
	# The kernel package is conventionally linux-image-<uname-r>; for a
	# non-standard flavour fall back to whatever owns the modules tree.
	pkgver=$(dpkg-query -W -f='${Version}' "linux-image-$kernelver" 2>/dev/null) || pkgver=
	if [ -z "$pkgver" ]; then
		pkg=$(dpkg-query -S "$moddir/kernel" 2>/dev/null | head -1 | cut -d: -f1)
		[ -n "$pkg" ] || fall_back "no dpkg package owns $moddir/kernel"
		pkgver=$(dpkg-query -W -f='${Version}' "$pkg" 2>/dev/null) ||
			fall_back "dpkg-query has no version for $pkg"
	fi
	;;
fedora | rhel | centos | rocky | almalinux | ol)
	command -v rpm >/dev/null 2>&1 || fall_back "rpm not found"
	arch=$(rpm -E '%{_arch}' 2>/dev/null) ||
		fall_back "rpm could not report its arch"
	# Confirm an rpm package owns this kernel (a custom kernel won't, and
	# falls back to a local build). The version key is uname-r itself: a
	# Fedora/RHEL uname-r already carries the full fcNN/elNN release + arch, so
	# the farm stores under it directly rather than a separate package version.
	rpm -qf "$moddir/kernel" >/dev/null 2>&1 ||
		fall_back "no rpm package owns $moddir/kernel"
	pkgver=$kernelver
	;;
arch | cachyos | endeavouros | manjaro)
	command -v pacman >/dev/null 2>&1 || fall_back "pacman not found"
	# uname -m matches Arch's package arch (x86_64). NOTE: CachyOS microarch
	# kernels (x86_64_v3) are a distinct binary the farm keys separately —
	# uname -m can't tell them apart; that's a known gap to revisit.
	arch=$(uname -m)
	# Confirm a pacman package owns this kernel (custom kernels fall back to a
	# local build). Like Fedora, an Arch uname-r already is the exact version,
	# so the farm keys on it directly — not on pacman's dotted pkgver.
	pacman -Qo "$moddir/pkgbase" >/dev/null 2>&1 ||
		pacman -Qo "$moddir/vmlinuz" >/dev/null 2>&1 ||
		fall_back "no pacman package owns the $kernelver kernel"
	pkgver=$kernelver
	;;
*)
	fall_back "unsupported distro '$distro' for prebuilt modules"
	;;
esac

[ -n "$pkgver" ] || fall_back "could not determine the kernel package version"

url=$base_url/$distro/$arch/$pkgver/bcachefs-$ref.ko

work=$(mktemp -d) || fall_back "could not create a working directory"
trap 'rm -rf "$work"' EXIT
ko=$work/bcachefs.ko

echo "bcachefs: trying prebuilt module $url" >&2
download "$url" "$ko" ||
	fall_back "not available for $distro/$arch/$pkgver bcachefs $ref"

# Authenticity gate: refuse anything not signed by the bcachefs key, so a bad
# mirror or MITM can't get a module installed. (ABI gate — vermagic — follows.)
verify_signature "$ko"

# The module must carry this kernel's exact vermagic or modprobe rejects it.
# Read the target vermagic from any module the kernel already ships.
ref_mod=$(find "$moddir/kernel" \( -name '*.ko' -o -name '*.ko.*' \) 2>/dev/null | head -1)
[ -n "$ref_mod" ] || fall_back "no installed module under $moddir to read vermagic from"
want_vm=$(modinfo -F vermagic "$ref_mod" 2>/dev/null || true)
got_vm=$(modinfo -F vermagic "$ko" 2>/dev/null || true)
[ -n "$want_vm" ] || fall_back "could not read vermagic from $ref_mod"
[ "$got_vm" = "$want_vm" ] ||
	fall_back "vermagic mismatch (got '$got_vm', need '$want_vm')"

mkdir -p "$(dirname "$dest")" 2>/dev/null || true
cp "$ko" "$dest" || fall_back "could not write $dest"
echo "bcachefs: installed prebuilt module -> $dest" >&2
echo "bcachefs:   bcachefs $ref, $distro $arch, kernel $pkgver (DKMS will sign + install)" >&2
exit 0
