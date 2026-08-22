#!/bin/bash
# Integration tests for cleanup-build-artifacts.sh.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CLEANUP_SCRIPT="$SCRIPT_DIR/cleanup-build-artifacts.sh"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/temper-cleanup-test.XXXXXX")"
TEST_ROOT="$(cd "$TEST_ROOT" && pwd -P)"

cleanup() {
    if [ -n "${TEST_ROOT:-}" ] && [ "$TEST_ROOT" != "/" ] && [ -d "$TEST_ROOT" ]; then
        rm -rf "$TEST_ROOT"
    fi
}
trap cleanup EXIT

REPO="$TEST_ROOT/repo"
MERGED="$TEST_ROOT/merged"
MERGED_ALL="$TEST_ROOT/merged-all"
UNMERGED="$TEST_ROOT/unmerged"
DIRTY="$TEST_ROOT/dirty"
SYMLINKED="$TEST_ROOT/symlinked"

git init -q -b main "$REPO"
git -C "$REPO" config user.email "cleanup-test@temper.invalid"
git -C "$REPO" config user.name "Temper Cleanup Test"
printf 'fixture\n' > "$REPO/fixture.txt"
printf 'target/\n' > "$REPO/.gitignore"
git -C "$REPO" add fixture.txt .gitignore
git -C "$REPO" -c commit.gpgsign=false commit -q -m "fixture"

git -C "$REPO" worktree add -q -b merged "$MERGED" main
git -C "$REPO" worktree add -q -b merged-all "$MERGED_ALL" main
git -C "$REPO" worktree add -q -b unmerged "$UNMERGED" main
git -C "$REPO" worktree add -q -b dirty "$DIRTY" main
git -C "$REPO" worktree add -q -b symlinked "$SYMLINKED" main

printf 'unmerged\n' > "$UNMERGED/unmerged.txt"
git -C "$UNMERGED" add unmerged.txt
git -C "$UNMERGED" -c commit.gpgsign=false commit -q -m "unmerged"
printf 'dirty\n' >> "$DIRTY/fixture.txt"

mkdir -p "$REPO/target" "$MERGED/target" "$MERGED/nested/target" \
    "$MERGED_ALL/target" "$UNMERGED/target" "$DIRTY/target"
printf 'build\n' > "$REPO/target/output"
printf 'build\n' > "$MERGED/target/output"
printf 'nested build\n' > "$MERGED/nested/target/output"
printf 'build\n' > "$MERGED_ALL/target/output"
printf 'build\n' > "$UNMERGED/target/output"
printf 'build\n' > "$DIRTY/target/output"
mkdir -p "$TEST_ROOT/outside-target"
ln -s "$TEST_ROOT/outside-target" "$SYMLINKED/target"

DRY_RUN_OUTPUT="$(cd "$REPO" && "$CLEANUP_SCRIPT")"
printf '%s\n' "$DRY_RUN_OUTPUT" | grep -F "stale" | grep -F "$MERGED" >/dev/null
printf '%s\n' "$DRY_RUN_OUTPUT" | grep -F "active" | grep -F "$REPO" >/dev/null
printf '%s\n' "$DRY_RUN_OUTPUT" | grep -F "active" | grep -F "$UNMERGED" >/dev/null
printf '%s\n' "$DRY_RUN_OUTPUT" | grep -F "active" | grep -F "$DIRTY" >/dev/null
printf '%s\n' "$DRY_RUN_OUTPUT" | grep -F "unsafe" | grep -F "$SYMLINKED" >/dev/null

[ -d "$MERGED/target" ]
(cd "$REPO" && "$CLEANUP_SCRIPT" --apply --worktree "$MERGED") >/dev/null
[ ! -e "$MERGED/target" ]
[ -d "$MERGED/nested/target" ]
[ -d "$REPO/target" ]
[ -d "$UNMERGED/target" ]

(cd "$REPO" && "$CLEANUP_SCRIPT" --apply --all-stale) >/dev/null
[ ! -e "$MERGED_ALL/target" ]
[ -d "$MERGED/nested/target" ]
[ -d "$REPO/target" ]
[ -d "$UNMERGED/target" ]
[ -d "$DIRTY/target" ]
[ -L "$SYMLINKED/target" ]
[ -d "$TEST_ROOT/outside-target" ]

if (cd "$REPO" && "$CLEANUP_SCRIPT" --apply --worktree "$UNMERGED") >/dev/null 2>&1; then
    printf 'expected unmerged target selection to fail\n' >&2
    exit 1
fi
[ -d "$UNMERGED/target" ]

if (cd "$REPO" && "$CLEANUP_SCRIPT" --apply) >/dev/null 2>&1; then
    printf 'expected unscoped apply to fail\n' >&2
    exit 1
fi

printf 'cleanup-build-artifacts tests passed\n'
