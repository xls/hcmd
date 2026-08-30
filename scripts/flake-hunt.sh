#!/bin/sh
# Run the test suite the way CI runs it, repeatedly, and report what fails.
#
#   scripts/flake-hunt.sh [rounds] [cpus]
#
# The reason a test passes here and fails on CI is almost never the code and
# almost always the machine: a GitHub runner has 4 cores, this workstation has
# 32. Tests that race finish in a different order under contention, and the
# ones that measure wall-clock time have three times less of it. So this pins
# the suite to as many CPUs as the runner has and runs it until something
# breaks, which is the only way to see locally what CI sees.
#
# Exit 0 means every round passed. Exit 1 names the first test that did not.
set -eu

cd "$(dirname "$0")/.."

rounds=${1:-5}
cpus=${2:-4}
log=$(mktemp)
trap 'rm -f "$log"' EXIT

if command -v taskset >/dev/null 2>&1; then
    last=$((cpus - 1))
    run="taskset -c 0-$last"
    echo "running the suite on $cpus CPUs, $rounds rounds"
else
    run=""
    echo "no taskset; running unpinned, which will NOT reproduce CI contention"
fi

# Built once, so a round is the tests and not the compiler.
cargo build --tests --quiet

failed=0
round=1
while [ "$round" -le "$rounds" ]; do
    printf 'round %s/%s ... ' "$round" "$rounds"
    start=$(date +%s)
    if $run cargo test --quiet >"$log" 2>&1; then
        echo "ok ($(( $(date +%s) - start ))s)"
    else
        echo "FAILED ($(( $(date +%s) - start ))s)"
        echo
        echo "--- the tests that failed:"
        grep -E '^(test .* FAILED|---- .* stdout)' "$log" | sort -u | head -20
        echo
        echo "--- why:"
        grep -A 4 'panicked at' "$log" | head -30
        failed=1
        break
    fi
    round=$((round + 1))
done

if [ "$failed" -eq 0 ]; then
    echo "$rounds rounds clean on $cpus CPUs"
fi
exit "$failed"
