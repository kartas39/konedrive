# The checks of a scenario and the suite's count of them; sourced by guest.sh.
#
# A scenario is a function, run by `scenario <function>`: every `check` in it compares what
# came of a step with what was wanted, and the scenario fails when one of them does. A scenario
# named in expected-to-fail.txt is counted apart when it fails, and does not fail the run; one
# named there that passes is reported, so that its line is taken out.

SCENARIO_CHECKS_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
EXPECTED_TO_FAIL_LIST=$SCENARIO_CHECKS_DIR/expected-to-fail.txt
PASSED_SCENARIOS=()
FAILED_SCENARIOS=()
EXPECTED_FAILURES=()
UNEXPECTED_PASSES=()
# Of the scenario that runs now: how many of its checks failed.
FAILED_CHECKS=0

# check <what> <got> <wanted>
check() {
    if [ "$2" = "$3" ]; then
        echo "    ok    $1"
    else
        echo "    FAIL  $1: got \"$2\", wanted \"$3\""
        FAILED_CHECKS=$((FAILED_CHECKS + 1))
    fi
}

# wait_for <seconds> <command…>: until the command succeeds; whether it did.
wait_for() {
    local tries=$(($1 * 5))
    shift
    for _ in $(seq $tries); do
        "$@" && return 0
        sleep 0.2
    done
    return 1
}

# check_within <seconds> <what> <command…>: the command succeeds before the seconds are over.
check_within() {
    local seconds=$1 what=$2
    shift 2
    if wait_for "$seconds" "$@"; then
        echo "    ok    $what"
    else
        echo "    FAIL  $what: not within $seconds s"
        FAILED_CHECKS=$((FAILED_CHECKS + 1))
    fi
}

# check_holds <what> <text> <fragment>: the text holds the fragment.
check_holds() {
    case "$2" in
        *"$3"*) echo "    ok    $1" ;;
        *)
            echo "    FAIL  $1: got \"$2\", which does not hold \"$3\""
            FAILED_CHECKS=$((FAILED_CHECKS + 1))
            ;;
    esac
}

# check_lacks <what> <text> <fragment>: the text does not hold the fragment.
check_lacks() {
    case "$2" in
        *"$3"*)
            echo "    FAIL  $1: got \"$2\", which holds \"$3\""
            FAILED_CHECKS=$((FAILED_CHECKS + 1))
            ;;
        *) echo "    ok    $1" ;;
    esac
}

# Why the scenario is expected to fail; nothing when it is not.
expected_failure_of() {
    awk -F '\t' -v name="$1" '$1 == name { print $2 }' "$EXPECTED_TO_FAIL_LIST"
}

# is_chosen <scenario>: whether the run was asked for it; every scenario, when ONLY is empty.
is_chosen() {
    [ -z "${ONLY:-}" ] && return 0
    case " $ONLY " in *" $1 "*) return 0 ;; esac
    return 1
}

# scenario <function> [<what a failure prints, a function>]
scenario() {
    local name=$1 evidence=${2:-} why
    is_chosen "$name" || return 0
    FAILED_CHECKS=0
    echo "-- $name"
    "$name"
    why=$(expected_failure_of "$name")
    if [ "$FAILED_CHECKS" -eq 0 ]; then
        if [ -n "$why" ]; then
            echo "  UNEXPECTED PASS  $name (expected to fail: $why)"
            UNEXPECTED_PASSES+=("$name")
        else
            echo "  pass  $name"
            PASSED_SCENARIOS+=("$name")
        fi
        return
    fi
    [ -n "$evidence" ] && "$evidence"
    if [ -n "$why" ]; then
        echo "  expected to fail  $name: $why"
        EXPECTED_FAILURES+=("$name")
    else
        echo "  FAILED  $name"
        FAILED_SCENARIOS+=("$name")
    fi
}

# The last lines of a run: what run.sh decides its exit status by.
summarize_scenarios() {
    echo "==== scenarios ===="
    [ ${#EXPECTED_FAILURES[@]} -gt 0 ] && printf 'expected to fail, and failed: %s\n' "${EXPECTED_FAILURES[@]}"
    [ ${#UNEXPECTED_PASSES[@]} -gt 0 ] && printf 'expected to fail, but passed: %s\n' "${UNEXPECTED_PASSES[@]}"
    [ ${#FAILED_SCENARIOS[@]} -gt 0 ] && printf 'FAILED: %s\n' "${FAILED_SCENARIOS[@]}"
    echo "scenarios: ${#PASSED_SCENARIOS[@]} passed, ${#FAILED_SCENARIOS[@]} failed, ${#EXPECTED_FAILURES[@]} expected to fail, ${#UNEXPECTED_PASSES[@]} expected to fail but passed"
}
