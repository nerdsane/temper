#!/bin/bash
# Safely inventory or remove stale top-level Cargo target directories.
set -euo pipefail

MODE="dry-run"
BASE_REF="main"
ALL_STALE=false
SELECTED_WORKTREES=()

usage() {
    cat <<'EOF'
Usage: scripts/cleanup-build-artifacts.sh [OPTIONS]

Inventory registered Temper worktrees and classify their exact top-level
target/ directories. Dry-run is the default.

Options:
  --apply              Remove eligible targets. Requires --worktree or --all-stale.
  --worktree PATH      Select one registered worktree. May be repeated.
  --all-stale          Select every eligible stale target.
  --base REF           Merge base used to identify stale worktrees (default: main).
  -h, --help           Show this help.

Eligibility requires a registered, clean, non-invoking worktree whose HEAD is
an ancestor of BASE. Symlinks and paths outside the exact worktree root are
always rejected. Cargo registry/git caches and nested target directories are
outside this command's scope.
EOF
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 2
}

physical_dir() {
    (cd "$1" 2>/dev/null && pwd -P)
}

human_kib() {
    awk -v kib="$1" 'BEGIN {
        if (kib >= 1048576) printf "%.1f GiB", kib / 1048576;
        else if (kib >= 1024) printf "%.1f MiB", kib / 1024;
        else printf "%d KiB", kib;
    }'
}

is_selected() {
    local candidate="$1"
    local selected
    if [ "${#SELECTED_WORKTREES[@]}" -eq 0 ]; then
        return 1
    fi
    for selected in "${SELECTED_WORKTREES[@]}"; do
        if [ "$candidate" = "$selected" ]; then
            return 0
        fi
    done
    return 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --apply)
            MODE="apply"
            shift
            ;;
        --worktree)
            [ "$#" -ge 2 ] || fail "--worktree requires a path"
            SELECTED_WORKTREES+=("$(physical_dir "$2")")
            shift 2
            ;;
        --all-stale)
            ALL_STALE=true
            shift
            ;;
        --base)
            [ "$#" -ge 2 ] || fail "--base requires a ref"
            BASE_REF="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown option: $1"
            ;;
    esac
done

if [ "$MODE" = "apply" ] && [ "$ALL_STALE" = false ] && [ "${#SELECTED_WORKTREES[@]}" -eq 0 ]; then
    fail "--apply requires --worktree PATH or --all-stale"
fi

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || fail "run inside a git worktree"
REPO_ROOT="$(physical_dir "$REPO_ROOT")"
BASE_COMMIT="$(git rev-parse --verify "${BASE_REF}^{commit}" 2>/dev/null)" || fail "unknown base ref: $BASE_REF"

if [ "${#SELECTED_WORKTREES[@]}" -gt 0 ]; then
    for selected in "${SELECTED_WORKTREES[@]}"; do
        if ! git worktree list --porcelain | grep -Fqx "worktree $selected"; then
            fail "not a registered worktree: $selected"
        fi
    done
fi

printf 'Mode: %s\nBase: %s (%s)\n' "$MODE" "$BASE_REF" "$BASE_COMMIT"
printf '%-8s %10s  %s\n' "CLASS" "SIZE" "WORKTREE"

STALE_KIB=0
RECLAIMED_KIB=0
SELECTED_REJECTED=false

while IFS= read -r line; do
    case "$line" in
        worktree\ *)
            worktree_path="${line#worktree }"
            worktree_path="$(physical_dir "$worktree_path")"
            target_path="$worktree_path/target"
            selected_for_apply=false
            if [ "$ALL_STALE" = true ] || is_selected "$worktree_path"; then
                selected_for_apply=true
            fi

            if [ ! -e "$target_path" ] && [ ! -L "$target_path" ]; then
                printf '%-8s %10s  %s (%s)\n' "none" "-" "$worktree_path" "no top-level target"
                continue
            fi

            target_kib="$(du -sk "$target_path" 2>/dev/null | awk '{print $1}')"
            target_size="$(human_kib "$target_kib")"
            class="stale"
            reason="clean and merged into $BASE_REF"

            if [ -L "$target_path" ]; then
                class="unsafe"
                reason="target is a symlink"
            elif [ ! -d "$target_path" ]; then
                class="unsafe"
                reason="target is not a directory"
            elif [ "$(physical_dir "$target_path/..")" != "$worktree_path" ]; then
                class="unsafe"
                reason="target parent does not resolve to worktree root"
            elif [ "$worktree_path" = "$REPO_ROOT" ]; then
                class="active"
                reason="invoking worktree"
            elif [ -n "$(git -C "$worktree_path" status --porcelain --untracked-files=normal)" ]; then
                class="active"
                reason="worktree has source changes"
            elif ! git -C "$worktree_path" merge-base --is-ancestor HEAD "$BASE_COMMIT"; then
                class="active"
                reason="HEAD is not merged into $BASE_REF"
            fi

            printf '%-8s %10s  %s (%s)\n' "$class" "$target_size" "$worktree_path" "$reason"

            if [ "$class" = "stale" ]; then
                STALE_KIB=$((STALE_KIB + target_kib))
                if [ "$MODE" = "apply" ] && [ "$selected_for_apply" = true ]; then
                    rm -rf "$target_path"
                    if [ -e "$target_path" ] || [ -L "$target_path" ]; then
                        fail "target still exists after removal: $target_path"
                    fi
                    RECLAIMED_KIB=$((RECLAIMED_KIB + target_kib))
                    printf 'removed  %10s  %s\n' "$target_size" "$target_path"
                fi
            elif [ "$MODE" = "apply" ] && is_selected "$worktree_path"; then
                SELECTED_REJECTED=true
                printf 'preserved selected target: %s (%s)\n' "$target_path" "$reason" >&2
            fi
            ;;
    esac
done < <(git worktree list --porcelain)

printf 'Eligible stale sum: %s (per-target; shared hard links may overlap)\n' "$(human_kib "$STALE_KIB")"
printf 'Removed target sum: %s (verify unique space with df)\n' "$(human_kib "$RECLAIMED_KIB")"
printf 'Shared Cargo registry/git caches: preserved (outside repository scope)\n'

if [ "$SELECTED_REJECTED" = true ]; then
    exit 2
fi
