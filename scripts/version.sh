#!/bin/sh
# The version a build carries, computed from the repository's git tags vX.Y.Z.
# Used by scripts/build-rpm.sh and by the release workflow
# (.github/workflows/release.yml), so that both choose it the same way
# (docs/releasing.md).
#
#     scripts/version.sh release   # X.Y.Z: the release this commit is, or would be
#     scripts/version.sh local     # X.Y.Z~dev.YYYYMMDD.SHA: a local build (RPM form)
#     scripts/version.sh previous X.Y.Z   # the tag before vX.Y.Z, if any
#
# Or sourced (`. scripts/version.sh`), for the functions alone. Each takes the
# repository's root as its first argument. Only tags the checkout has are seen:
# `git fetch --tags` first.
#
# The next release is the highest tag with one added to its last number, or
# 0.1.1 with no tag at all. A tag pushed by hand sets a new base: after v0.2.0
# comes 0.2.1. The version in Cargo.toml and in the spec is a placeholder.

# Every tag vX.Y.Z of the repository as X.Y.Z, lowest first.
konedrive_tagged_versions() {
    git -C "$1" tag --list 'v[0-9]*' |
        sed -n 's/^v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$/\1/p' |
        sort -t. -k1,1n -k2,2n -k3,3n
}

# The version after the highest tag: X.Y.(Z+1), or 0.1.1 with no tag.
konedrive_next_version() {
    kv_last=$(konedrive_tagged_versions "$1" | tail -n 1)
    if [ -z "$kv_last" ]; then
        echo 0.1.1
    else
        echo "${kv_last%.*}.$((${kv_last##*.} + 1))"
    fi
}

# A release's version: a tag already on HEAD is reused (a rerun after the tag
# was pushed makes no second one); otherwise the next version.
konedrive_release_version() {
    kv_here=$(git -C "$1" tag --points-at HEAD --list 'v[0-9]*' |
        sed -n 's/^v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$/\1/p' |
        sort -t. -k1,1n -k2,2n -k3,3n | tail -n 1)
    if [ -n "$kv_here" ]; then
        echo "$kv_here"
    else
        konedrive_next_version "$1"
    fi
}

# A local build's version, in RPM's form: the next version with a suffix that
# sorts below it (RPM's `~`), from HEAD's commit date (UTC) and short hash, so
# that the next release upgrades it. Cargo's form has `-` for `~`
# (konedrive_cargo_version).
konedrive_local_version() {
    kv_date=$(TZ=UTC git -C "$1" log -1 --format=%cd --date=format-local:%Y%m%d HEAD)
    kv_sha=$(git -C "$1" rev-parse --short=7 HEAD)
    echo "$(konedrive_next_version "$1")~dev.$kv_date.$kv_sha"
}

# Cargo's form of an RPM version: Cargo refuses `~`, RPM refuses `-`.
konedrive_cargo_version() {
    echo "$1" | tr '~' '-'
}

# The highest tag below version $2, as vX.Y.Z; nothing if there is none.
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
            previous) konedrive_previous_tag "$kv_root" "${2:?previous needs a version}" ;;
            *) echo "usage: $0 release | local | previous X.Y.Z" >&2; exit 2 ;;
        esac
        ;;
esac
