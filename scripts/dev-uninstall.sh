#!/bin/sh
# Removes what scripts/dev-install.sh installed for the current user, as
# yourself: no root. Run it before installing the RPM packages, whose files
# the ones under ~/.local and ~/.config would otherwise override (README,
# "Switching from the developer install").
#
# It stops the daemon, removes exactly the files dev-install.sh puts in place,
# and reloads the user's systemd and D-Bus. It never touches your settings
# (~/.config/konedrive/config.toml), the tree store and activity log
# (~/.local/state/konedrive), the refresh token in KWallet, or the sync folder:
# the packaged daemon picks them up where this one left them.
set -eu

# The same locations as dev-install.sh.
prefix="$HOME/.local"
config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"

remove() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        rm -f "$1"
        echo "removed $1"
    fi
}

# Disabled first, while its unit file is there to say what to disable (in case
# it was enabled by hand; dev-install.sh does not), and stopped only once its
# program is gone, so that nothing can start it again in between.
systemctl --user disable konedrived.service 2>/dev/null || true

# What dev-install.sh copies itself...
remove "$prefix/bin/konedrived"
remove "$prefix/bin/konedrivectl"
remove "$config_home/systemd/user/konedrived.service"
remove "$data_home/dbus-1/services/org.konedrive.Daemon.service"
# ...and what its `cmake --install` of app/ puts under the same prefix
# (app/CMakeLists.txt's install rules).
remove "$prefix/bin/konedrive"
remove "$prefix/share/applications/org.konedrive.KOneDrive.desktop"
remove "$prefix/share/knotifications6/konedrive.notifyrc"

systemctl --user stop konedrived.service 2>/dev/null || true
systemctl --user daemon-reload || true
busctl --user call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig >/dev/null || true

# "Start at login" (limitations log A4): the entry the window wrote runs the
# program just removed. Pointed at the one the konedrive package installs, so
# that it keeps working once the package is in.
autostart="$config_home/autostart/org.konedrive.KOneDrive.desktop"
if [ -f "$autostart" ] && grep -qxF "Exec=$prefix/bin/konedrive --background" "$autostart"; then
    tmp="$autostart.dev-uninstall"
    awk -v old="Exec=$prefix/bin/konedrive --background" \
        '$0 == old { print "Exec=/usr/bin/konedrive --background"; next } { print }' \
        "$autostart" >"$tmp"
    mv -f "$tmp" "$autostart"
    echo "\"Start at login\" now runs /usr/bin/konedrive, where the konedrive package installs it"
elif [ -f "$autostart" ] && grep -qF "$prefix/bin/konedrive" "$autostart"; then
    echo "note: $autostart still runs $prefix/bin/konedrive; once the"
    echo "package is installed, switch \"Start at login\" off and on again in the window's Settings."
fi

# Not dev-install.sh's, so only pointed out: the Dolphin plugins installed for
# this user by hand (README, "Dolphin integration"), which would be loaded
# instead of the konedrive-kde package's.
for plugin in "$prefix/lib64/plugins/kf6/overlayicon/konedriveoverlay.so" \
        "$prefix/lib64/plugins/kf6/kfileitemaction/konedriveactions.so" \
        "$config_home/plasma-workspace/env/konedrive-dolphin.sh"; do
    if [ -e "$plugin" ]; then
        echo "note: $plugin is from the per-user Dolphin install; remove it by hand if the"
        echo "konedrive-kde package is to provide the plugins."
    fi
done

if pgrep -u "$(id -u)" -x konedrive >/dev/null 2>&1; then
    echo "note: the KOneDrive window is still running from the removed program: quit it from"
    echo "its tray icon, and start it again from the launcher once the package is installed."
fi

echo "The developer install is removed. Your settings, the tree store and the sync folder are"
echo "as they were."
if [ -e /usr/local/libexec/konedrive-helper ] || [ -e /etc/systemd/system/konedrive-helper.service ]; then
    echo "The helper from scripts/install-helper.sh is still installed; to switch to the packages,"
    echo "remove it next (the folder stays registered, and the packaged helper takes it over):"
    echo "    sudo scripts/install-helper.sh --uninstall --force"
fi
