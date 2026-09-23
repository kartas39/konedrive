#!/bin/sh
# Builds everything and installs it for the current user under ~/.local.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
prefix="$HOME/.local"
config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"

cargo build --release --manifest-path "$root/Cargo.toml" -p konedrived -p konedrivectl
install -Dm755 "$root/target/release/konedrived" "$prefix/bin/konedrived"
install -Dm755 "$root/target/release/konedrivectl" "$prefix/bin/konedrivectl"
install -Dm644 "$root/packaging/systemd/konedrived.service" "$config_home/systemd/user/konedrived.service"
install -Dm644 "$root/packaging/dbus/org.konedrive.Daemon.service" "$data_home/dbus-1/services/org.konedrive.Daemon.service"

cmake -S "$root/app" -B "$root/build/app-release" -DCMAKE_INSTALL_PREFIX="$prefix" -DCMAKE_BUILD_TYPE=RelWithDebInfo -DBUILD_TESTING=OFF
cmake --build "$root/build/app-release"
cmake --install "$root/build/app-release"

systemctl --user daemon-reload
busctl --user call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig >/dev/null
systemctl --user try-restart konedrived.service

echo "Installed. Open KOneDrive from the application launcher, or run: $prefix/bin/konedrive"
