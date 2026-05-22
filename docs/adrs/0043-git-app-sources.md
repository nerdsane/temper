# ADR-0043: Git-Based App Sources

## Status

Superseded by Genesis-only install.

The implementation described here has been removed. Runtime apps are no
longer loaded from external git source lists, local symlink farms, or
app-as-skill aliases. Genesis is the registry source of truth: bootstrap
the minimal Genesis app, then install normal Temper apps by pinned
Genesis ref through spec-owned `App.Install`.

## Date

2026-04-14

## Deciders

Temper core maintainers

## Related

- `crates/temper-platform/src/os_apps/mod.rs` (app catalog, discovery, install)
- `TEMPER_OS_APPS_DIR` env var (existing app directory config)

## Context

Temper's OS app catalog discovers apps exclusively from local filesystem
directories. `AppCatalog::discover()` reads `TEMPER_OS_APPS_DIR` (or falls back
to compile-time/CWD-relative paths), and hosts can register additional
directories via `set_os_apps_dir()` / `add_os_apps_dir()`.

This works when all apps live in the same repo or are co-located on disk (via
symlinks). It breaks in two real scenarios:

1. **Multi-repo projects**: Palimpsest (`arni-labs/palimpsest`) defines Temper
   apps (`knowledge-bank`, `voice-bank`, `content-forge`) in its own repo,
   separate from OpenPaw's `os-apps/`. Local development uses symlinks, but
   symlinks are dead links in Docker images and CI environments.

2. **Deployment**: The Dockerfile does `COPY os-apps ./os-apps`, which only
   copies the host repo's apps. External repos are not available. Git submodules
   would work but add maintenance burden and version-pinning complexity.

The gap: there is no way for a Temper deployment to declaratively specify
"also load apps from these git repositories" without filesystem-level hacks.

## Decision

### Sub-Decision 1: external git source environment variable

This sub-decision was not kept. Temper no longer accepts a runtime git
source list for app installation. App bytes must be published into Genesis
objects and installed by pinned ref.

### Sub-Decision 2: Git via `std::process::Command`

Use `std::process::Command::new("git")` for clone and fetch operations. No new
crate dependencies (no `git2`, `gix`, or similar).

Operations:
- **First clone**: `git clone --depth 1 --branch {ref} {url} {cache_dir}/{name}`
- **Update**: `git fetch --depth 1 origin {ref}` + `git checkout FETCH_HEAD`

Shallow clone (`--depth 1`) keeps it fast — only the current tree is needed.

**Why this approach**: `git` CLI is universally available (already in dev
environments, trivially added to Docker images). Adding `git2` or `gix` would
pull ~30 crates for a feature that runs once at startup. The CLI approach is
simpler, has zero dependency footprint, and the output/error handling is
straightforward.

### Sub-Decision 3: Cache Directory Owned by Host

The host (e.g., OpenPaw) provides the cache directory path. The
`sync_and_register_git_sources(cache_dir: &Path)` function takes it as a
parameter rather than choosing a location itself.

**Why this approach**: `temper-platform` is a library crate — it shouldn't own
filesystem paths or data directories. The host knows where its data lives
(OpenPaw uses `~/.local/share/openpaw/`). This keeps the separation clean.

### Sub-Decision 4: Non-Fatal on Failure

If git clone/fetch fails (network error, bad URL, missing ref), the server logs
a warning and continues booting with whatever local apps are available. Git app
sources are additive — they never replace or override apps already in the
catalog (`add_os_apps_dir` uses `or_insert`, not `insert`).

**Why this approach**: Startup must be resilient. A temporary network issue
shouldn't prevent the server from running with its local apps.

### Sub-Decision 5: Repo Scanning Convention

After cloning, the repo root is passed to `add_os_apps_dir()`. This means
the repo is scanned for subdirectories containing `app.toml` + `APP.md` — the
same convention as any other apps directory.

## Consequences

### Positive

- Any Temper deployment can load apps from external git repos via one env var
- No symlinks needed — works in Docker, CI, and dev identically
- Zero new Rust dependencies
- Existing `add_os_apps_dir` deduplication ensures no conflicts with local apps

### Negative

- Requires `git` binary available at runtime
- First boot with new sources has network latency (clone)

### Risks

- **Git not installed**: logged as warning, server continues without those apps.
- **Stale cache**: fetch + checkout on every startup keeps it current.

### DST Compliance

Not applicable. Changes are in `temper-platform` only (not simulation-visible).

## Non-Goals

- Private repo authentication UI (standard git credential helpers suffice)
- Build-time Docker optimization (future Dockerfile stage)
- Hot-reload from git (startup only)
- Version pinning / lock files (`@ref` is sufficient)

## Alternatives Considered

1. **Git submodules** — Rejected: couples repo versions, maintenance burden.
2. **`git2` / `gix` crate** — Rejected: ~30 deps for one startup call.
3. **HTTP-based app registry** — Rejected: requires registry infrastructure.

## Rollback Policy

Remove the `git_sources` module. Purely additive — no existing behavior modified.
