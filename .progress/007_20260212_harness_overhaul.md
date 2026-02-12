# 007 — Harness Overhaul: DST Review, Mandatory Code Review, Enhanced Enforcement

**Created**: 2026-02-12
**Status**: Complete

## Goal
Overhaul the Temper development harness based on conversation findings:
1. Expand determinism guard from 4 to ~25 patterns, make BLOCKING
2. Create DST reviewer agent (.claude/agents/dst-reviewer.md)
3. Add PreToolUse gate on `git commit` — blocks without review markers
4. Add review marker checks to Stop hook as safety net
5. Update CLAUDE.md with mandatory review instructions
6. Rewrite HARNESS.md with Excalidraw-style diagrams

## Phases

### Phase 1: Enhanced Determinism Hook ✅
- Expand check-determinism.sh from 4 to ~25 critical patterns
- Make it BLOCKING (exit 2)
- Keep `// determinism-ok` escape hatch

### Phase 2: DST Reviewer Agent
- Create .claude/agents/dst-reviewer.md
- Embed FoundationDB/TigerBeetle DST ruleset
- Agent reviews simulation-visible code changes semantically

### Phase 3: Pre-Commit Review Gate
- New hook: pre-commit-review-gate.sh
- PreToolUse on Bash — detects `git commit`
- Checks for dst-reviewed + code-reviewed markers
- Also runs cargo test before commit
- BLOCKING (exit 2)

### Phase 4: Stop Hook Update
- Add review marker checks to stop-verify.sh
- Safety net: blocks exit if commits exist without review markers

### Phase 5: Settings + CLAUDE.md Update
- Add new PreToolUse Bash hook to settings.json
- Add mandatory review instructions to CLAUDE.md

### Phase 6: HARNESS.md Rewrite
- Full rewrite with Excalidraw-style ASCII diagrams
- Incorporate all new components
- Clear explanation of each layer
