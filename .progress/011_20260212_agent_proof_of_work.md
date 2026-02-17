# 011: Agent Proof of Work System

**Created:** 2026-02-12
**Status:** COMPLETE

## Goal
Implement a proof-of-work system that verifies alignment between agent intent (plan), action (trace), and claim (certificate). Plugs into the existing harness via hooks and markers.

## Phases

### Phase 0: Structured Markers ✅
- [x] Create `scripts/pow-write-marker.sh` (shared TOML marker writer)
- [x] Update `dst-reviewer.md` to write TOML markers with evidence
- [x] Update `code-reviewer.md` to write TOML markers with evidence
- [x] Update gates to accept both old and new marker formats
- [x] Update `post-push-verify.sh` to write TOML markers

### Phase 1: Trace Capture ✅
- [x] Create `.claude/hooks/trace-capture.sh` (PostToolUse JSONL logger)
- [x] Create `scripts/pow-verify-trace.sh` (hash chain verification)
- [x] Add trace-capture hook to `.claude/settings.json`

### Phase 2: Claims + Comparison ✅
- [x] Create `scripts/pow-produce-claims.sh` (claims skeleton generator)
- [x] Create `scripts/pow-compare.sh` (trace vs claims comparison engine)
- [x] Add pow-verified check to pre-commit gate
- [x] Add pow-verified check to stop gate

### Phase 3: Alignment Reviewer ✅
- [x] Create `.claude/agents/alignment-reviewer.md`
- [x] Add alignment-reviewed check to pre-commit gate
- [x] Add alignment-reviewed check to stop gate

### Phase 4: Proof Document Generator ✅
- [x] Create `scripts/pow-generate-proof.sh`
- [x] Update `docs/HARNESS.md` with proof system documentation

### Code Review Fixes ✅
- [x] CRITICAL: HARNESS.md duplicate row numbers (renumbered 14,15,16)
- [x] CRITICAL: TOML injection prevention in pow-write-marker.sh (escape_toml)
- [x] IMPORTANT: trace-capture.sh — dropped pipefail (script always exits 0)
- [x] IMPORTANT: post-push-verify.sh — removed dead WORKSPACE_ROOT_ABS variable
- [x] IMPORTANT: pow-produce-claims.sh — renamed tests_pass → tests_ran
- [x] IMPORTANT: pow-compare.sh — added TOML parsing assumption comments
- [x] IMPORTANT: pow-compare.sh — removed 2>/dev/null from comm calls
- [x] IMPORTANT: pow-produce-claims.sh + pow-generate-proof.sh — jq slurp-then-filter
- [x] Code review: PASS

## Verification
- pow-write-marker.sh: tested — writes TOML + plain markers correctly
- pow-verify-trace.sh: tested — verifies hash chain, detects tampering
- Gates: backward-compatible marker_exists() checks both .toml and plain
- Settings.json: trace-capture hook added as first PostToolUse entry
- Code review: PASS — all critical and important issues resolved

## Files

### New (7 files)
| File | Phase |
|------|-------|
| `scripts/pow-write-marker.sh` | 0 |
| `.claude/hooks/trace-capture.sh` | 1 |
| `scripts/pow-verify-trace.sh` | 1 |
| `scripts/pow-produce-claims.sh` | 2 |
| `scripts/pow-compare.sh` | 2 |
| `.claude/agents/alignment-reviewer.md` | 3 |
| `scripts/pow-generate-proof.sh` | 4 |

### Modified (7 files)
| File | Phase |
|------|-------|
| `.claude/agents/dst-reviewer.md` | 0 |
| `.claude/agents/code-reviewer.md` | 0 |
| `.claude/hooks/pre-commit-review-gate.sh` | 0, 2, 3 |
| `.claude/hooks/stop-verify.sh` | 0, 2, 3 |
| `.claude/hooks/post-push-verify.sh` | 0 |
| `.claude/settings.json` | 1 |
| `docs/HARNESS.md` | 4 |
