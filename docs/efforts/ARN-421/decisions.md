# Decision log — ARN-421

Repos/worktrees:
- temperpaw: branch `claude/genesis-install-sot` (off origin/main 52a569b4d)
- temper:    branch `claude/genesis-install-verify-rollback` (off origin/main dec295e6)
- temperpaw pins temper by git rev b0c79312 → temper PR merges first, then bump the pin in the
  temperpaw PR (merge order Temper → TemperPaw).

---

## D1 — env bootstrap pin is a floor, not a ceiling (Gap 1, temperpaw) — DONE, approved
- **Decision:** keep any runtime-ready Genesis install regardless of hash; (re)install the env pin
  only when nothing healthy exists.
- **Came up because:** the `Ok(Some(record))` catch-all reinstalled the env pin on ANY hash mismatch,
  reverting a newer agent-published version on redeploy.
- **Options:** (a) keep-any-runtime-ready-genesis [chosen]; (b) true floor via Genesis git ancestry
  [rejected: no monotonic version integer on the record; ancestry query is network + speculative].
- **Chose a over b because:** deterministic, no network, matches the owner decision "any hash → KEEP".
  Gained: redeploy never downgrades. Gave up: an env-pin bump no longer force-upgrades a healthy older
  install — explicit install is the upgrade path (team lead confirmed this consequence is intended).
- **Where:** crates/temperpaw/src/startup.rs — `classify_bootstrap_action` + `BootstrapAction`,
  hash-agnostic probe `genesis_bootstrap_app_runtime_ready`, rewritten match. Unit test
  `classify_bootstrap_keeps_runtime_ready_genesis_install_regardless_of_hash`. Commit 98ec1af55.

## D2 — verify+rollback lives in a post-materialization helper (Gap 2, temper) — approved (Decision A)
- **Decision:** extract verify+rollback into a helper wrapping reconcile+record, pure decision fn
  {Committed | RollBackToPrevious | FailNoRollback}; `install_genesis_app_from_registry` calls it
  after materializing so all three callers inherit it; DST drives the same helper on the local catalog.
- **Came up because:** `install_genesis_app_from_registry` is network/git-backed and not DST-testable;
  the simulatable seam is `reconcile_materialized_app_closure`/`reconcile_os_app`.
- **Options:** (a) extract post-materialization helper [chosen — forced]; (b) inline in the install fn
  [rejected: not DST-testable, violates harness-first].
- **Chose a because:** only shape that is both DST-testable and a single shared path for all callers.
- **Where:** crates/temper-platform/src/genesis_install.rs (impl in progress).

## D3 — verify granularity: B2 (readiness + eager wasm compile) — approved
- **Decision:** verify = `recover_installed_app_runtime_state` == Ready|Healed AND eager-compile every
  app-REQUIRED wasm module (declared in the bundle's `app.toml`) via `WasmEngine::compile_and_cache`.
- **Came up because:** the real prod failure ARN-420 names is "failed to compile lazy-loaded WASM
  module" (and the wasip1 wrong-target class). B1 (readiness only) would let a broken bundle install
  and fail later — a band-aid.
- **Options:** B1 readiness-only [rejected: band-aid]; B2 readiness + compile [chosen].
- **Chose B2 because:** catches the exact prod bug at install so it triggers rollback ("make the wrong
  thing impossible"). **Scoping guard (team lead):** eager-compile ONLY app-required modules, so a
  stray/optional non-required `.wasm` never fails an otherwise-good install. Live/Datadog health stays
  ARN-420's outer layer; the kernel owns readiness + compile.
- **Where:** the verify step of the new helper (genesis_install.rs).

## D4 — rollback mechanics — approved (self-evident, logged)
- Capture prior `InstalledAppRecord` before reconcile. On verify-fail: restore prior provenance record
  + re-reconcile the prior bundle (re-materialize the prior pinned ref if its cache was evicted), then
  re-verify last-good. Fresh install with no prior → fail cleanly, mark `AppInstallation` failed. If
  the prior ref ALSO fails verify → hard both-broken error.

## D5 — follow-latest authority — OUT OF SCOPE, approved
- Bootstrap uses follow_policy "pinned"; the floor fix resolves the redeploy-revert bug without
  touching follow-latest. Making Genesis `LatestVersionHash` auto-authoritative for `follow_latest`
  apps is speculative (no such app is bootstrapped today) → deferred, recorded not dropped. Team lead
  will flag to the owner for possible veto; not blocking.

## D6 — three-review panel run + dispositions
- **Panel:** Fable (fresh subagent, xhigh) and Codex (`gpt-5.6-sol`, xhigh, read-only) ran adversarially on both diffs. **Grok was unavailable** — its CLI 402'd (out of credits) and is mis-provisioned to `z-ai/glm-5.2`, not Grok 4.6 (the off-laptop trio gap, ARN-405). Surfaced to the team lead; not silently skipped.
- **No P0 blockers.** ~13 findings each, strong overlap. Fixed in-scope:
  - Prior-record read swallowed store errors (`.ok().flatten()`) → now propagates; the install aborts before mutating (rollback safety net). [genesis_install.rs]
  - Verification only probed the ROOT app → now verifies EVERY app in the closure (`verify_install_closure_runtime_ready`). [genesis_install_verify.rs]
  - Rollback restored only the root record → now restores ALL captured dependency provenance records. [genesis_install.rs]
  - `FailNoRollback` left the record `status="installed"` for a broken version → now marks it `failed`. [mark_install_failed]
  - Store-less instances were forced to `FailNoRollback` (regression) → a successful reconcile with no store now commits (prior behavior). [genesis_install.rs]
  - Commit ignored provenance-write failures → the ROOT provenance write is now enforced (install fails if it can't persist), deps best-effort. [record_genesis_install_metadata → Result]
  - Rollback wrote the prior record without proving the reconciled bundle matches it → digest-integrity check before restore. [restore_prior_install]
  - `ensure_prior_bundle_available` trusted `is_dir()` → now confirms the cache yields the app at the expected digest, else re-materializes. 
  - Bootstrap read-error fell through to reinstalling the pin (downgrade risk) → now SKIPS on an indeterminate read. [startup.rs]
  - DST P18 was near-tautological → reworked to a faithful, non-tautological invariant (partial local state → restored prior good on Ok; never a bogus Genesis provenance on faulted Err); 64 seeds heavy faults; fails-before-fix confirmed. Coverage boundary documented in the test.
  - temperpaw: WARN when keeping an install whose hash ≠ the env pin (operator visibility for the intended non-upgrade); restored the recovery-outcome diagnostic log.
- **Deferred to ARN-423 (residual risks, tracked not dropped):** additive-reconcile rollback (v2-only artifacts not removed), non-atomic rollback with no durable intent, in-memory-vs-durable verification, forward-path catalog-selection integrity, and temperpaw bootstrap compile-heal (needs the temper-pin bump; `verify_install_runtime_ready` is now `pub` for it). These need a transactional versioned-state layer — the ARN-420 Temper-native deploy future.
- **Greptile:** requested on both PRs.
