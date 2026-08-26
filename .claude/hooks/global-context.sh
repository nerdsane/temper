#!/usr/bin/env bash
# Injects the global instruction layer ONLY where the user-level file is absent
# (cloud VMs). Locally ~/.claude/CLAUDE.md exists, so this emits nothing and
# the global layer loads once, from home. No duplication either way.
if [ ! -f "$HOME/.claude/CLAUDE.md" ]; then
  cat "$(dirname "$0")/../global.md"
fi
