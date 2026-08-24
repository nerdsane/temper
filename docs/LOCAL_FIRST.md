# Local-first Temper

Local operation is the default development path. Genesis remains the optional
publication and collaboration layer; an OCI host such as Railway remains the
optional always-on deployment layer.

## Start

```console
$ temper up
```

`temper up` binds to `127.0.0.1:3000`, creates an embedded libSQL database and
blob/cache directories under `~/.local/share/temper`, generates an owner-only
operator credential, and starts the HTTP/OData server, embedded Observe, and
stateless HTTP MCP. Use `--data-dir` or `--port` to isolate an instance.

The printed Observe bootstrap URL is single-use. It exchanges its nonce for an
HttpOnly, SameSite=Strict browser cookie. The durable operator credential is
never placed in the URL, page, browser storage, or logs.

## Create and develop an app

```console
$ temper init my-app
$ cd my-app
$ temper dev
```

The scaffold contains `app.toml`, `APP.md`, `temper.lock.toml`, an IOA behavior
specification, and a CSDL data contract. `temper dev` snapshots the workspace,
runs the verification cascade, and installs the successful revision by content
digest. An invalid edit is reported while the last valid revision remains
active.

For an explicit lifecycle, run `temper up` in one terminal and:

```console
$ temper app install ./my-app
```

The installed version is copied into a content-addressed cache. Later workspace
edits cannot change that installed version. Restarting the daemon validates and
restores the exact bundle referenced by durable provenance.

## Local dependencies

Dependencies never resolve through ambient filesystem search. Declare every
local source explicitly and commit the resolved lock:

```console
$ temper app lock --local shared=../shared
$ temper app install --locked .
```

`--locked` rejects a missing lock, an unresolved dependency digest, or content
that no longer matches the recorded digest.

## Cache maintenance

```console
$ temper app cache gc --dry-run
$ temper app cache gc
```

Collection treats durable installation records as roots. It retains every
referenced manifest and blob, including bundles installed for tenants other
than the credential tenant.

## MCP

The local daemon accepts `POST /mcp` using protocol revision `2026-07-28`.
Requests require the local bearer credential plus matching
`MCP-Protocol-Version`, `Mcp-Method`, and, for tool calls, `Mcp-Name` headers.
The transport is stateless, validates browser origins, creates a fresh sandbox
per request, and disables host-path operations. `temper mcp` remains available
for clients that require the older stdio transport.

## Publish or host later

Local bundles use the same immutable manifest, resource budgets, safe-path
rules, dependency semantics, and governed installation boundary as remote
bundles. Publishing to Genesis adds discovery, collaboration, and cross-machine
distribution; deploying Temper to an OCI host adds remote, always-on access.
Neither is required for local development.
