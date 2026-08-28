# Spec: gates on temper

The five workflows from the stack (planning, decisions, verification, review,
decision-intake), byte-identical to temperpaw's post-pilot state. Logic lives
in arni-labs/stack and is cloned at gate run time; the workflows are thin
callers. The review gate validates records (the implementer runs the panel);
proof scope-skips docs/workflow-only PRs; branch protection (set after merge)
requires planning, decision-log, proof, review plus the kernel's existing
required checks.
