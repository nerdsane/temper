# Implementation plan

1. Add a reproducing bounded-workflow test and seeded budget scenarios using production context code. Preserve the existing inline-cycle regression.
2. Separate total continuation hops from inline depth. Clear only inline depth at the existing detached WASM, adapter, and compensation task boundaries. Carry both counters through internal context inheritance.
3. Verify finite completion, inline and background cycle bounds, and authorization/tenant invariants. Run formatting, Clippy, relevant callback suites, and the model checker.
4. Review the complete kernel diff using the fixed panel; resolve confirmed findings. Publish the one kernel PR for ARN-518 and merge through the required gates.
5. Pin the resulting kernel in TemperPaw PR #526. Rebuild and demonstrate research, predictions, learning, subsequent predictions, and visible activity on the actual artifact. Complete the application review and dedicated Foresight deployment, including affected Genesis dependencies and live verification.
