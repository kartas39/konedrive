#!/bin/sh
# Builds the RPM packages from the committed tree (HEAD), as yourself: no root.
#
#     scripts/build-rpm.sh                # both binary packages and the source RPM
#     scripts/build-rpm.sh --sources-only # only the two source tarballs
#
# Everything lands under target/rpm/ in this repository (rpmbuild's _topdir);
# the RPMs are listed at the end. Needs rpmbuild (`sudo dnf install rpm-build`)
# and the spec's build dependencies (`sudo dnf builddep packaging/rpm/konedrive.spec`).
# Uncommitted changes are not packaged.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
spec="$root/packaging/rpm/konedrive.spec"
top="$root/target/rpm"

sources_only=no
for arg in "$@"; do
    case $arg in
        --sources-only) sources_only=yes ;;
        *) echo "unknown argument: $arg (see the top of this script)" >&2; exit 2 ;;
    esac
done

if [ "$sources_only" = no ] && ! command -v rpmbuild >/dev/null 2>&1; then
    echo "rpmbuild is not installed: sudo dnf install rpm-build" >&2
    exit 1
fi

# The version is Cargo's ([workspace.package] in Cargo.toml), and the spec must
# say the same.
cargo_version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml")
spec_version=$(sed -n 's/^Version: *//p' "$spec")
if [ -z "$cargo_version" ] || [ "$cargo_version" != "$spec_version" ]; then
    echo "Cargo.toml says version '$cargo_version' and $spec says '$spec_version':" >&2
    echo "make them the same (and add a changelog entry to the spec)." >&2
    exit 1
fi
name=konedrive-$cargo_version

if [ -n "$(git -C "$root" status --porcelain --untracked-files=no)" ]; then
    echo "note: the working tree has uncommitted changes; only what HEAD holds is packaged." >&2
fi

# Start from nothing, so that the output directories hold only this build.
rm -rf "$top"
mkdir -p "$top/SOURCES" "$top/vendor-stage"

# Source0: the committed tree.
git -C "$root" archive --format=tar.gz --prefix="$name/" -o "$top/SOURCES/$name.tar.gz" HEAD

# Source1: the crates of that tree's Cargo.lock, and the configuration that
# `cargo vendor` prints to use them, saved as .cargo/config.toml. Vendored
# from an unpacked copy of Source0, so that the lock file is HEAD's.
stage="$top/vendor-stage"
tar -xzf "$top/SOURCES/$name.tar.gz" -C "$stage"
mkdir -p "$stage/.cargo"
(cd "$stage" && cargo vendor --locked --manifest-path "$name/Cargo.toml" vendor) >"$stage/.cargo/config.toml"
tar -czf "$top/SOURCES/$name-vendor.tar.gz" -C "$stage" vendor .cargo
rm -rf "$stage"

if [ "$sources_only" = yes ]; then
    echo "Source tarballs:"
    ls -1 "$top/SOURCES/$name.tar.gz" "$top/SOURCES/$name-vendor.tar.gz"
    exit 0
fi

rpmbuild -ba --define "_topdir $top" "$spec"

echo
echo "Built:"
find "$top/RPMS" "$top/SRPMS" -name '*.rpm' | sort
echo
echo "Install both packages (see README, \"Install from RPM\"):"
echo "    sudo dnf install $top/RPMS/*/konedrive-*$cargo_version-*.rpm"
