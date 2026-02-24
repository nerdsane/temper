#!/bin/bash
# Item 15: Git Hook Installer
# Installs pre-commit, pre-push, and post-commit hooks into .git/hooks/
# Idempotent — safe to run multiple times.
set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOOKS_DIR="$WORKSPACE_ROOT/.git/hooks"
SOURCE_DIR="$WORKSPACE_ROOT/.claude/hooks"

echo "=== Installing Git Hooks ==="

# Ensure .git/hooks exists
mkdir -p "$HOOKS_DIR"

# Install pre-commit hook
if [ -f "$HOOKS_DIR/pre-commit" ] && ! grep -q "temper harness" "$HOOKS_DIR/pre-commit" 2>/dev/null; then
    echo "WARNING: Existing pre-commit hook found. Backing up to pre-commit.backup"
    cp "$HOOKS_DIR/pre-commit" "$HOOKS_DIR/pre-commit.backup"
fi

cat > "$HOOKS_DIR/pre-commit" << 'HOOK_EOF'
#!/bin/bash
# temper harness — pre-commit hook (Items 7, 8, 9)
# Installed by scripts/setup-hooks.sh
WORKSPACE_ROOT="$(git rev-parse --show-toplevel)"
exec "$WORKSPACE_ROOT/.claude/hooks/pre-commit.sh"
HOOK_EOF
chmod +x "$HOOKS_DIR/pre-commit"
echo "Installed: pre-commit (integrity check, spec syntax, dep audit)"

# Install pre-push hook
if [ -f "$HOOKS_DIR/pre-push" ] && ! grep -q "temper harness" "$HOOKS_DIR/pre-push" 2>/dev/null; then
    echo "WARNING: Existing pre-push hook found. Backing up to pre-push.backup"
    cp "$HOOKS_DIR/pre-push" "$HOOKS_DIR/pre-push.backup"
fi

cat > "$HOOKS_DIR/pre-push" << 'HOOK_EOF'
#!/bin/bash
# temper harness — pre-push hook (Item 10)
# Installed by scripts/setup-hooks.sh
WORKSPACE_ROOT="$(git rev-parse --show-toplevel)"
exec "$WORKSPACE_ROOT/.claude/hooks/pre-push.sh"
HOOK_EOF
chmod +x "$HOOKS_DIR/pre-push"
echo "Installed: pre-push (integrity/readability + fmt/check/clippy/tests)"

# Install post-commit hook
if [ -f "$HOOKS_DIR/post-commit" ] && ! grep -q "temper harness" "$HOOKS_DIR/post-commit" 2>/dev/null; then
    echo "WARNING: Existing post-commit hook found. Backing up to post-commit.backup"
    cp "$HOOKS_DIR/post-commit" "$HOOKS_DIR/post-commit.backup"
fi

cat > "$HOOKS_DIR/post-commit" << 'HOOK_EOF'
#!/bin/bash
# temper harness — post-commit hook (marker wiring)
# Installed by scripts/setup-hooks.sh
WORKSPACE_ROOT="$(git rev-parse --show-toplevel)"
exec "$WORKSPACE_ROOT/.claude/hooks/post-commit.sh"
HOOK_EOF
chmod +x "$HOOKS_DIR/post-commit"
echo "Installed: post-commit (commit-pending/sim-changed markers)"

echo ""
echo "=== Git hooks installed ==="
echo "Pre-commit: integrity check, spec syntax validation, dependency audit"
echo "Pre-push: integrity + readability + fmt + check + clippy + cargo test --workspace"
echo "Post-commit: commit lifecycle markers for stop gate"
echo ""
echo "Bypass for emergencies: git commit --no-verify / git push --no-verify"
