#!/bin/sh
# Builds the RPM packages from the committed tree (HEAD), as yourself: no root.
#
#     scripts/build-rpm.sh                   # both binary packages and the source RPM
#     scripts/build-rpm.sh --sources-only    # only the two source tarballs
#     scripts/build-rpm.sh --version X.Y.Z   # a release's version (the release workflow)
#
# Everything lands under target/rpm/ in this repository (rpmbuild's _topdir);
# the RPMs are listed at the end. Needs rpmbuild (`sudo dnf install rpm-build`)
# and the spec's build dependencies (`sudo dnf builddep packaging/rpm/konedrive.spec`).
# Uncommitted changes are not packaged.
#
# The version: without --version, a local build's, from the git tags
# (scripts/version.sh; run `git fetch --tags` first), such as
# 0.1.2~dev.20260929.fad78d9, which the release 0.1.2 upgrades. It is written
# into the build's copies of Cargo.toml, Cargo.lock and the spec; the versions
# in git are placeholders (docs/releasing.md).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
spec="$root/packaging/rpm/konedrive.spec"
top="$root/target/rpm"
. "$root/scripts/version.sh"

sources_only=no
version=
while [ $# -gt 0 ]; do
    case $1 in
        --sources-only) sources_only=yes ;;
        --version)
            [ $# -ge 2 ] || { echo "--version needs a version, X.Y.Z" >&2; exit 2; }
            version=$2
            shift
            ;;
        *) echo "unknown argument: $1 (see the top of this script)" >&2; exit 2 ;;
    esac
    shift
done

if [ "$sources_only" = no ] && ! command -v rpmbuild >/dev/null 2>&1; then
    echo "rpmbuild is not installed: sudo dnf install rpm-build" >&2
    exit 1
fi

if [ -n "$version" ]; then
    if ! echo "$version" | grep -qx '[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*'; then
        echo "--version takes a release's version, X.Y.Z, not '$version'" >&2
        exit 2
    fi
else
    if [ -z "$(konedrive_tagged_versions "$root")" ]; then
        echo "note: no vX.Y.Z tag in this checkout; if the repository has some, run git fetch --tags." >&2
    fi
    version=$(konedrive_local_version "$root")
fi
# RPM's form (`~` for a local build) and Cargo's (`-`).
cargo_version=$(konedrive_cargo_version "$version")
name=konedrive-$version

if [ -n "$(git -C "$root" status --porcelain --untracked-files=no)" ]; then
    echo "note: the working tree has uncommitted changes; only what HEAD holds is packaged." >&2
fi

# Start from nothing, so that the output directories hold only this build.
rm -rf "$top"
mkdir -p "$top/SOURCES" "$top/SPECS" "$top/tree" "$top/vendor-stage"

# The committed tree, with the version written into its copies of Cargo.toml,
# Cargo.lock (every package without a source: the workspace's own) and the
# spec (Version:, and a changelog entry above the others, dated with HEAD's
# commit, in the name of the spec's newest entry). The build stops if a line
# it expects is not there.
tree="$top/tree/$name"
git -C "$root" archive --format=tar --prefix="$name/" HEAD | tar -xf - -C "$top/tree"

sed -i '/^\[workspace\.package\]$/,/^\[/s/^version = ".*"$/version = "'"$cargo_version"'"/' "$tree/Cargo.toml"
sed -n '/^\[workspace\.package\]$/,/^\[/p' "$tree/Cargo.toml" | grep -qx "version = \"$cargo_version\""

awk -v v="$cargo_version" '
    function flush(   i) {
        for (i = 1; i <= n; i++) {
            if (package && !source && buf[i] ~ /^version = /) buf[i] = "version = \"" v "\""
            print buf[i]
        }
        n = 0; package = 0; source = 0
    }
    /^$/ { flush(); print; next }
    { buf[++n] = $0 }
    $0 == "[[package]]" { package = 1 }
    /^source = / { source = 1 }
    END { flush() }' "$tree/Cargo.lock" >"$top/Cargo.lock"
mv "$top/Cargo.lock" "$tree/Cargo.lock"
grep -A1 -x 'name = "konedrived"' "$tree/Cargo.lock" | grep -qx "version = \"$cargo_version\""

packager=$(sed -n '/^%changelog$/,$s/^\* [A-Z][a-z][a-z] [A-Z][a-z][a-z] [0-9][0-9]* [0-9][0-9]* \(.*\) - .*$/\1/p' "$spec" | head -n 1)
[ -n "$packager" ] || { echo "no changelog entry to take the packager from in $spec" >&2; exit 1; }
date=$(LC_ALL=C TZ=UTC git -C "$root" log -1 --format=%cd --date='format-local:%a %b %d %Y' HEAD)
sha=$(git -C "$root" rev-parse --short=7 HEAD)
awk -v version="$version" -v entry="* $date $packager - $version-1" -v sha="$sha" '
    /^Version:/ { sub(/[^ ]+$/, version); print; next }
    /^%changelog$/ { print; print entry; print "- Built from commit " sha "."; print ""; next }
    { print }' "$spec" >"$top/SPECS/konedrive.spec"
grep -qx "Version: *$version" "$top/SPECS/konedrive.spec"
cp "$top/SPECS/konedrive.spec" "$tree/packaging/rpm/konedrive.spec"

# Source0: that tree.
tar -czf "$top/SOURCES/$name.tar.gz" -C "$top/tree" "$name"

# Source1: the crates of that tree's Cargo.lock, and the configuration that
# `cargo vendor` prints to use them, saved as .cargo/config.toml. --locked
# also stops the build if the rewritten lock file does not fit Cargo.toml.
stage="$top/vendor-stage"
mkdir -p "$stage/.cargo"
(cd "$stage" && cargo vendor --locked --manifest-path "$tree/Cargo.toml" vendor) >"$stage/.cargo/config.toml"
tar -czf "$top/SOURCES/$name-vendor.tar.gz" -C "$stage" vendor .cargo
rm -rf "$stage" "$top/tree"

if [ "$sources_only" = yes ]; then
    echo "Source tarballs ($version):"
    ls -1 "$top/SOURCES/$name.tar.gz" "$top/SOURCES/$name-vendor.tar.gz"
    exit 0
fi

rpmbuild -ba --define "_topdir $top" "$top/SPECS/konedrive.spec"

echo
echo "Built ($version):"
find "$top/RPMS" "$top/SRPMS" -name '*.rpm' | sort
echo
echo "Install both packages (see README, \"Install from RPM\"):"
echo "    sudo dnf install $top/RPMS/*/konedrive-*$version-*.rpm"
