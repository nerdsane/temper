# WS1: Dev Harness (Deep Sci-Fi Pattern)

## Status: COMPLETE

## Phases
- [x] 1.1 Create `.vision/` directory (PHILOSOPHY, ARCHITECTURE, CONSTRAINTS, VERIFICATION)
- [x] 1.2 Enhance `.claude/settings.json` hooks + hook scripts
- [x] 1.3 Create `scripts/` directory (verify-all, check-dst-coverage, audit-deps)
- [x] 1.4 Enhance `CLAUDE.md` with enforcement sections

## Artifacts
- `.vision/PHILOSOPHY.md` -- 8 core beliefs
- `.vision/ARCHITECTURE.md` -- Crate dependency graph, data flow diagrams
- `.vision/CONSTRAINTS.md` -- Non-negotiable rules
- `.vision/VERIFICATION.md` -- 5-level verification cascade
- `.claude/hooks/check-plan-reminder.sh` -- PreToolUse plan reminder
- `.claude/hooks/post-push-verify.sh` -- PostToolUse test runner after push
- `.claude/hooks/stop-verify.sh` -- Stop hook workspace check
- `.claude/settings.json` -- Updated with all hook types
- `scripts/verify-all.sh` -- Full test suite runner
- `scripts/check-dst-coverage.sh` -- DST coverage checker
- `scripts/audit-deps.sh` -- Dependency isolation verifier
- `CLAUDE.md` -- Added verification checklist, TigerStyle standards, deployment steps
