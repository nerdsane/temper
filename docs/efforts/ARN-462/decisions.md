# ARN-462 — Decision log

Appended at the moment each call is made. Self-contained for a reader with zero session context.

---

### D1 — Skip insert when an enabled row already has the same Cedar text
- **Decision:** `save_policy` refuses a *new* `policy_id` when any enabled row for that tenant already stores the same `cedar_text` hash.
- **Came up because:** Genesis install (`persist_bundle_policy_rows`), recovery, and load-inline-era append paths created a new row for the same permit under a different id (ARN-286/399 class). Hash-gating only compared `(tenant, policy_id)`.
- **Options:** (a) keep inserting and let recovery concatenate duplicates; (b) skip only in `persist_bundle_policy_rows`; (c) skip at `save_policy` so every insert path shares the rule.
- **Chose (c) over (a)/(b) because:** the class is “identical enabled text must not grow the table,” not “this one caller is noisy.” Callers that update an existing `policy_id` still write. A disabled row does not block a new enabled insert. Gave up: two enabled ids cannot carry the same text on purpose (no use case).
- **Where:** `crates/temper-store-turso/src/store/policy.rs`; `crates/temper-store-postgres/src/platform.rs`; tests in `crates/temper-store-turso/src/store/tests/policy_approval.rs`.

### D2 — Do not hydrate File actors to attach an ADR warning
- **Decision:** `build_adr_warning_context` still emits `missing_adrs` from the submitted CSDL/app_name, but it no longer lists Files or `GetState`s each one.
- **Came up because:** `POST /api/specs/load-inline` after a successful merge called `find_existing_adr_paths`, which walked every File id and spawned/replayed the actor. That work is not the submitted specs. On a passivated production tenant it can dominate the request (87s wall / 64s busy).
- **Options:** (a) keep the scan; (b) query-plane prefix lookup; (c) drop the File walk and warn from submitted specs only.
- **Chose (c) over (a)/(b) because:** a trajectory annotation is not worth waking every File. Query-plane would add machinery for a warning. The warning can be noisy when ADRs already exist; that is cheaper than 60s of actor replay. Merge already verifies only submitted IOAs — the File walk was the leftover class of “not the submitted specs.”
- **Where:** `crates/temper-server/src/observe/specs/load_inline/support.rs`.

### D3 — Skip the verification cascade for unchanged already-passed specs
- **Decision:** `load_specs_from_directory` snapshots hash-identical, already-passed specs *before* register (which resets status to Pending) and the stream emits `cached: true` / `reason: unchanged_verified` without L0–L3.
- **Came up because:** load-inline always ran 5 sim seeds × 100 prop tests per submitted entity, including agent re-submits of an unchanged app. Install/bootstrap already hash-gates; load-dir/load-inline did not.
- **Options:** (a) always re-verify (correct but 60s+ on re-push); (b) skip only in merge mode; (c) skip whenever the incoming IOA hash matches a passed/restored registry entry.
- **Chose (c) over (a)/(b) because:** replace-mode `load-dir` of the same directory has the same waste. First submit and content changes still run the cascade. Gave up: a caller who wants a forced re-verify must change a byte.
- **Where:** `crates/temper-server/src/observe/specs_helpers.rs`; `load_dir.rs`; `verification_stream.rs`; `verification_cached.rs`.

### D4 — Recovery concatenates each Cedar text once
- **Decision:** `recover_cedar_policies` tracks seen trimmed texts and does not append a granular (or legacy) copy that is already in the reconstructed blob.
- **Came up because:** production already has duplicate enabled rows; insert-skip alone does not shrink what boot loads. `merge_bundle_policies` already used `contains()`; recovery concatenated every row.
- **Options:** (a) leave recovery concatenating duplicates; (b) dedupe by trimmed text while building the blob.
- **Chose (b) over (a) because:** boot was compiling the same multi-statement policy twice when `primary` and an app row held the same text. Gave up: two rows that differ only by whitespace collapse to one statement (intended).
- **Where:** `crates/temper-platform/src/recovery.rs`.

