#!/bin/sh
# Runs the KIO probe against a scratch home on a private session bus: the
# user's own thumbnail cache, configuration and session bus are never touched.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
build=${BUILD:-$here/../../build/kio-probe}
cmake -S "$here" -B "$build" -DCMAKE_BUILD_TYPE=RelWithDebInfo >/dev/null
cmake --build "$build" >/dev/null

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/home/folder" "$scratch/cache" "$scratch/config" "$scratch/data" "$scratch/runtime"
chmod 700 "$scratch/runtime"

HOME="$scratch/home" \
XDG_CACHE_HOME="$scratch/cache" \
XDG_CONFIG_HOME="$scratch/config" \
XDG_DATA_HOME="$scratch/data" \
XDG_RUNTIME_DIR="$scratch/runtime" \
QT_QPA_PLATFORM=offscreen \
    dbus-run-session -- "$build/kio_probe" "$scratch/home/folder" "$scratch/cache/thumbnails"
