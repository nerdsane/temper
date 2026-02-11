# WS4: Claude Code Skill

## Status: COMPLETE

## Phases
- [x] 4.1 Create `.claude/commands/temper.md` skill file
  - Interview Protocol (8-step structured interview)
  - IOA TOML Spec Format (actual format from codebase)
  - Spec Patterns Catalog (6 patterns: Lifecycle, Approval, Issue Tracker, Support Ticket, Payment, Subscription)
  - Error Translation Table (7 verification failure types with explanations)
  - Progressive Disclosure (6 levels from minimal to multi-entity)
  - CLI Reference (init, verify, codegen, serve with curl examples)
  - CSDL + Cedar templates
  - Anti-Patterns table

## Key Decisions
- Used actual IOA TOML format from codebase (flat `[automaton]`, `[[action]]`, not nested)
- Documented `/tdata` endpoint (not `/odata`)
- Guard/effect strings match parser expectations
- All patterns verified against temper-spec parser structure
