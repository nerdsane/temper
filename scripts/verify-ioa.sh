#!/bin/bash
# Run `temper verify-ioa` on IOA specs: each one parses and passes its
# verification cascade on its own. A directory stands for every *.ioa.toml in
# it. This is the local check, and it runs no tests: tests run in CI.
#
# The checks that need a whole source directory (lint, CSDL bindings, the spec
# model, policies and the composite check over several entities) run in CI,
# in verify::repository_tests. `temper verify` checks complete applications
# only and has no partial modes (c7996e098), so they have no command to run
# here.
set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ "$#" -eq 0 ]; then
    echo "Usage: $0 SPEC_OR_DIRECTORY..." >&2
    exit 2
fi

cd "$WORKSPACE_ROOT"

FAILED=0
verify() {
    # verify-ioa prints the cascade's result as JSON and exits non-zero when a
    # level fails or the spec does not parse.
    local RESULT
    if RESULT="$(cargo run --locked --quiet -p temper-cli --bin temper -- verify-ioa < "$1" 2>&1)"; then
        echo "PASS $1" >&2
    else
        echo "FAIL $1" >&2
        echo "$RESULT" >&2
        FAILED=1
    fi
}

for ARG in "$@"; do
    case "$ARG" in
        /*) ;;
        *) ARG="$OLDPWD/$ARG" ;;
    esac
    if [ -d "$ARG" ]; then
        for SPEC in "$ARG"/*.ioa.toml; do
            if [ -e "$SPEC" ]; then
                verify "$SPEC"
            fi
        done
    else
        verify "$ARG"
    fi
done
exit "$FAILED"
