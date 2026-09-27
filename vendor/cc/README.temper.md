# Temper's cc build-helper patch

This directory imports the published `cc` 1.4.5 manifest, upstream README,
licenses, and complete `src/` tree. It does not change native compiler flags,
cryptographic source, Cargo's jobserver tokens, or native source selection.

## Provenance

- Crate: <https://crates.io/crates/cc/1.4.5>
- Published archive: `cc-1.4.5.crate`
- Archive SHA-256:
  `005ec2760ca554fae18df7a11195552ec576cd665632a881bc011d5bb2fd4d80`
- Upstream Git revision recorded in the archive's `.cargo_vcs_info.json`:
  `171f8f64698226b88c1971afc50a4b7fcdd3ff74`
- Upstream source:
  <https://github.com/rust-lang/cc-rs/tree/171f8f64698226b88c1971afc50a4b7fcdd3ff74>
- Licenses: unmodified `LICENSE-MIT` and `LICENSE-APACHE`.

## Scheduling-only difference

The upstream parallel executor uses a no-op waker and polls for child exits
and available jobserver tokens. After four no-progress yields it sleeps for
100 ms, increasing linearly to 1,000 ms. Short native compilation jobs can
therefore wait for a polling deadline even after completing.

`src/parallel/async_executor.rs` now delegates the delay calculation to
`backoff_duration`. Linux build hosts use a 1 ms quantum capped at 10 ms;
other hosts retain the upstream 100 ms quantum capped at 1,000 ms. The first
four yields, progress reset, polling order, no-op waker, error propagation,
and cancellation behavior are unchanged. The helper returns zero for the
initial yield counts, but the executor calls it only after those yields.
Every idle iteration still yields or sleeps: this is not a busy-poll loop.

Host scoping is intentional: this helper executes in native build scripts,
so a Linux cross-build uses the Linux polling policy. A build script running
on macOS or Windows retains the original policy regardless of its target.

All other imported files remain byte-identical to the published crate.
`src/parallel/async_executor/tests.rs` is a new regression-test file. It
checks exact host-specific intervals and caps, completion without repolling,
and error propagation with pending-future cancellation from either branch.
It contains no wall-time assertions. No new dependencies are required.

## Verification and updates

Run the helper regression tests with:

```sh
cargo test --manifest-path vendor/cc/Cargo.toml --features parallel --lib parallel::async_executor::tests
```

Also run a multi-file native archive build with Cargo `-j1`, check successful
outputs and compiler-failure propagation, and run the ordinary full cold
workspace test command. Compare native build duration and complete command
time against the same GCC, profile, worker-count, and disk-backed baseline.
A shorter polling interval is a performance hypothesis until measured;
the regression tests do not claim a timing improvement.

To update this vendored dependency:

1. Verify the new published archive checksum and record its upstream revision.
2. Replace the imported manifest, README, licenses, and source from that
   archive. Preserve this note and reapply only the scheduling helper and
   regression tests if upstream has not fixed the polling delay.
3. Review the new executor's waker and jobserver behavior before carrying
   this patch forward. Never bypass token acquisition to obtain parallelism.
4. Update the root dependency patch and lockfile, rerun the regression/native
   checks, and repeat the full cold workspace measurement.
5. Remove the local scheduling patch once an upstream fix is verified and
   the equivalent full-command result is measured.

As inspected on 2026-09-26, upstream tag `cc-v1.5.1` and the main branch still
contain the original 100 ms to 1,000 ms backoff; an update alone does not
currently replace this scheduling change.
