# PawFS Context Read Plane Live E2E

Date: 2026-04-24
Worktree: `/Users/seshendranalla/Development/temper-worktrees/pawfs-context-read-plane`
Validated through local `temperpaw-server` using this Temper worktree via a temporary local dependency patch in the paired OpenPaw worktree.

## Goal

Prove the Temper-side architecture works in a real server process:

- OS-app reactions load and execute after install
- `FileVersion` lineage is explicit at runtime
- `POST /api/files/read-version-text-batch` serves immutable historical content
- the internal blob endpoint fast path works for local server verification

## Live proof

On the local server:

1. Created a `File`
2. Wrote two distinct text payloads through `PUT /tdata/Files('<id>')/$value`
3. Queried the resulting `File` and `FileVersion` entities
4. Called `POST /api/files/read-version-text-batch` with both the superseded and current version ids

Observed runtime facts:

- `File.fields.last_version_id` updated to the newest `FileVersion`
- newest `FileVersion.status=Current`
- previous `FileVersion.status=Superseded`
- batch immutable read returned:
  - `first version from live e2e`
  - `second version from live e2e`

## Runtime bugs found and fixed during live verification

### 1. Reactions were not actually live after app install

Symptom:

- specs defined the supersede reaction
- but the previous version stayed `Current` at runtime after later app installs

Root cause:

- OS-app bundles were not loading `reactions/reactions.toml`
- bootstrap registration dropped reaction rules
- later app installs overwrote existing tenant reactions

Fix:

- load reactions into `AppBundle`
- register them during bootstrap
- rebuild the live `ReactionDispatcher` after install
- merge tenant reactions instead of replacing them wholesale

### 2. Immutable batch reads used the wrong blob path locally

Symptom:

- `POST /api/files/read-version-text-batch` returned `401 Unauthorized` on the local server even though the content existed

Root cause:

- batch version reads followed the configured blob endpoint even when it pointed at the server's own internal `/_internal/blobs` route

Fix:

- detect the internal local blob endpoint
- read directly from the local store instead of doing an external HTTP GET

## What this proves

- Temper now has a real native immutable read plane for TemperFS hot paths.
- `FileVersion` lineage is not just modeled in specs; it executes correctly in the running platform.
- app-installed reactions are active immediately after install.
- live immutable batch reads work on the same stack OpenPaw uses for Session context assembly.
