# ADR-0163: Inline spec ingestion must not write outside a private temp dir

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-server/src/observe/specs/load_inline.rs` (`POST /api/specs/load-inline`)
  - ARN-229 (security finding)

> This is Fable's competing entry for ARN-229; compared head-to-head by the arena judge.

## Context

`POST /api/specs/load-inline` accepts a JSON body with `specs` — a map of
**filename → content** — and materializes it to disk before delegating to the
load-dir loader (`load_inline.rs`):

```rust
let tmp_dir = std::env::temp_dir().join(format!("temper-inline-{}", tenant));  // predictable, shared
std::fs::remove_dir_all(&tmp_dir);
std::fs::create_dir_all(&tmp_dir)?;
for (filename, content) in &body.specs {
    let path = tmp_dir.join(filename);        // filename is caller-controlled, unvalidated
    std::fs::create_dir_all(path.parent()…)?;
    std::fs::write(&path, content)?;          // arbitrary host write
}
```

Two filesystem defects (ARN-229):

1. **Path traversal → arbitrary host write.** Each `filename` comes straight from
   the request body and is `tmp_dir.join`'d with no validation. A key like
   `../../../../etc/cron.d/x` or an absolute path escapes the temp dir, so an
   authorized spec submitter can write attacker-controlled content anywhere the
   server process can write. `resolve_inline_specs_root` has the same flaw for the
   `model.csdl.xml` parent path.
2. **Predictable shared temp dir → symlink / TOCTOU / cross-tenant.** The dir is a
   fixed `temp_dir()/temper-inline-{tenant}` in world-writable `/tmp`. A local
   attacker who pre-creates it as a symlink redirects every (even validated) write
   out of the intended tree; and `remove_dir_all` on a symlinked path deletes an
   arbitrary directory. The fixed name also invites cross-invocation interference.

## Decision

### Sub-Decision 1: Validate every caller-supplied spec path

`safe_inline_spec_path(tmp_dir, filename)` requires `filename` to be a non-empty
relative path whose every component is a `Component::Normal` (no `..`, no absolute
/ root / prefix, no `.`), then joins it under the temp dir. Applied to every
`specs` key and the cross-invariants path before any `create_dir_all` / `write`,
so a traversal or absolute name is rejected with `400` before touching the
filesystem. This mirrors the `safe_bundle_relative_path` posture used elsewhere.

The **`tenant`** value is also caller-supplied and is interpolated into the temp
dir leaf name, and `TenantId::new` permits `/` and `..`. The leaf must therefore be
a **single** path component (`safe_temp_dir_leaf`, exactly one `Component::Normal`),
not merely traversal-free: a tenant with `/` (e.g. `evil/x`) would make only the
final component uuid-suffixed, leaving a *predictable* intermediate dir
(`/tmp/temper-inline-evil`) a local attacker could pre-plant a symlink at — the
very vector the unpredictable name is meant to close. A tenant with `..` or an
absolute segment is likewise rejected. "Every caller-supplied path" includes the
tenant, and for the temp-dir leaf the bar is a single unpredictable component.

### Sub-Decision 2: Private, unpredictable temp dir

The per-request dir gets an unguessable suffix
(`temper-inline-{tenant}-{uuidv7}`), created fresh and removed after loading, so a
pre-planted symlink can't be targeted and no state is shared across requests or
tenants.

## Consequences

### Positive
- A malicious/oversized `specs` map can no longer escape the temp dir to write or
  delete arbitrary host files, and the symlink/TOCTOU vector on the fixed path is
  closed. Legitimate inline submissions (relative spec paths under a model root)
  are unaffected.

### DST Compliance
- `temper-server` is simulation-visible, but this is an HTTP handler that runs
  outside the deterministic simulation core (existing `// determinism-ok`
  annotations on its `std::fs` / `temp_dir` calls). The new validation is pure; the
  unpredictable suffix uses `uuid::Uuid::now_v7()` annotated `// determinism-ok`,
  consistent with the handler's existing exemptions.

## Non-Goals / Follow-ups
- **Symlink-safe creation primitives** (`O_NOFOLLOW` / `openat` on each component)
  would harden the write step itself against a symlink planted *between* dir
  creation and write. The unpredictable-name defense closes the disclosed vector;
  fully atomic symlink-safe I/O is a broader change tracked as a follow-up.

## Alternatives Considered
1. **Reject only `..` textually.** Rejected: misses absolute paths and reserved
   components; component-based validation via `Path::components` is exhaustive.
2. **Keep the fixed dir, only validate names.** Rejected: leaves the symlink/TOCTOU
   vector, which validated relative writes still follow.
