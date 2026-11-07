# pprof-rs 0.14.1, vendored for Temper

The published crate, patched in two lines (the workspace `Cargo.toml` points
`[patch.crates-io]` here):

1. `src/collector.rs`: `BUCKETS` is `1 << 8` (upstream `1 << 12`).
2. `src/lib.rs`: `#![allow(warnings, clippy::all)]`, because a path dependency's
   lints are not capped the way a registry crate's are.

Why: the collector's hash table is allocated whole, `BUCKETS` x 4 slots of a
128-frame stack, and `Profiler::stop()` allocates a fresh one and keeps it.
Upstream's size held 77.5 MB in every Temper from its first capture on
(measured from the heap profile, 7 Nov 2026). At 256 buckets the table is about
4.8 MB. Stack depth is unchanged. A stack that finds its bucket full evicts the
least-counted entry to the collector's spill buffer (256 KiB, then a temporary
file), and the report merges those back, so a smaller table drops no samples
while the temporary directory is writable. It writes to the file more often.

The rest of the crate is unchanged. `benches/`, `examples/` and their manifest
entries were left out.

To move to a new pprof-rs, copy the published crate here and apply the two
lines again.
