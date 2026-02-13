# Fix Gap #23: `rule_index` lost on deserialization

## Status: IMPLEMENTATION COMPLETE — running reviews

## Context

`TransitionTable` in temper-jit has a `rule_index: BTreeMap<String, Vec<usize>>` field marked
`#[serde(skip, default)]`. When serialized then deserialized, the index becomes empty.
Since `evaluate_ctx()` does `self.rule_index.get(action)`, a deserialized table returns
`None` for all actions — a correctness bug.

### Phase 1: Fix `TransitionTable` deserialization (COMPLETE)

**File:** `crates/temper-jit/src/table/types.rs`

1. Removed `Deserialize` from `#[derive(...)]` — now `#[derive(Debug, Clone, Serialize)]`
2. Added manual `impl<'de> Deserialize<'de> for TransitionTable`:
   - Uses `TransitionTableRaw` helper struct for the 4 persistent fields
   - Deserializes into raw, constructs table, calls `rebuild_index()`
3. Changed `#[serde(skip, default)]` to `#[serde(skip)]` on `rule_index`

### Phase 2: Add roundtrip test (COMPLETE)

**File:** `crates/temper-jit/src/table/evaluate.rs`

Added `test_serde_roundtrip_preserves_rule_index`:
- Builds table from IOA source
- Serialize → deserialize roundtrip via serde_json
- Asserts `rule_index` is non-empty
- Asserts `evaluate("Draft", 2, "SubmitOrder")` still returns `success: true`

No Cargo.toml changes needed — `serde_json` already in `[dependencies]`.

### Phase 3: Verification (COMPLETE)

- `cargo test -p temper-jit` — 16/16 passed
- `cargo test --workspace` — all passed, zero failures
