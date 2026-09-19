# Shared review contract

Review the accepted outcome and the supplied change. Read this file plus the target repository's root `REVIEW.md`, if present. The repository file adds domain-specific checks; it does not create another workflow. The user's objective, accepted decisions, code and diff are evidence.

## What to check

- Does the changed behavior deliver the accepted outcome?
- Does the diff introduce or worsen a concrete defect: wrong results, broken flows, data loss, unauthorized access, secret exposure or a relevant regression?
- Is new complexity actually needed for that outcome? Identify a concrete unnecessary addition rather than requesting a redesign for style.
- For a feature, read the code **and independently exercise the changed behavior** when practical. Use the repository's existing test and control tools. Report the revision, inputs, observed results and any limits. Code inspection alone is not an independently tested feature.

Read surrounding code for context, not to audit the whole repository. Check whether a suspected failure also exists on the base revision. Omit unrelated improvements, pre-existing defects, speculative hardening and preferences already settled by the user. Reviewer agreement does not expand scope.

## What to return

Use short, readable prose. For each finding give the relevant `file:line`, the triggering input or action, expected versus observed behavior, and why the change causes it. Distinguish a demonstrated defect from an untested concern. If there are no findings, say so; there is no quota.

List what you actually tested and what remains unverified. Do not implement fixes, start another effort, spawn reviewers or decide merge authorization. After a fix, recheck the affected behavior and new changes; do not repeat a full review of already accepted code.

Reviews are advice to the implementing agent and user. There is no rubric score, mandatory panel, model membership, JSON schema or review approval gate. The implementer validates findings, fixes relevant defects and explains dismissals briefly. Existing user authorization determines delivery.
