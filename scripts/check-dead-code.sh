#!/bin/sh
# konedrived holds nothing that nothing calls (issue #262).
#
# The compiler finds dead code only in what is not `pub`: in konedrived an item is `pub` only
# when something outside the crate uses it, and `pub(crate)` otherwise. This builds the
# library as it ships, with each feature that changes what is called, and with its own
# tests, and fails on an item never used, an import never used, and a `pub` item that
# names a type that is not `pub`.
#
#     scripts/check-dead-code.sh
#
# A function only the crate's tests call is `#[cfg(test)]`; one only a feature calls has
# that feature's `cfg`.
set -eu
cd "$(dirname "$0")/.."

deny="-D dead_code -D unused_imports -D private_interfaces -D private_bounds"

check() {
    echo "== konedrived: $*"
    # shellcheck disable=SC2086
    cargo rustc --locked -p konedrived --lib "$@" -- $deny
}

check --profile check
check --profile check --features dev-tools
check --profile check --all-features
check --profile test --all-features
echo "check-dead-code: nothing in konedrived is dead"