### D5 — Leave `handle_add_policy_rule` for the other ARN-462 agent
- **Decision:** Do not edit `crates/temper-server/src/api/policies.rs` in the first pass.
- **Came up because:** that handler still appended to the in-memory concat and `persist_and_activate_policy(..., "primary", full_concat)`. The sibling kernel agent owned list/passivate and might take that file.
- **Options:** (a) rewrite add-rule to a granular row here; (b) leave it for the sibling.
- **Chose (b) over (a) because:** coordination rule is do not fight over a file. D1 already stops a *new* id from duplicating enabled text; the concat-as-primary write is a separate shape.
- **Where:** superseded by D6.

### D6 — Add-rule persists `rule:{sha256}`, not a rewritten `primary`
- **Decision:** `POST /policies/rules` writes the new Cedar text under `rule:{sha256(text)}` and leaves `primary` untouched. A second add of the same trimmed text returns the existing row.
- **Came up because:** live `primary` grew to 449k because every approval dumped the tenant concat into that one id. D5 deferred the handler; the test `add_policy_rule_persists_own_row_without_rewriting_primary` now encodes the contract.
- **Options:** (a) keep writing concat to `primary`; (b) new uuid per add; (c) content-addressed `rule:{sha256}`.
- **Chose (c) over (a)/(b) because:** (a) is the production failure. (b) still grows the table on retries. Hash-stable id plus the D1 enabled-text skip means a duplicate POST is a no-op. Gave up: two different ids cannot carry the same rule on purpose.
- **Where:** `crates/temper-server/src/api/policies.rs`; `crates/temper-server/src/api/policies/support.rs` (`add_rule_policy_id`); test in `crates/temper-server/tests/policy_authorization.rs`.

### D7 — Gap-reconcile empty exact-match even when the type is under scan budget
- **Decision:** The ARN-68 field-index coverage-gap reconcile runs for every empty exact-match page, including types smaller than `scan_candidate_budget`. When the probe succeeds, the planner does not pass the full type id list to `read_from_source_cursor`.
- **Came up because:** Production DesignLanguages (1275 ids) was under the 10k–20k default budget, so the over-budget gap gate never ran and the planner hydrated all 1275 and returned 0.
- **Options:** (a) lower `scan_candidate_budget` below typical type sizes; (b) skip ARN-89 when the native page is empty; (c) reuse the existing gap ∪ native-page union for in-budget empty exact-match too.
- **Chose (c) over (a) and (b) because:** the gap is the set of entities that might actually match under projection lag. Hydrating the projected majority cannot change an empty native page. Gained: the 1275/0 shape is impossible when field-index coverage is known. Gave up: a no-field-index backend still falls through to the in-budget source-cursor path rather than inventing a second probe.
- **Where:** `crates/temper-server/src/odata/query_plane_read/mod.rs`; test in `tests/proof.rs`.

### D8 — Passivate oldest-idle first, 32 actors per tick
- **Decision:** `passivate_idle_actors` sorts candidates by `last_accessed` ascending and processes at most `PASSIVATE_IDLE_ACTORS_PER_TICK` (32 actors / tick). Each processed actor still GetState-retries and snapshots before stop (ADR-0048).
- **Came up because:** one tick snapshotted 430 and 735 idle actors sequentially on the request pool (17s / 24.5s, almost all idle_ns).
- **Options:** (a) skip snapshots and only stop (violates ADR-0048); (b) snapshot all idle actors concurrently (still a pool storm); (c) explicit per-tick actor budget, leftover idle until the next tick.
- **Chose (c) over (a) and (b) because:** the pool is the scarce resource. 32 snapshots per minute drains 735 idle actors in about 23 minutes without blocking ordinary reads. Gained: bounded pool occupancy. Gave up: some actors stay warm longer than the idle timeout (next tick picks them up).
- **Where:** `crates/temper-server/src/state/entity_ops.rs`; test in `tests/passivation_respawn.rs`.
