# KOneDrive for Fedora: two binary packages from one source.
#   konedrive      the daemon, the command line, the window, and the root helper
#                  with its system unit: they cannot work without each other
#   konedrive-kde  the Dolphin plugins
# Built offline from vendored crates by scripts/build-rpm.sh, which makes both
# source tarballs from the committed tree. What goes where, and why:
# docs/design/packaging.md.

# Built locally, and installed with a glob over the output directory
# (README, "Install from RPM"): debuginfo and debugsource packages would match
# that glob too, and the debugsource one would carry every vendored crate.
%global debug_package %{nil}

Name:           konedrive
Version:        0.1.0
Release:        1%{?dist}
Summary:        OneDrive client for KDE Plasma with files on demand

# Only the project's own license. The binaries also link the vendored crates,
# each under its own license (MIT, Apache-2.0 and others); the limitations log
# (R4) says what a public build would have to add.
License:        GPL-3.0-or-later
URL:            https://github.com/kartas39/konedrive
Source0:        %{name}-%{version}.tar.gz
# The crates of Cargo.lock (cargo vendor), and the .cargo/config.toml that
# points cargo at them.
Source1:        %{name}-%{version}-vendor.tar.gz

ExclusiveArch:  %{rust_arches}

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  make
BuildRequires:  cmake
BuildRequires:  extra-cmake-modules
BuildRequires:  kf6-rpm-macros
BuildRequires:  qt6-qtbase-devel
BuildRequires:  qt6-qtdeclarative-devel
BuildRequires:  kf6-kirigami-devel
BuildRequires:  kf6-kirigami-addons-devel
BuildRequires:  kf6-ki18n-devel
BuildRequires:  kf6-kcoreaddons-devel
BuildRequires:  kf6-kconfig-devel
BuildRequires:  kf6-knotifications-devel
BuildRequires:  kf6-kstatusnotifieritem-devel
BuildRequires:  kf6-kdbusaddons-devel
BuildRequires:  kf6-kio-devel
BuildRequires:  kf6-kwindowsystem-devel
BuildRequires:  kf6-kjobwidgets-devel
BuildRequires:  desktop-file-utils
BuildRequires:  systemd-rpm-macros

# The window's QML modules, which no library dependency pulls in.
Requires:       kf6-kirigami%{?_isa}
Requires:       kf6-kirigami-addons%{?_isa}
Requires:       qt6-qtdeclarative%{?_isa}
Recommends:     %{name}-kde = %{version}-%{release}

%description
KOneDrive shows your OneDrive as a folder of real files at their real sizes,
each one an empty placeholder until something opens it; opening it downloads
it first. It has a daemon that runs as your user, a small root-owned helper
that intercepts opens with fanotify, a window with a tray icon, and the
konedrivectl command line. It is read-only for now: nothing is uploaded.

%package        kde
Summary:        Dolphin integration for KOneDrive: file state emblems and menu actions
Requires:       dolphin

%description    kde
Two Dolphin plugins for the KOneDrive folder: an emblem on each file that
shows whether it is online-only, downloading or downloaded, and "Download" and
"Free up space" in its context menu. Neither plugin opens a file in the folder.

%prep
%autosetup -a 1

%build
# The Rust workspace, offline. The vendored .cargo/config.toml replaces
# crates.io with vendor/. Cargo's home and output stay in the build directory.
# RUSTFLAGS come from Fedora's build flags. No --features: fault-injection is
# the VM suite's alone and must never ship (limitations log W8).
export CARGO_HOME="$PWD/.cargo-home"
export CARGO_TARGET_DIR="$PWD/target"
cargo build --release --offline --locked \
    -p konedrived -p konedrivectl -p konedrive-helper
# The helper's test hooks are all named KONEDRIVE_FAULT_*; a release helper has
# none. scripts/install-helper.sh refuses such a binary the same way.
if grep -aq KONEDRIVE_FAULT_ target/release/konedrive-helper; then
    echo "konedrive-helper was built with the fault-injection feature" >&2
    exit 1
fi

# The window, then the Dolphin plugins, each with Fedora's KF6 settings:
# installed under /usr, tests off.
pushd app
%cmake_kf6 -DBUILD_TESTING=OFF
%cmake_build
popd
pushd dolphin
%cmake_kf6 -DBUILD_TESTING=OFF
%cmake_build
popd

%install
install -Dpm 0755 target/release/konedrived %{buildroot}%{_bindir}/konedrived
install -Dpm 0755 target/release/konedrivectl %{buildroot}%{_bindir}/konedrivectl
install -Dpm 0755 target/release/konedrive-helper %{buildroot}%{_libexecdir}/konedrive-helper

