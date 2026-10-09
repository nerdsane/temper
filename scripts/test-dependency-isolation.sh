#!/usr/bin/env bash
# Exercise the same guard CI invokes, with controlled Cargo output and exits.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GUARD="${1:-$ROOT/scripts/check-dependency-isolation.sh}"
TASK_TMP="$(mktemp -d "${TMPDIR:-/tmp}/temper-dependency-test.XXXXXX")"
trap 'rm -rf "$TASK_TMP"' EXIT
mkdir "$TASK_TMP/bin"

cat > "$TASK_TMP/bin/cargo" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$CALLS"
# The old CI query is accepted here so the RED detects its swallowed failure,
# rather than merely testing an argument mismatch.
crate=''
while [[ $# -gt 0 ]]; do
    if [[ "$1" == '-p' ]]; then
        crate="$2"
        break
    fi
    shift
done
if [[ "$crate" == "$TARGET_CRATE" ]]; then
    case "$MODE" in
        cargo-error)
            printf '%s v0.1.0\n' "$crate"
            echo 'cargo fixture: dependency query failed' >&2
            exit 42
            ;;
        sigpipe)
            printf '%s v0.1.0\n' "$crate"
            echo 'cargo fixture: interrupted output' >&2
            exit 141
            ;;
        empty) exit 0 ;;
        whitespace) printf ' \t\n'; exit 0 ;;
    esac
fi
printf '%s v0.1.0\n' "$crate"
if [[ "$crate" == "$TARGET_CRATE" ]]; then
    case "$MODE" in
        forbidden|large-forbidden)
            printf '├── %s v1.0.0\n' "$FORBIDDEN"
            ;;
        allowed)
            printf '├── proptest-helper v1.0.0\n└── stateright-helper v1.0.0\n'
            ;;
    esac
    if [[ "$MODE" == 'large-forbidden' ]]; then
        for ((i = 0; i < 10000; i++)); do
            printf '├── allowed-dependency-%s v1.0.0\n' "$i"
        done
    fi
fi
MOCK
chmod +x "$TASK_TMP/bin/cargo"

check_case() {
    local name="$1" mode="$2" crate="$3" forbidden="$4" expected="$5" diagnostic="$6"
    local actual=0
    local output="$TASK_TMP/$name.stdout" errors="$TASK_TMP/$name.stderr"
    local calls="$TASK_TMP/$name.calls"
    if PATH="$TASK_TMP/bin:$PATH" MODE="$mode" TARGET_CRATE="$crate" \
        FORBIDDEN="$forbidden" CALLS="$calls" bash "$GUARD" > "$output" 2> "$errors"; then
        actual=0
    else
        actual=$?
    fi
    if [[ "$actual" -ne "$expected" ]]; then
        cat "$output" "$errors" >&2
        printf 'FAIL %s: expected exit %s, got %s\n' "$name" "$expected" "$actual" >&2
        exit 1
    fi
    if [[ "$expected" -eq 0 ]]; then
        grep -Fx 'Dependency isolation: OK' "$output"
        printf 'tree --edges no-dev -p %s --color never\n' \
            temper-jit temper-server temper-runtime > "$TASK_TMP/expected.calls"
        diff -u "$TASK_TMP/expected.calls" "$calls"
    else
        if grep -F 'Dependency isolation: OK' "$output" "$errors"; then
            printf 'FAIL %s: failure reported success\n' "$name" >&2
            exit 1
        fi
        grep -F "$diagnostic" "$errors"
    fi
    printf 'PASS %s (exit %s)\n' "$name" "$actual"
}

# First case is the discriminating regression for the original if-pipeline.
check_case cargo-error-jit cargo-error temper-jit '' 42 'cargo fixture: dependency query failed'
check_case allowed allowed temper-jit '' 0 ''
check_case jit-verifier forbidden temper-jit temper-verify 1 'temper-jit has production dependency on temper-verify'
# temper-verify itself is only forbidden in temper-jit, not the other roots.
for crate in temper-server temper-runtime; do
    check_case "$crate-verifier-allowed" forbidden "$crate" temper-verify 0 ''
done
for crate in temper-jit temper-server temper-runtime; do
    for forbidden in stateright proptest; do
        check_case "$crate-$forbidden" forbidden "$crate" "$forbidden" 1 "$crate production binary includes stateright or proptest"
    done
    check_case "$crate-empty" empty "$crate" '' 1 "cargo tree returned an empty dependency graph for $crate"
done
for crate in temper-server temper-runtime; do
    check_case "$crate-cargo-error" cargo-error "$crate" '' 42 'cargo fixture: dependency query failed'
done
check_case whitespace whitespace temper-jit '' 1 'cargo tree returned an empty dependency graph for temper-jit'
check_case sigpipe sigpipe temper-jit '' 141 'cargo fixture: interrupted output'
check_case large-forbidden large-forbidden temper-jit proptest 1 'temper-jit production binary includes stateright or proptest'
