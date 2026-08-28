# ARN-428: roll the SDLC gates to temper

## Problem
The enforced SDLC gates run only on temperpaw. Temper merges are gated by its
kernel CI but not by the loop's planning/decisions/proof/review contract.

## Proposed outcome
The five gate workflows vendored here, wired to the stack's gate logic (cloned
at run time), with branch protection requiring them after merge. Same contract
as temperpaw, proven there first.

## Affected users and systems
.github/workflows only. Kernel CI unchanged. Secrets: STACK_TOKEN (present).

## Constraints
Kernel CI (DST, spec verification) stays exactly as is - the gates add the
loop's contract, they replace nothing.

## Open questions
None - this is a vendoring of a proven pilot.
