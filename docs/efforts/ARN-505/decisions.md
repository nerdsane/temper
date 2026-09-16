# Decisions and tradeoffs

Carried from ARN-499 (D11, D16, D19, D24, D25 there), then superseded by D6 below.

## D1: Install an approved policy into the authorization engine when it activates

**Decision:** Add a `PolicyActivated` consumer that reloads the tenant's Cedar
policy set at the moment a Policy entity reaches `Active`, before the row is
persisted as activated.

**Came up because.** The event existed and nothing listened to it. A policy could
be materialized by an install, transitioned to `Active`, and reported as applied,
while the authorization engine went on evaluating the policy set it loaded at
startup. The effect was ARN-164: every newly installed app's collections answered
403 until a human made a manual policy API call. The install said it succeeded;
nothing about the decision changed.

**Options.** Reload the whole tenant policy set on every authorization check
(correct, and pays the cost on the hot path); poll for changed Policy rows;
consume the activation event that is already published.

**Chose the event consumer because.** The publisher was already there — the gap
was a missing subscriber, not missing machinery. Reloading per check would put a
database read in front of every Cedar decision. Polling would reintroduce the
delay the event exists to remove.

**Ordering matters here.** The reload happens *before* `persist_and_activate_policy`,
not after. If the reload fails, the row is not marked active — so the state that
claims a policy is in force cannot outrun the engine that enforces it. This is
the ARN-497 class: a governed action that reports success without having had an
effect.

**Where.** `crates/temper-platform/src/policy_activation.rs` (new), wired in
`crates/temper-cli/src/serve/mod.rs` beside `spawn_reconciler` (temper `0d63e12e`).

## D2: The policy consumer is a reconciler, not an activate hook

**Decision:** Re-read the `Policy` row on every change and make the engine match
it — install when `Active`, remove otherwise — plus a sweep on startup and after
a broadcast lag, and a rollback if the durable write fails.

**Came up because:** The panel found three holes in the activate-only version:
revoking a policy left its statement in force; a lagged broadcast dropped an
approved policy with only a warning; and rows already `Active` before the task
subscribed were never installed at all. All three are the ARN-494 failure —
state and enforcement disagreeing — pointing in different directions.

**Options:** Patch the three cases individually onto the activate hook; rebuild
the tenant's policy set from scratch on every change; make the consumer a
reconciler that applies one row at a time and sweeps when it may have missed
something.

**Chose the reconciler because:** The three bugs are one bug — the code assumed
an event stream is a state description. Rebuilding from scratch each time would
discard policy text that has no `Policy` row behind it (the bootstrap permits),
so there is no complete source to rebuild from. Applying a row and sweeping on
doubt converges without needing one.

**What it does not cover.** The sweep can only visit tenants whose policy text
this process already tracks, because the kernel has no tenant enumeration. A
tenant outside that set still converges through its own events; the sweep adds
recovery, not discovery. Recorded rather than hidden.

**On the durable write.** `persist_and_activate_policy` returns `bool` for three
different situations — no store configured, nothing changed, and a real failure
— so it cannot be checked by the caller. The reconciler writes through the store
directly, and on a write error reloads the previous text, rather than leaving a
running process enforcing a rule no restart would reproduce.

**Where.** `crates/temper-platform/src/policy_activation.rs`, with four tests for
the merge/remove inverse.

## D3: A revoked policy is disabled durably, and a shared statement is not pulled out from under another row

**Decision:** On revoke, disable the durable policy row rather than saving the
statement; and before removing a statement from the live text, check that no
other `Active` Policy carries the same statement.

**Came up because:** Round two of the panel read the reconciler I had just
written. Removal reloaded the engine and then called `save_policy` with the
revoked statement, so the durable row stayed enabled and the next boot loaded
the revoked policy straight back in — the fix had a restart-shaped hole in it.
Separately, two rows may carry identical text, and revoking one removed the
text for both.

**Options:** Delete the durable row; disable it; leave it and filter revoked
rows at load. For the shared statement: refcount statements; compare text
across Active rows at removal time; accept the collision as unlikely.

