# ARN-453 — Spec

What we are addressing: CI and hooks carried duplicated, dead, and inaccurate
checks that cost time and trust without adding signal.

Expected end state:
1. No pre-push hook. Its four gates (fmt, clippy, readability ratchet, full
   tests) run in CI; the hook duplicated 20+ min per push and flaked (ARN-440).
   Pre-commit (staged-file placeholder/unwrap/spec-syntax/dep checks) stays.
2. No nightly cron: main pushes already run DST full mode.
3. No badges workflow. It was failing (expired gist token) and produced gist
   badges the README never showed; the README's own Rust/version badges were
   hardcoded. Replaced by shields.io dynamic-TOML badges that read Cargo.toml
   from main — accurate by construction, no workflow, no secrets.
4. No verification-contract job: it ran the hook installer on the CI runner and
   validated a report about hook installation — it tested the installer, not
   temper. The scripts remain for local harness use, minus pre-push checks.
5. Bench compile weekly + on demand (was every main push, ~16 min).
6. The four per-PR SDLC gates are one workflow file with four jobs; the
   branch-protection contexts (planning, decision-log, proof, review) are the
   job names and are unchanged. Canonical in stack; vendored to temper now;
   other repos at their next sync. decision-intake stays separate
   (issue_comment trigger). Scripts that re-run gates by workflow name are
   updated to the combined workflow.
7. Dead config removed (the disabled DST pattern-scan comment).

Kept, with reason: instrumentation-hygiene (ADR-0052 enforcement — every
registered metric must have an emission site), integrity, verify-specs, tests,
the DST matrix. release.yml (cargo-dist, never run) left as-is: product call.
