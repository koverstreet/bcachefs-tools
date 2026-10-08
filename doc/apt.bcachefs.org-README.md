To add this repository to your computer, do:
```bash
sudo install -d -m 0755 /etc/apt/keyrings
wget -qO- https://apt.bcachefs.org/apt.bcachefs.org.asc | sudo tee /etc/apt/keyrings/apt.bcachefs.org.asc > /dev/null
sudo chmod 0644 /etc/apt/keyrings/apt.bcachefs.org.asc
# Fingerprint: EA483B991020C72A8A5035ADA0620B5E0E01C1DD
sudo tee /etc/apt/sources.list.d/apt.bcachefs.org.sources > /dev/null <<EOF
Types: deb deb-src
URIs: https://apt.bcachefs.org/$(. /etc/os-release && echo ${UBUNTU_CODENAME:-$VERSION_CODENAME})/
Suites: bcachefs-tools-release
Components: main
Architectures: $(dpkg --print-architecture)
Signed-By: /etc/apt/keyrings/apt.bcachefs.org.asc
EOF
sudo apt update
sudo apt install bcachefs-tools
```

> **_NOTE:_**
Packages are built per distribution: the URI must name your release's
codename, which the snippet above fills in from `/etc/os-release` - for a
derivative such as Mint, the Ubuntu release it's based on. The releases
there are repositories for are the directories at https://apt.bcachefs.org/.
`unstable` is built against Debian sid, and its dependencies often aren't
installable on a stable release - if you set this repository up with
`unstable` in the URI and you're not on sid, replace it with your codename
in `/etc/apt/sources.list.d/apt.bcachefs.org.sources`.

> **_NOTE:_**
This will give you packages for the latest release of `bcachefs-tools`.
If you need packages for the latest `git master` commit,
replace `bcachefs-tools-release` with `bcachefs-tools-snapshot`.

Stable channel:
`Suites: bcachefs-tools-release`

Snapshot/nightly channel:
`Suites: bcachefs-tools-snapshot`

If you want to ensure that the packages from this repository are always preferred, do:
```bash
sudo mkdir -p /etc/apt/preferences.d
sudo tee /etc/apt/preferences.d/apt.bcachefs.org.pref > /dev/null <<EOF
Package: *
Pin: origin apt.bcachefs.org
Pin-Priority: 1000
EOF
```

> **_NOTE:_**
Note that yes, you should always prefer `Pin: origin <hostname>`,
over `Pin: release o=<origin>`, because pinning by origin
will *actually* pin by the full hostname of APT repository,
whereas any repository can claim anything in it's `Origin: ` field,
and thus pinning by origin label is inherently insecure!


For more information, see:
https://wiki.debian.org/DebianRepository/UseThirdParty

Source, Debian tarballs, dsc files and binary `.deb` packages can be verified using https://github.com/sigstore/rekor.

Binary `.deb` packages are signed with debsigs.
