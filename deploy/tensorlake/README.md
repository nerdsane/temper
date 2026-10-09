# Temper on Tensorlake

One named sandbox runs Temper, a standalone Observe server, and an nginx password guard for Observe. One Cloud Volume holds the embedded database, blobs, and Temper data directory. The runtime image contains no credentials. HTTP traffic wakes the named sandbox; the idle timeout is 600 seconds. No external database, scheduler, or warm pool is required.

## Supported behavior

- One writable sandbox per volume. This is an operational constraint, not a filesystem lock: Tensorlake concurrent mounts use last-writer-wins file reconciliation.
- HTTP requests and immediate actions. Delayed actions, schedule/schedule_at effects, state-timeout deadlines, and persistent channel transports have no supported delivery guarantee on this deployment. Existing kernel syntax is unchanged; callers must not depend on timer-driven progress. Background work may pause while suspended.
- Active Observe tabs keep the sandbox awake. Hidden tabs close event streams and skip polling.
- Process-crash retry deduplication and automatic failover are deferred. Do not automatically replay an ambiguous mutation.
- Cloud Volume autosave is asynchronous. A locally acknowledged database commit can precede remote durability. Recovery reaches the last durable autosave; recent changes may be lost on host loss. Live SQLite/WAL mount compatibility must be tested for the deployed runtime.

## Build

Build the selected source revision using the pinned Rust toolchain and a compatible Z3 library. The image is Linux x86-64 Ubuntu 24.04; the binary and libz3 must be compatible. For example:

```sh
cargo build --locked --profile dist -p temper-cli
cd ui/observe
npm ci
npm test -- --run __tests__/lib/event-source.test.ts
TEMPER_API_URL=http://127.0.0.1:3000 npm run build
cd ../..
python3 deploy/tensorlake/prepare.py --binary target/dist/temper --z3-lib /path/to/z3/lib --node /path/to/node --output /tmp/temper-image-context
tl sbx image create /tmp/temper-image-context/Dockerfile --registered-name temper-REVISION
```

`prepare.py` records the source commit and binary SHA-256 in `/opt/temper/build.json`. Keep image names immutable. The production TypeScript config checks application code independently of test fixtures.

## First deployment

```sh
python3 deploy/tensorlake/deploy.py --name temper-runtime --image temper-REVISION --credentials-dir /private/path/temper-runtime
```

The script creates a new volume and sandbox and refuses to reuse an existing volume. It grants the authenticated `operator` identity read access in the `default` tenant for Observe; the kernel separately seeds policy-management permission. Application mutations require explicit Cedar policies and are not enabled by this script. It writes private local credential files before launching the runtime. Preserve `runtime.env`: its stable vault key is needed after replacement. `credentials.json` contains the API key and Observe login. Never commit or put these files in the image context. If provisioning fails, inspect the recorded resources before retrying; the script intentionally does not delete or adopt resources automatically.

Verify `/healthz`, authenticated API access, and unauthenticated rejection from inside the sandbox before exposing ports:

```sh
tl sbx port expose temper-runtime 3000 8080
```

Port 3000 is the Temper API and requires its bearer credential on protected routes. Port 8080 is Observe, protected by HTTP Basic authentication at nginx. Use the assigned regional HTTPS hostname returned while the sandbox is running, with the port prefix. Record it before suspension. In this deployment, the generic `sandbox.tensorlake.ai` URL returned while suspended failed with a proxy 502; the recorded regional endpoint successfully woke the sandbox. Recheck routing after replacement or a regional move. The internal Node port 3001 must stay unexposed: Observe injects the operator API key after the nginx login guard. Keep the Tensorlake management API private.

## Upgrade or replace

Use a maintenance window; never create a second writable runtime while the old one could resume.

1. Remove exposed ports to stop admission, then stop the managed `temper` process and wait for all child processes to exit. Confirm the database is closed and volume autosave has completed; retain a permanent snapshot.
2. Terminate the old sandbox and verify it is terminated. Suspension is insufficient because it can auto-resume. Never restart this old generation after its volume has a new writer.
3. Create a new sandbox from the new immutable image, mounting the existing volume at `/var/lib/temper`. Restore the same runtime secrets, then launch `/opt/temper/start.sh` as a managed process.
4. Verify readiness, credentials, stored entities, and blobs; expose ports and update the client URL if necessary.

If the old sandbox's state is uncertain, stop and resolve it before replacement. Do not use live VM cloning as a replica/failover mechanism. A database migration may prevent rollback to an older binary; retain the pre-upgrade snapshot and restore it to a separate recovery volume if needed.

## Validation

The deployment smoke check must create actual Temper data and blobs, restart the process, suspend/resume, and read the same data back. A volume compatibility probe should commit SQLite WAL data, crash without closing the database, and recover it on a new sandbox with `PRAGMA integrity_check`. These checks are evidence for the exercised cases, not a provider guarantee for all host failures.

### Exercised deployment (2026-09-27)

- Sandbox `temper`, image `temper-2a63e19b`, 2 CPUs / 4 GiB, idle timeout 600 seconds; Cloud Volume `temper-data` mounted at `/var/lib/temper`.
- API health and authorized entity reads passed over HTTPS; unauthenticated API requests returned 401. Observe required Basic authentication and successfully proxied authenticated data requests.
- An actual Plan entity and blob survived a managed-process restart and suspension. The temporary policy allowed writes only to the two smoke resources and was disabled afterward.
- HTTP alone woke the suspended sandbox through its assigned regional endpoint; the first health request completed in approximately 0.75 seconds. The full ten-minute idle interval was not separately timed.
- A separate Cloud Volume probe recovered 1,000 committed SQLite WAL rows and a blob after process crash, suspend/resume, and replacement-sandbox attachment; integrity checks passed. This does not establish zero-loss host-failure durability.
- UI stream lifecycle tests (2), application TypeScript checks, production Next.js build, shell/Python syntax checks, and the Rust release build passed.
