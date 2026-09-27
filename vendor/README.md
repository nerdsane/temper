# Vendored dependencies

## Native-build polling (`cc`)

`cc` 1.4.5 retains its published source and packaging except for the Linux-host
executor's bounded idle-polling interval and focused regression tests. It keeps
the original Cargo jobserver and compiler behavior. See
[`cc/README.temper.md`](cc/README.temper.md) for provenance, the precise change,
and verification/update instructions.

## Rust-only Turso SDK package copies

These copies import the exact crates.io 0.7.2 source releases of
`turso_sdk_kit` and `turso_sync_sdk_kit`. Temper consumes their Rust libraries,
not their standalone C-ABI dynamic or static libraries. All Rust sources,
generated bindings, headers, build scripts, documentation, feature definitions,
and dependency declarations are retained unchanged.

The active normalized `Cargo.toml` files have only these packaging changes:

- Both `[lib].crate-type` lists contain only `lib`, instead of also producing
  `cdylib` and `staticlib`. This avoids unused native artifacts and permits
  Rust metadata pipelining.
- `turso_sdk_kit` sets `package.build = false`. Its upstream script only
  produces Windows version resources for the now-unbuilt DLL; it generates no
  Rust code, bindings, or configuration. The script is retained unchanged.
  The sync SDK already disables its build script.

These packaging changes apply on every platform because Cargo has no
target-specific library crate-type setting. They do not remove the C API source,
Rust API, encryption/FTS support, IOCP/io_uring code, or debug assertions.
These copies are not intended for distributing standalone SDK DLLs or
static libraries.

`Cargo.toml.orig` is retained unchanged for provenance. Cargo uses the normalized,
self-contained `Cargo.toml`, not the workspace-inheriting original.
The root workspace patches these packages and excludes their directories from
membership, so vendoring does not add third-party tests to `--workspace`.
Unused nested lockfiles and registry bookkeeping files are omitted.

## Provenance and license

Both published packages record upstream Git commit
[`046e9cbf67d22491e8ecc941ec2891b02a9f3cad`](https://github.com/tursodatabase/turso/tree/046e9cbf67d22491e8ecc941ec2891b02a9f3cad)
in their `.cargo_vcs_info.json` metadata:

| Published package | Upstream directory | Published .crate SHA-256 |
| --- | --- | --- |
| turso_sdk_kit 0.7.2 | sdk-kit | 18c1dc1c0304348c39b97bc6b27cdcb1d7292454ebd0de0f30b5ee3a4c61f9bb |
| turso_sync_sdk_kit 0.7.2 | sync/sdk-kit | d48cb47d056c3ec567745761fa6f832358ca30ef9eb0435fdcfb77c558e70089 |

Each directory includes the [upstream MIT license](https://github.com/tursodatabase/turso/blob/046e9cbf67d22491e8ecc941ec2891b02a9f3cad/LICENSE.md),
copyright 2024 the Turso authors.

## Updating

Revisit these copies when upgrading Turso. Import the exact published packages,
verify provenance and source hashes, reapply only the packaging changes above,
and preserve all dependency features. Check the resolved lockfile and package
metadata: a future semver-compatible registry release can supersede a local patch
unless resolution continues selecting this version. Recheck the Windows build
script before disabling it for another release.

Evaluate performance with a clean full workspace command and the unchanged SQL
workload. Standalone native SDK packaging is outside this patch.

## Unmodified Rust source hashes

These SHA-256 hashes match the published package files byte-for-byte:

| File | SHA-256 |
| --- | --- |
| turso-sdk-kit/src/bindings.rs | 5540469ad1d62aeb1ef454576de343dc998dc7fcc15952e4a28b8c05553dd1e0 |
| turso-sdk-kit/src/capi.rs | b2027193cd3e22125293fe88ba1d154ac02e1a28ab660afd7b35160cb493ed06 |
| turso-sdk-kit/src/lib.rs | 8659233733cf284cef13a95b5278f92bcd31a41d7156b1e74391387fdef6049b |
| turso-sdk-kit/src/rsapi.rs | 7c986b54b1b44e4b4762290c1ea2b39a274183f46c1cb4421e4df7d1a66fa003 |
| turso-sync-sdk-kit/src/bindings.rs | 211d64ebc6486faac38f6388578592f3fb70d4802867c892dc192a9be7a371a0 |
| turso-sync-sdk-kit/src/capi.rs | 34e79cf17cdabfb5addb1902630c15066624c2699f04be490f7d0f8604fc2388 |
| turso-sync-sdk-kit/src/lib.rs | 9da9435339b8e6560cb944a5a71bc9b4c90673a4d08630581efa3d33c2b963c9 |
| turso-sync-sdk-kit/src/rsapi.rs | e5d653669391777e0923d7d4a222ba2b5292f5d52b4de880699b4b55b3140b14 |
| turso-sync-sdk-kit/src/sync_engine_io.rs | af38057f715cfd4c422bfd7d95679f3353d2f5f2ead142d60a89e60106c9fb99 |
| turso-sync-sdk-kit/src/turso_async_operation.rs | 76d12c1f848494e327dfce7ac3661bfc481960ce93ddcceeab8d1cb3ca8ef294 |
