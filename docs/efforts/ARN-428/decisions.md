## Decisions & Tradeoffs

**Decision:** vendor byte-identical workflows rather than adapt them per repo.
**Came up because:** temper has heavy kernel CI already.
**Options:** repo-specific gate variants; identical vendoring.
**Chose** identical **because** the gates' logic lives in the stack anyway - a
repo-specific fork would drift for no gain; kernel CI remains untouched
alongside. Gave up nothing.
**Where:** .github/workflows/sdlc-*.yml (this PR).
