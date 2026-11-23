%global crate beyrix
%global upver 0.1.0-alpha.1
%global tag v%{upver}

# The vendored dependencies are extracted next to the upstream source tree by
# %%autosetup -a 1. They are generated with `cargo vendor --locked` from the
# tagged Cargo.lock, so the build never reaches the network and resolves the
# exact same crate versions as upstream.
%global vendordir vendor

Name:           beyrix
Version:        0.1.0~alpha.1
Release:        1%{?dist}
Summary:        Experimental Wayland terminal emulator with OpenGL rendering

License:        MPL-2.0 AND (MIT OR Apache-2.0) AND (Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT) AND (MIT OR Apache-2.0 OR LGPL-2.1-or-later) AND (MIT OR Apache-2.0 OR Zlib) AND (Unlicense OR MIT) AND ISC AND Unicode-3.0 AND Zlib
URL:            https://github.com/Beykus-Y/Beyrix
Source0:        %{url}/archive/refs/tags/%{tag}.tar.gz#/Beyrix-%{upver}.tar.gz
Source1:        beyrix-vendor-%{upver}.tar.gz
Source2:        io.github.beykus_y.Beyrix.desktop
Source3:        io.github.beykus_y.Beyrix.metainfo.xml
Source4:        beyrix.1

ExclusiveArch:  x86_64

BuildRequires:  cargo-rpm-macros >= 24
BuildRequires:  rust
BuildRequires:  cargo
BuildRequires:  gcc
BuildRequires:  pkgconf-pkg-config
BuildRequires:  libxkbcommon-devel
# Tests load system fonts through fontconfig and exercise the Git prompt.
BuildRequires:  fontconfig
BuildRequires:  dejavu-sans-mono-fonts
BuildRequires:  git-core

Requires:       bash
Requires:       fontconfig
# Loaded at runtime with dlopen(), so these are not picked up by RPM's
# automatic dependency generator.
Requires:       libwayland-client.so.0()(64bit)
Requires:       (dejavu-sans-mono-fonts or liberation-mono-fonts or google-noto-sans-mono-fonts)
Recommends:     libwayland-egl.so.1()(64bit)
Recommends:     libEGL.so.1()(64bit)
Recommends:     libGLESv2.so.2()(64bit)
Recommends:     libvulkan.so.1()(64bit)

%description
Beyrix is an experimental terminal emulator for Linux and Wayland. It renders
with OpenGL ES through EGL and falls back to software rendering when hardware
acceleration is unavailable. A PTY-backed Bash session provides the interactive
shell, and BEdit opens projects and edits multiple files in tabs.

The project is alpha quality and supports x86_64 Wayland sessions only.

%prep
%autosetup -n Beyrix-%{upver} -a 1
%cargo_prep -v %{_builddir}/%{buildsubdir}/%{vendordir}

%build
%cargo_build

%install
%cargo_install
install -Dpm0644 %{SOURCE2} %{buildroot}%{_datadir}/applications/io.github.beykus_y.Beyrix.desktop
install -Dpm0644 %{SOURCE3} %{buildroot}%{_metainfodir}/io.github.beykus_y.Beyrix.metainfo.xml
install -Dpm0644 %{SOURCE4} %{buildroot}%{_mandir}/man1/beyrix.1
%{cargo_license} > cargo-license.txt
%cargo_vendor_manifest
install -Dpm0644 cargo-license.txt %{buildroot}%{_licensedir}/%{name}/cargo-license.txt
install -Dpm0644 cargo-vendor.txt %{buildroot}%{_licensedir}/%{name}/cargo-vendor.txt

%check
%cargo_test

%files
%license LICENSE
%doc README.md CONFIGURATION.en.md CONFIGURATION.md
%{_bindir}/beyrix
%{_bindir}/bedit
%{_datadir}/applications/io.github.beykus_y.Beyrix.desktop
%{_metainfodir}/io.github.beykus_y.Beyrix.metainfo.xml
%{_mandir}/man1/beyrix.1*
%{_licensedir}/%{name}/cargo-license.txt
%{_licensedir}/%{name}/cargo-vendor.txt

%changelog
* Tue Nov 24 2026 Beykus Y <beykus-y@users.noreply.github.com> - 0.1.0~alpha.1-1
- Initial Fedora package for the v0.1.0-alpha.1 release