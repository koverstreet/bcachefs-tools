%global kmodname bcachefs

# Ensure that the build script shell is bash
%global _buildshell /bin/bash

# debbuild doesn't define _usrsrc yet
%if 0%{?_debbuild}
%global _usrsrc %{_prefix}/src
%endif

# Set up the correct DKMS module name, following proper conventions
%if 0%{?_debbuild}
%global dkmsname %{kmodname}-dkms
%else
%global dkmsname dkms-%{kmodname}
%endif

# SUSE Linux does not define the dist tag, so we must define it manually
%if "%{_vendor}" == "suse"
%global dist .suse%{?suse_version}
%endif

# Disable LTO for now until more testing can be done.
%global _lto_cflags %{nil}

%global make_opts VERSION="%{version}" BCACHEFS_FUSE=1 BUILD_VERBOSE=1 PREFIX=%{_prefix} ROOT_SBINDIR=%{_sbindir}

Name:           bcachefs-tools
Version:        1.31.5
Release:        0%{?dist}
Summary:        Userspace tools for bcachefs

# --- rust ---
# Apache-2.0
# Apache-2.0 OR MIT
# Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT
# MIT
# MIT OR Apache-2.0
# MPL-2.0
# Unlicense OR MIT
# --- misc ---
# GPL-2.0-only
# GPL-2.0-or-later
# LGPL-2.1-only
# BSD-3-Clause
License:        GPL-2.0-only AND GPL-2.0-or-later AND LGPL-2.1-only AND BSD-3-Clause AND (Apache-2.0 AND (Apache-2.0 OR MIT) AND (Apache-2.0 with LLVM-exception OR Apache-2.0 OR MIT) AND MIT AND MPL-2.0 AND (Unlicense OR MIT))
%if 0%{?_debbuild}
Packager:       Bcachefs Developers <linux-bcachefs@vger.kernel.org>
Group:          kernel
%endif
URL:            https://bcachefs.org/
Source:         https://evilpiepirate.org/%{name}/%{name}-vendored-%{version}.tar.zst

BuildRequires:  findutils
BuildRequires:  gcc
BuildRequires:  make
BuildRequires:  tar
BuildRequires:  zstd

BuildRequires:  cargo

%if 0%{?suse_version}
BuildRequires:  rust
%else
BuildRequires:  rustc
%endif

%if 0%{?_debbuild}
BuildRequires:  libaio-dev
BuildRequires:  libattr1-dev
BuildRequires:  libblkid-dev
BuildRequires:  libfuse3-dev >= 3.7
BuildRequires:  libkeyutils-dev
BuildRequires:  liblz4-dev
BuildRequires:  libsodium-dev
BuildRequires:  libudev-dev
BuildRequires:  liburcu-dev
BuildRequires:  libzstd-dev
BuildRequires:  systemd-dev
BuildRequires:  uuid-dev
BuildRequires:  zlib1g-dev

BuildRequires:  libclang-dev
BuildRequires:  llvm-dev
BuildRequires:  pkg-config

BuildRequires:  systemd-deb-macros
%else
BuildRequires:  libaio-devel
BuildRequires:  libattr-devel
BuildRequires:  pkgconfig(blkid)
BuildRequires:  pkgconfig(fuse3) >= 3.7
BuildRequires:  pkgconfig(libkeyutils)
BuildRequires:  pkgconfig(liblz4)
BuildRequires:  pkgconfig(libsodium)
BuildRequires:  pkgconfig(libudev)
BuildRequires:  pkgconfig(liburcu)
BuildRequires:  pkgconfig(libzstd)
BuildRequires:  pkgconfig(udev)
BuildRequires:  pkgconfig(uuid)
BuildRequires:  pkgconfig(zlib)

BuildRequires:  clang-devel
BuildRequires:  llvm-devel
BuildRequires:  pkgconfig

BuildRequires:  systemd-rpm-macros
%endif

# Rust parts FTBFS on 32-bit arches
ExcludeArch:    %{ix86} %{arm32}

%description
The bcachefs-tools package provides all the userspace programs needed to create,
check, modify and correct any inconsistencies in the bcachefs filesystem.

%files
%license COPYING
%doc doc/bcachefs-principles-of-operation.tex
%doc doc/bcachefs.5.rst.tmpl
%{_sbindir}/bcachefs
%{_sbindir}/mount.bcachefs
%{_sbindir}/fsck.bcachefs
%{_sbindir}/mkfs.bcachefs
%{_mandir}/man8/bcachefs.8*
%{_udevrulesdir}/64-bcachefs.rules

# ----------------------------------------------------------------------------

%package -n fuse-bcachefs
Summary:        FUSE implementation of bcachefs
Requires:       %{name}%{?_isa} = %{version}-%{release}

%description -n fuse-bcachefs
This package is an experimental implementation of bcachefs leveraging FUSE to
mount, create, check, modify and correct any inconsistencies in the bcachefs filesystem.

%files -n fuse-bcachefs
%license COPYING
%{_sbindir}/mount.fuse.bcachefs
%{_sbindir}/fsck.fuse.bcachefs
%{_sbindir}/mkfs.fuse.bcachefs

# ----------------------------------------------------------------------------

%package -n %{dkmsname}
Summary:        Bcachefs kernel module managed by DKMS
Requires:       diffutils
Requires:       dkms >= 3.2.1
Requires:       gcc
Requires:       make
Requires:       perl
Requires:       python3

Requires:       %{name} = %{version}-%{release}

# For Fedora/RHEL systems
%if 0%{?fedora} || 0%{?rhel}
Supplements:    (bcachefs-tools and kernel-core)
%endif
# For SUSE systems
%if 0%{?suse_version}
Supplements:    (bcachefs-tools and kernel-default)
%endif

BuildArch:      noarch

%description -n %{dkmsname}
This package is an implementation of bcachefs built using DKMS to offer the kernel
module to mount, create, check, modify and correct any inconsistencies in the bcachefs
filesystem.

%preun -n %{dkmsname}
if [  "$(dkms status -m %{kmodname} -v %{version})" ]; then
   dkms remove -m %{kmodname} -v %{version} --all %{!?_debbuild:--rpm_safe_upgrade}
fi

%post -n %{dkmsname}
%if 0%{?_debbuild}
if [ "$1" = "configure" ]; then
%else
if [ "$1" -ge "1" ]; then
%endif
   if [ -f /usr/lib/dkms/common.postinst ]; then
      /usr/lib/dkms/common.postinst %{kmodname} %{version}
      exit $?
   fi
fi

%files -n %{dkmsname}
%license COPYING
%{_usrsrc}/%{kmodname}-%{version}/

# ----------------------------------------------------------------------------

%if 0%{?_debbuild}
%package -n bcachefs-initramfs
Summary:        bcachefs support for initramfs-tools
Requires:       %{name} = %{version}-%{release}
Requires:       initramfs-tools
BuildArch:      noarch

%description -n bcachefs-initramfs
This package includes hooks for bcachefs support for initramfs-tools.

%files -n bcachefs-initramfs
%license COPYING
%{_datadir}/initramfs-tools/scripts/local-premount/bcachefs
%{_datadir}/initramfs-tools/hooks/bcachefs
%endif

# ----------------------------------------------------------------------------


%prep
%autosetup


%build
%set_build_flags
%make_build %{make_opts}


%install
%set_build_flags
%make_install %{make_opts}


%if 0%{?_debbuild} == 0
# Purge unneeded debian stuff
rm -rfv %{buildroot}/%{_datadir}/initramfs-tools
%endif


%changelog
* Sat Sep 27 2025 Neal Gompa <neal@gompa.dev>
- Initial package
