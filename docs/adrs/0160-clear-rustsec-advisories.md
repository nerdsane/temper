# ADR-0160: Clear reachable RUSTSEC advisories in transitive deps (ARN-169)

- Status: Accepted
- Date: 2026-08-15
- Issue: ARN-169 (follow-up to the wasmtime bump in ADR-0159)

## Context

`cargo audit` on `main` reported 18 advisories. ADR-0159 closed the critical
wasmtime one. This pass clears the remaining advisories that are reachable without
a cascading parent upgrade. Cleared here (crate → advisories):

- `postgres-protocol` 0.6.11 → 0.6.12 — RUSTSEC-2026-0179, -0180
- `tokio-postgres` 0.7.17 → 0.7.18 — RUSTSEC-2026-0178 (DoS panic)
- `quinn-proto` 0.11.14 → 0.11.16 — RUSTSEC-2026-0185
- `quick-xml` 0.37.5 → 0.41 — RUSTSEC-2026-0194, -0195 (DoS)
- `rustls-webpki` (0.103 line) 0.103.9 → 0.103.13 — RUSTSEC-2026-0049, -0098, -0099, -0104
- `crossbeam-epoch` 0.9.18 → 0.9.20 — RUSTSEC-2026-0204

## Decision

All but one are plain `cargo update` (same major, no source change). The one
source change is `quick-xml` 0.37 → 0.41, which is pinned by our own `temper-spec`
(0.41.0 is the *minimum* version patching RUSTSEC-2026-0194/-0195, so the API break
was unavoidable — no smaller bump exists):

- 0.41's `Reader::read_text` returns a `Result<BytesText>` instead of an owned
  `String`. The Collection<String> annotation path (`csdl/parser/elements.rs`)
  now calls `.decode()` on it. **Behavior is preserved exactly**: 0.37's
  `read_text` also only charset-decoded and did *not* unescape XML entities (its
  rustdoc: "does not unescape read data"), so `A &amp; B` stays literal under
  both versions, consistent with the inline-attribute path (`attr_str`). A
  regression test (`test_parse_collection_annotation_preserved_across_quick_xml_bump`)
  locks the preserved behavior in.
- All other `temper-spec` quick-xml uses (`read_event_into`, `element.name()`,
  raw attribute bytes) are version-stable.

Not changed here (deliberately, to keep this a behavior-neutral dep bump): the CSDL
parser does not unescape XML entities in annotation text — neither before nor after
this bump — and the parse→emit path double-escapes such values. Adding proper
entity handling is a separate, pre-existing correctness fix that needs its own
sign-off, out of scope for an advisory-clearance bump.

One behavior difference exists, and only for **ill-formed** XML: 0.41 is stricter
about a raw, un-escaped `&` in element text (its text scan is `memchr2(b'<', b'&')`
where 0.37's was `memchr(b'<')`), so such an annotation item is dropped where 0.37
kept it verbatim. Well-formed CSDL is unaffected (our emitter always escapes, and no
fixture contains a raw `&`), and the surrounding document still parses. Stricter
rejection of invalid XML is the desirable direction.

That the item is *silently* dropped rather than raising `CsdlParseError` is a
pre-existing shape of this code that the bump makes reachable; it is tracked as
**ARN-347** rather than fixed here, so this stays a dependency bump.

## Consequences

- `cargo audit` drops from 18 to 7 (11 advisory instances cleared). temper-spec's
  suite passes and the workspace builds clean; a server built on the bumped stack
  boots and parses seeded CSDL through the new quick-xml path without error.
- Who is actually affected, so a future reader audits the right crate:
  - `postgres-protocol` / `tokio-postgres` reach us through `deadpool-postgres`
    (`temper-actor-runtime`, `temper-cli`, `temper-agents`) — **not**
    `temper-store-postgres`, which uses `sqlx`. 0.6.12 rejects SCRAM iteration
    counts above 100_000 (Postgres defaults to 4096) and turns hstore / short
    `DataRow` panics into errors.
  - the `rustls-webpki` 0.103 line serves `reqwest` / `bollard` / `testcontainers` /
    `quinn`. It is **not** the Turso TLS path — that still resolves 0.102.8 via
    `libsql`, which is why the residual below is a real cascade rather than an
    oversight.
  - `quick-xml` affects only the CSDL parser in `temper-spec`.
- The lockfile also moves crates beyond the six above, which is expected resolver
  dedup rather than part of the security change: `hyper-util` / `quinn` /
  `quinn-udp` re-point from `socket2` 0.6.3 to 0.5.10, and several dependents from
  `windows-sys` 0.61.2 down to already-present 0.60/0.59/0.52/0.48 lines. Both
  `socket2` lines already coexisted on `main`, every requirement is a range
  (`>=0.5.9, <0.7`), no advisory covers the selected versions, and builds, clippy
  and tests are clean. Called out explicitly because a networking-crate *downgrade*
  inside a security PR is exactly the kind of thing a later audit should not have to
  re-derive.

## Residual (out of scope here, tracked on ARN-169)

Remaining `cargo audit` findings, each needing a major/cascading upgrade or having
no fixed release:

- `rustls-webpki` 0.102.8 (RUSTSEC-2026-0049/-0098/-0099/-0104) — pulled by
  `libsql` 0.9.x → `hyper-rustls` 0.25 → `rustls` 0.22. Clearing it needs bumping
  `libsql` to a release on `rustls` 0.23 — a cascade into the Turso storage client
  with real breakage risk.
- `protobuf` 2.28.0 (RUSTSEC-2024-0437) — fix is `>=3.7.2`, a major bump; a
  separate, larger migration.
- `rsa` 0.9.10 (RUSTSEC-2023-0071, Marvin timing) and `tokio-tar` 0.3.1
  (RUSTSEC-2025-0111) — no fixed release available; accept/monitor.
- **temperpaw** carries the same dependency set and needs the equivalent pass in
  its own repo.

Consider adding `cargo audit` to CI so new advisories surface automatically.
