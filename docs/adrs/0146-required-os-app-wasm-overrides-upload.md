# ADR-0146: Required OS-App WASM Overrides Upload Drift

## Status

Accepted

## Context

OS app reconcile preserves `source = "upload"` WASM rows so operators can hotfix
modules without having the next app reconcile immediately clobber them. ADR-0145
added durable drift recovery, but production exposed a remaining failure mode:
an old image can re-upload stale bytes for an app-required module after the app
metadata already records the current bundle digest. On the next boot the stale
upload is newer than the app record, so preservation keeps the bad module even
though the app bundle contains the required fixed module.

For required modules, entity transitions and integrations rely on the bundled
module matching the installed app contract. Preserving a mismatched upload for
those modules can silently route state transitions through old logic.

## Decision

When an OS app bundle contains a WASM module declared `app-required` or
`platform-required`, reconcile replaces any differing `source = "upload"` row
with the bundled module, regardless of upload timestamp.

Optional modules keep the previous hot-upload preservation behavior: if the app
bundle digest is unchanged and the upload is newer than the app record, reconcile
preserves the upload.

## Consequences

- Required app/platform modules are owned by the installed bundle and cannot be
  shadowed indefinitely by stale uploads.
- Optional modules still support deliberate long-lived hot uploads.
- Emergency hotfixes to required modules remain possible, but they are temporary:
  the durable fix must land in the app bundle before the next reconcile/deploy.
- Reconcile evidence becomes easier to reason about because required module
  drift always converges to the bundle.

## Verification

- Added a regression test where app metadata already matches the bundle, then a
  newer stale upload shadows an `app-required` module. Reconcile now restores the
  bundled hash and source.
- Retained a preservation test for optional modules with newer uploads.