**Chose disable because** it keeps the audit trail that a policy once existed
and was withdrawn, which deletion destroys, while `load_policies_for_tenant`
already honours the enabled flag. **Chose the comparison because** a refcount is
state that can drift from the rows it counts, and the rows are the truth; the
comparison is a scan of one entity type that happens only on revoke.

**Where.** `crates/temper-platform/src/policy_activation.rs`
(`another_active_policy_owns`, `disable_durable_record`).

## D4: Ownership of a policy statement is the durable row, never text containment

**Decision:** A revoked Policy removes its statement from the live text only if
it has an enabled durable row of its own AND no other enabled row carries the
same text. Otherwise the row is disabled and the engine is left alone.

**Came up because:** Greptile and codex, separately. The live policy text is a
merge of several sources — bootstrap permits, the legacy blob, other Policy rows
— and the previous guard only compared against other *Policy entities*, then
removed by unrestricted string replacement. Revoking a Policy whose text
happened to match an existing `forbid` would have deleted that restriction from
the running engine. And a row that never reached `Active` was treated as owning
text it had never contributed.

**Options:** Track ownership in a new side table; give each statement an
identifying comment marker; use the durable policy row that already exists.

**Chose the durable row because** it is already the record of what this entity
installed — `save_policy` writes it keyed by entity id — so ownership needs no
new state that could drift from the thing it describes. A marker comment would
put bookkeeping inside Cedar source that humans read.

**Where.** `crates/temper-platform/src/policy_activation.rs`
(`this_row_owns_the_statement`).

## D5: Recovery enumerates tenants from the store, not from the state it is recovering

**Decision:** The startup and lag sweep takes its tenant list from the durable
policy store, unioned with the tracked in-memory map.

**Came up because:** Greptile pointed out the circularity: the sweep discovered
tenants through `tenant_policies`, which is exactly the in-memory state the
sweep exists to rebuild. A Policy that reached `Active` durably before this
consumer wrote anything leaves no trace there, so after a restart its tenant was
never visited and its approved policy stayed uninstalled — the precise gap the
sweep was added to close.

**Options:** Add a tenant registry to the kernel; enumerate tenants from the
event store; read them from the policy store rows.

**Chose the policy store because** it is the durable record of the very thing
being recovered, so any tenant with a policy to restore is in it by
construction. A kernel-wide tenant registry is the better primitive and a much
larger change.

**Where.** `crates/temper-platform/src/policy_activation.rs`
(`reconcile_tracked_tenants`).


## D6: Replace the reconciler with a hook that uses the kernel's own primitive

**Decision:** Delete the event-subscriber reconciler and its sweeps, ownership
lookups and text merge/remove; install and revoke a policy by writing or
disabling its durable row and calling the recompose the kernel already has,
from inside the action's dispatch.

**Came up because:** Review rounds 3–5 on ARN-499 put nine findings on the
reconciler and three on everything else. Each fix added machinery, and the
machinery kept having edges: ownership inferred from text, a sweep that
enumerated tenants from the state it was rebuilding, a load-before-persist
contract a detached subscriber could not keep. Rita asked for the
over-engineering cleaned up and an elegant solution built with what was known.

**Options:** Keep patching the reconciler; rebuild the tenant's policy text from
scratch on every change; use the durable rows and the existing recompose.

**Chose the rows and the hook because** the kernel already treats the rows as the
source of truth and already recomposes from them at boot and after API writes —
the reconciler had re-implemented that with string surgery. Running in the
dispatch removes the event, the lag and the sweep, and lets the caller see a
failure. Nine of the ten open findings stop existing rather than getting fixed.

**What it costs:** a `Policy.Activate` whose install fails returns an error after
the state machine has already moved to `Active`; the caller sees it, but the row
is not rolled back. That is a bounded, visible failure instead of a silent one.
And the DST scenario is still owed — now for ~90 lines instead of ~330.

**Where.** `crates/temper-platform/src/policy_activation.rs`,
`crates/temper-platform/src/state.rs`.
