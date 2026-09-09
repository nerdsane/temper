#!/usr/bin/env bash
# Bring a cloud agent's environment up to laptop parity: clone the stack and
# install its skills + global instructions the same way the laptop's sync.sh does.
# Cursor/Codex/Claude cloud all run this from their environment setup step.
# The repo's own AGENTS.md already carries the vendored global block, so this is
# only needed for the shared SKILLS (interrogate, verify, deterministic-simulation, etc.).
set -euo pipefail
STACK_DIR="${STACK_DIR:-$HOME/stack}"
if [ ! -d "$STACK_DIR/.git" ]; then
  git clone --depth 1 https://github.com/arni-labs/stack "$STACK_DIR"
else
  git -C "$STACK_DIR" pull --ff-only || true
fi
# install skills into every harness root this environment has
bash "$STACK_DIR/sync.sh" || echo "sync.sh reported issues (non-fatal for skills that linked)"
echo "cloud-setup: stack skills installed from $STACK_DIR"
