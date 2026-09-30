#!/bin/sh
# The version a build carries. It lives in one place: `version` in
# [workspace.package] of the root Cargo.toml, the next release's version,
# X.Y.Z. Used by scripts/build-rpm.sh and by the release workflow
# (.github/workflows/release.yml), so that both choose it the same way
# (docs/releasing.md, "The version").
#
#     scripts/version.sh release   # X.Y.Z: the file's version
#     scripts/version.sh local     # X.Y.Z~dev.N: any other build (RPM form)
#     scripts/version.sh commit    # HEAD's full hash
#     scripts/version.sh previous X.Y.Z   # the tag before vX.Y.Z, if any
#
# Or sourced (`. scripts/version.sh`), for the functions alone. Each takes the
# repository's root as its first argument.
#
# N is the number of commits in HEAD's history (`git rev-list --count HEAD`):
# local history only, no tags, no network. It grows with every merge into dev,
# so a newer build upgrades an older one, and the release X.Y.Z upgrades every
# X.Y.Z~dev.N (RPM sorts `~` below the version without it).

# The version in [workspace.package] of Cargo.toml; fails unless it is X.Y.Z.
konedrive_file_version() {
    kv_file=$(sed -n '/^\[workspace\.package\]$/,/^\[/s/^version = "\(.*\)"$/\1/p' "$1/Cargo.toml" | head -n 1)
    if ! echo "$kv_file" | grep -qx '[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*'; then
        echo "no X.Y.Z version in [workspace.package] of $1/Cargo.toml (found '$kv_file')" >&2
        return 1
    fi
    echo "$kv_file"
}

# A release's version: the file's.
konedrive_release_version() {
    konedrive_file_version "$1"
}

# Any other build's version, in RPM's form: the file's version with a suffix
# that sorts below it (RPM's `~`) and grows with HEAD's history. Cargo's form
# has `-` for `~` (konedrive_cargo_version).
konedrive_local_version() {
    kv_base=$(konedrive_file_version "$1") || return 1
    kv_count=$(git -C "$1" rev-list --count HEAD) || return 1
    echo "$kv_base~dev.$kv_count"
}

# HEAD's full hash.
konedrive_commit() {
    git -C "$1" rev-parse HEAD
}

# Fails, saying why, unless $2 is the file's version: a release is built only
# with the version the repository holds.
konedrive_check_release_version() {
    kv_want=$(konedrive_file_version "$1") || return 1
    if [ "$2" != "$kv_want" ]; then
        echo "--version $2 is not the version in Cargo.toml ($kv_want); a release carries the file's version (docs/releasing.md)" >&2
        return 1
    fi
}

# Cargo's form of an RPM version: Cargo refuses `~`, RPM refuses `-`.
konedrive_cargo_version() {
    echo "$1" | tr '~' '-'
}

# Every tag vX.Y.Z of the repository as X.Y.Z, lowest first.
konedrive_tagged_versions() {
    git -C "$1" tag --list 'v[0-9]*' |
        sed -n 's/^v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$/\1/p' |
        sort -t. -k1,1n -k2,2n -k3,3n
}

# The highest tag below version $2, as vX.Y.Z; nothing if there is none. Only
# tags the checkout has are seen (the release workflow fetches them all).
konedrive_previous_tag() {
    konedrive_tagged_versions "$1" | awk -F. -v want="$2" '
        BEGIN { split(want, w, ".") }
        ($1 < w[1]) || ($1 == w[1] && $2 < w[2]) || ($1 == w[1] && $2 == w[2] && $3 < w[3]) { last = $0 }
        END { if (last != "") print "v" last }'
}

# Run as a program rather than sourced.
case $0 in
    */version.sh | version.sh)
        set -eu
        kv_root=$(cd "$(dirname "$0")/.." && pwd)
        case ${1-} in
            release) konedrive_release_version "$kv_root" ;;
            local) konedrive_local_version "$kv_root" ;;
            commit) konedrive_commit "$kv_root" ;;
            previous) konedrive_previous_tag "$kv_root" "${2:?previous needs a version}" ;;
            *) echo "usage: $0 release | local | commit | previous X.Y.Z" >&2; exit 2 ;;
        esac
        ;;
esac
