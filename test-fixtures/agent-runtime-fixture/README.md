# Fixture repo for Agent Runtime PoC

A deliberately broken Python project used as the target for agent runs.

## What's wrong

`src/calculator.py` has a bug in the `divide` method: it returns `a * b`
instead of `a / b`. The test in `tests/test_calculator.py` fails.

## Expected agent behavior

1. Agent provisions a sandbox and clones this repo.
2. Agent runs the test, sees it fail.
3. Agent reads `src/calculator.py`, finds the bug.
4. Agent edits the file to fix the division.
5. Agent re-runs the test, sees it pass.
6. Agent returns the final `git diff`.
