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
- `protobuf` 2.28.0 — RUSTSEC-2024-0437, removed from the tree entirely (see below)

## Decision

Most are plain `cargo update` (same major, no source change). Two entries need
source changes: the `quick-xml` bump below, and the `protobuf` removal described in
Consequences (a `pprof` feature swap plus its one-line serializer call).

The first is `quick-xml` 0.37 → 0.41, which is pinned by our own `temper-spec`
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

- `cargo audit` drops from 18 to 6 (12 advisory instances cleared). temper-spec's
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
- **`protobuf` needed no upgrade at all.** It was in the tree only because
  `temper-server` asked `pprof` for its `protobuf-codec` feature, which pins
  protobuf 2.x — a line with no fix for RUSTSEC-2024-0437. The same `pprof` version
  offers `prost-codec`, so switching the feature (and `Profile::write_to_vec` →
  prost's `Message::encode`) drops the vulnerable crate outright. Both codecs emit
  the same pprof protobuf wire format, so uploaded profiles are unchanged.
- The lockfile also moves crates beyond the upgrades above, which is expected resolver
  dedup rather than part of the security change: `hyper-util` / `quinn` /
  `quinn-udp` re-point from `socket2` 0.6.3 to 0.5.10, and several dependents from
  `windows-sys` 0.61.2 down to already-present 0.60/0.59/0.52/0.48 lines. Both
  `socket2` lines already coexisted on `main`, every requirement is a range that
  admits 0.5.10 (`hyper-util` wants `>=0.5.9, <0.7`; `quinn`/`quinn-udp` the broader
  `>=0.5, <0.7`), no advisory covers the selected versions, and builds, clippy
  and tests are clean. Called out explicitly because a networking-crate *downgrade*
  inside a security PR is exactly the kind of thing a later audit should not have to
  re-derive. The `prost-codec` swap likewise adds five lock entries — `prost-build`
  and `prost-types` 0.12.6, `petgraph` 0.6.5, `fixedbitset` 0.4.2, `multimap`
  0.10.1 — all **build-time only** via pprof's `[build-dependencies]`; `prost`
  itself was already in the tree via `libsql-hrana`, so no new runtime dependency
  is introduced.

## Residual (out of scope here, tracked on ARN-169)

After this pass `cargo audit` reports 6. Each remaining finding needs a cascading
parent upgrade, has no fixed release, or would cost a capability we actually use:

- `rustls-webpki` 0.102.8 (RUSTSEC-2026-0049/-0098/-0099/-0104) — pulled by
  `libsql` 0.9.x → `hyper-rustls` 0.25 → `rustls` 0.22. Two routes exist and both
  are rejected deliberately, not by omission:
  - *Upgrade:* needs `libsql` on `rustls` 0.23 — a cascade into the Turso storage
    client with real breakage risk.
  - *Feature toggle:* `hyper-rustls` is optional in libsql behind its default-on
    `tls` feature, so this is structurally the same shape as the `protobuf-codec`
    swap above. It is rejected because `temper-store-turso` calls
    `Builder::new_remote` (`store/mod.rs`, the `libsql://` branch of the production
    constructor), and with `tls` off libsql's `connector()` is an outright
    `panic!("The \`tls\` feature is disabled, ...")` — so dropping it breaks remote
    Turso at runtime, not just in theory. Removing a working capability to clear an
    advisory needs sign-off, so it stays residual.
- `rsa` 0.9.10 (RUSTSEC-2023-0071, Marvin timing) and `tokio-tar` 0.3.1
  (RUSTSEC-2025-0111) — no fixed release available; accept/monitor.
- **temperpaw** carries the same dependency set and needs the equivalent pass in
  its own repo.

Consider adding `cargo audit` to CI so new advisories surface automatically.
