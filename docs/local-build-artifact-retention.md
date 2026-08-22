# Local build artifact retention

Rust builds are regenerable, but a full Temper workspace `target/` can consume
tens of gigabytes. Separate targets in long-lived Git worktrees multiply that
cost and can exhaust developer or CI disks.

## Safe cleanup command

Run the inventory from any Temper worktree:

```bash
scripts/cleanup-build-artifacts.sh
```

The command is dry-run by default. It reports the size and classification of
each registered worktree's exact top-level `target/`:

- `stale`: the worktree is clean, is not the worktree running the command, and
  its `HEAD` is an ancestor of the selected base (default `main`).
- `active`: the worktree is current, dirty, or unmerged. Its target is kept.
- `unsafe`: the target is a symlink, is not a directory, or does not resolve
  directly beneath the registered worktree. It is never removed.
- `none`: the worktree has no top-level target.

Remove one or more reviewed stale targets by exact worktree path:

```bash
scripts/cleanup-build-artifacts.sh --apply \
  --worktree /absolute/path/to/temper-worktree
```

After reviewing the dry-run, remove every eligible stale target with:

```bash
scripts/cleanup-build-artifacts.sh --apply --all-stale
```

Use `--base <ref>` when the integration branch is not local `main`. The command
refuses unscoped `--apply`, unknown worktrees, and selected active/unsafe paths.
It never scans nested repositories or nested `target/` directories.

Per-target sizes are useful for ranking candidates, but their sum can count
shared hard links more than once. Use `df -h` before and after an applied
cleanup to measure unique filesystem capacity reclaimed.

## Retention rules

- Keep the target for the worktree currently under development. This avoids a
  surprise rebuild while an agent, editor, or test loop is using it.
- Keep dirty and unmerged worktree targets by default. Clean them only after
  the work has been integrated or by using Cargo's own command manually with
  full knowledge of the active task.
- Clean merged worktree targets after their final validation. Remove the
  worktree itself separately only after confirming its branch and source state.
- Preserve `~/.cargo/registry` and `~/.cargo/git`. These are shared dependency
  caches, not per-checkout build products; deleting them increases downloads
  and does not belong in a repository cleanup command.
- Preserve checked-in reports and release artifacts. Retention for durable
  evidence is owned by the producing workflow, not inferred from a filename.
- Inspect nested repositories independently. A nested TemperPaw checkout, for
  example, has its own source, worktrees, and artifact-retention policy.

Run the dry-run weekly on machines with several worktrees, and after merging or
abandoning a build-heavy branch. Compare `df -h` before and after cleanup when
disk pressure is the trigger.

## CI guidance

CI jobs should start from disposable runners, use bounded cache retention, and
key Rust caches by toolchain plus dependency lockfile. Cache Cargo registry/git
data and the workspace target only when it materially improves feedback time;
do not cache ad hoc worktree paths or durable verification reports as build
output. Let the CI provider's retention policy expire superseded cache entries,
and keep the final full validation lane authoritative even when a cache misses.