pushd app
%cmake_install
popd
pushd dolphin
%cmake_install
popd

# The unit and activation files in packaging/ are the developer install's, which
# runs the daemon from ~/.local/bin and the helper from /usr/local/libexec. Each
# path is rewritten, and the build stops if a line it expects has changed.
sed 's|^ExecStart=%%h/\.local/bin/konedrived$|ExecStart=%{_bindir}/konedrived|' \
    packaging/systemd/konedrived.service > konedrived.service
grep -qx 'ExecStart=%{_bindir}/konedrived' konedrived.service
sed 's|^ExecStart=/usr/local/libexec/konedrive-helper$|ExecStart=%{_libexecdir}/konedrive-helper|' \
    packaging/systemd/konedrive-helper.service > konedrive-helper.service
grep -qx 'ExecStart=%{_libexecdir}/konedrive-helper' konedrive-helper.service
sed 's|^Exec=/bin/false$|Exec=%{_bindir}/konedrived|' \
    packaging/dbus/org.konedrive.Daemon.service > org.konedrive.Daemon.service
grep -qx 'Exec=%{_bindir}/konedrived' org.konedrive.Daemon.service

install -Dpm 0644 konedrived.service %{buildroot}%{_userunitdir}/konedrived.service
install -Dpm 0644 konedrive-helper.service %{buildroot}%{_unitdir}/konedrive-helper.service
install -Dpm 0644 org.konedrive.Daemon.service \
    %{buildroot}%{_datadir}/dbus-1/services/org.konedrive.Daemon.service
# The helper is enabled on install: without it a OneDrive folder is not kept in
# step, and placeholders read as zeros.
install -dm 0755 %{buildroot}%{_presetdir}
echo 'enable konedrive-helper.service' > %{buildroot}%{_presetdir}/80-konedrive.preset

%check
desktop-file-validate %{buildroot}%{_datadir}/applications/org.konedrive.KOneDrive.desktop

%post
%systemd_post konedrive-helper.service
%systemd_user_post konedrived.service
# A helper from scripts/install-helper.sh has its unit in /etc, which overrides
# this package's unit in /usr/lib (README, "Switching from the developer install").
if [ -e /etc/systemd/system/konedrive-helper.service ]; then
    echo "konedrive: /etc/systemd/system/konedrive-helper.service, from scripts/install-helper.sh," >&2
    echo "konedrive: overrides the packaged helper. Remove it: sudo scripts/install-helper.sh --uninstall --force" >&2
fi
# First install only: the preset has enabled the helper, and it starts now
# rather than at the next boot, so that a folder can be registered at once.
# systemd reloads its units only when the transaction ends, so it is told to
# here, first. Neither call may fail the transaction; a failed start is said.
if [ $1 -eq 1 ] && [ -d /run/systemd/system ]; then
    systemctl daemon-reload || :
    systemctl start konedrive-helper.service ||
        echo "konedrive: the helper did not start; see: systemctl status konedrive-helper" >&2
fi

%preun
# Removal only: the helper and every running daemon are stopped and disabled.
%systemd_preun konedrive-helper.service
%systemd_user_preun konedrived.service

%postun
# Upgrade only: the helper, and the daemon of every logged-in user, restart
# once the transaction ends, so that the new binaries run together. The cost is
# limitations log Z1: stopping the helper closes its fanotify group, and the
# kernel lets every open still waiting for a download through, so a program
# waiting for a file at that moment reads the placeholder's zeros, and the
# download it waited for is cut off.
%systemd_postun_with_restart konedrive-helper.service
%systemd_user_postun_with_restart konedrived.service

%files
%license LICENSE
%doc README.md SECURITY.md docs
%{_bindir}/konedrive
%{_bindir}/konedrived
%{_bindir}/konedrivectl
%{_libexecdir}/konedrive-helper
%{_unitdir}/konedrive-helper.service
%{_presetdir}/80-konedrive.preset
%{_userunitdir}/konedrived.service
%{_datadir}/dbus-1/services/org.konedrive.Daemon.service
%{_datadir}/applications/org.konedrive.KOneDrive.desktop
%{_datadir}/knotifications6/konedrive.notifyrc

%files kde
%license LICENSE
%dir %{_qt6_plugindir}/kf6/overlayicon
%dir %{_qt6_plugindir}/kf6/kfileitemaction
%{_qt6_plugindir}/kf6/overlayicon/konedriveoverlay.so
%{_qt6_plugindir}/kf6/kfileitemaction/konedriveactions.so

%changelog
* Fri Sep 25 2026 kartas <kartas39@gmail.com> - 0.1.0-1
- First package: konedrive (daemon, command line, window, root helper) and
  konedrive-kde (the Dolphin plugins), built locally from vendored crates.
