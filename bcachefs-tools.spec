%global kmodname bcachefs

# Ensure that the build script shell is bash
%global _buildshell /bin/bash

%global dkmsname dkms-%{kmodname}

# SUSE Linux does not define the dist tag, so we must define it manually
%if "%{_vendor}" == "suse"
%global dist .suse%{?suse_version}
%endif

# Disable LTO for now until more testing can be done.
%global _lto_cflags %{nil}

%global make_opts VERSION="%{version}" BCACHEFS_FUSE=1 BUILD_VERBOSE=1 PREFIX=%{_prefix} ROOT_SBINDIR=%{_sbindir}

Name:           bcachefs-tools
# define with i.e. --define '_version 1.0'
Version:        0%{?_version}
Release:        0%{?dist}
Summary:        Userspace tools for bcachefs

%global MSRV 1.77

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
URL:            https://bcachefs.org/
%if 0%{?_version} == 0
Source:         bcachefs-tools_%{version}.tar.xz
Source1:        bcachefs-tools_%{version}.tar.xz.sig
Source2:        apt.bcachefs.org.keyring
Source3:        cargo.config
Source99:       %{dkmsname}.rpmlintrc
%else
Source:         https://evilpiepirate.org/%{name}/%{name}-vendored-%{version}.tar.zst
%endif

BuildRequires:  findutils
BuildRequires:  gcc
BuildRequires:  jq
BuildRequires:  make
BuildRequires:  tar
%if 0%{?_version} == 0
BuildRequires:  xz
%else
BuildRequires:  zstd
%endif

BuildRequires:  cargo >= %{MSRV}

%if 0%{?suse_version}
BuildRequires:  rust >= %{MSRV}
%else
BuildRequires:  rustc >= %{MSRV}
%endif

BuildRequires:  kernel-headers >= 6.11.3
BuildRequires:  libaio-devel >= 0.3.111
BuildRequires:  libattr-devel
BuildRequires:  pkgconfig(blkid)
BuildRequires:  pkgconfig(fuse3) >= 3.7
BuildRequires:  pkgconfig(libkeyutils)
BuildRequires:  pkgconfig(liblz4)
BuildRequires:  pkgconfig(libsodium)
BuildRequires:  pkgconfig(libudev)
BuildRequires:  pkgconfig(liburcu) >= 0.15
BuildRequires:  pkgconfig(libzstd)
BuildRequires:  pkgconfig(udev)
BuildRequires:  pkgconfig(uuid)
BuildRequires:  pkgconfig(zlib)

BuildRequires:  clang-devel
BuildRequires:  llvm-devel
BuildRequires:  pkgconfig

BuildRequires:  systemd-rpm-macros

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
Requires:       kernel-devel >= 6.16
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
   dkms remove -m %{kmodname} -v %{version} --all --rpm_safe_upgrade
   exit $?
fi

%post -n %{dkmsname}
if [ "$1" -ge "1" ]; then
%if "%{_vendor}" == "suse"
   if [ -f %{_libexecdir}/dkms/common.postinst ]; then
      %{_libexecdir}/dkms/common.postinst %{kmodname} %{version}
      exit $?
   fi
%else
   if [ -f /usr/lib/dkms/common.postinst ]; then
      /usr/lib/dkms/common.postinst %{kmodname} %{version}
      exit $?
   fi
%endif
fi

%files -n %{dkmsname}
%license COPYING
%{_usrsrc}/%{kmodname}-%{version}/

# ----------------------------------------------------------------------------


%prep
%autosetup


%build
%if 0%{?_version} == 0
export CARGO_HOME=$PWD/.cargo
export CARGO_ARGS="--frozen"
rm -rf $PWD/.cargo
mkdir -p $PWD/.cargo
cp %{_sourcedir}/cargo.config $PWD/.cargo/config.toml
%endif
%set_build_flags
%make_build %{make_opts}


%install
%if 0%{?_version} == 0
export CARGO_HOME=$PWD/.cargo
export CARGO_ARGS="--frozen"
rm -rf $PWD/.cargo
mkdir -p $PWD/.cargo
cp %{_sourcedir}/cargo.config $PWD/.cargo/config.toml
%endif
%set_build_flags
%make_install %{make_opts}

# Purge unneeded debian stuff
rm -rfv %{buildroot}/%{_datadir}/initramfs-tools


%changelog
* Sun Oct 19 2025 Roman Lebedev <lebedev.ri@gmail.com>
- Fix DKMS support on SUSE
* Sun Oct 12 2025 Roman Lebedev <lebedev.ri@gmail.com>
- OBS support
* Sat Sep 27 2025 Neal Gompa <neal@gompa.dev>
- Initial package based on Fedora package
