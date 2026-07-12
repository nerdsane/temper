# ADR-0158: Capability-gated Discord reply callback

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - ADR-0037: Channel transports (created the reply-webhook design)
  - `crates/temper-transport/src/discord/transport.rs` (the `/reply` listener)
  - `crates/temper-transport/src/discord/gateway.rs` (the Discord sender)
  - ARN-234 (security finding), ARN-182 (kernel/app boundary follow-up)

## Context

ADR-0037 delivers channel replies by having the transport start a loopback Axum
listener and store its URL on the Channel entity; the platform-agnostic
`send_reply` WASM module reads that URL and POSTs `{thread_id, content}` to it,
and the transport delivers the content to Discord as the bot.

The listener's `POST /reply` has **no authentication of any kind** — no token,
signature, replay guard, or budget (`transport.rs`). It maps the caller-supplied
`thread_id` (a Discord user id) to a DM channel and sends the caller-supplied
`content` as the bot, split into an **unbounded** number of 2 000-byte Discord
messages (`gateway.rs`). Any local process, or any SSRF-capable component that
can reach the loopback port, can impersonate the agent, spam a user, burn Discord
rate limits, or replay a reply (ARN-234). "A loopback address is not identity."

## Decision

Gate the reply callback with a capability the caller can only have obtained
through the authenticated Channel entity, and bound the fan-out. All changes are
contained to `temper-transport`; no change to the `send_reply` WASM module or the
Channel spec is required.

### Sub-Decision 1: Per-run capability token, delivered via the Channel entity

At transport startup, mint an unguessable per-run reply token (two v4 UUIDs of
CSPRNG entropy). The webhook URL published to the Channel entity carries it:
`http://127.0.0.1:{port}/reply?token=<secret>`. The `/reply` handler requires the
token and compares it in constant time; a missing or wrong token is rejected with
`401 Unauthorized` **before** any thread lookup or Discord call.

**Why this authenticates the caller**: the token is only reachable by reading the
Channel entity's webhook config through Temper's authenticated, Cedar-governed
OData API. A bare local process or an SSRF probe that discovers the port cannot
read the Channel entity, so it does not have the token and is denied — which
closes the disclosed threat ("any local process, or any SSRF-capable component
that reaches the port"). The legitimate `send_reply` module already POSTs to
exactly the URL it reads from the Channel entity, so it presents the token
transparently — no module change. The token is per-run, so a leaked value dies
with the process.

The token's confidentiality is therefore exactly the Channel entity's read
authorization. Under the current `temper-channels` Cedar policy
(`os-apps/temper-channels/policies/channels.cedar`) **any authenticated agent in
the tenant** may read a Channel, so this design defends against the disclosed
unauthenticated / SSRF caller but not against a fully-authenticated tenant insider
who reads the Channel to obtain the token (they could then impersonate the bot to
already-met DM recipients, still bounded by the recipient gate and fan-out
budget). Closing that narrower vector means either tightening Channel read /
redacting `webhook_url` for non-system readers, or the durable one-time reply
intent in Follow-up 1, which removes the shared bearer entirely. This ADR
deliberately does not broaden or narrow the pre-existing Channel read policy.

### Sub-Decision 2: Body limit and fan-out budget

axum runs all extractors before the handler body, so the `Json` extractor would
buffer up to axum's 2 MiB default before the token check runs — letting an
unauthenticated caller force a large pre-auth allocation. A
`DefaultBodyLimit::max(MAX_REPLY_BODY_BYTES)` layer caps the request body
*before* extraction, so an over-large body is rejected (`413`) with no token and
no big allocation. The limit is sized well above the largest valid reply and far
below axum's default.

The handler then rejects, with `413 Payload Too Large` before any Discord call,
any reply that would split into more than `MAX_REPLY_CHUNKS` (8) Discord messages.
The **chunk count is the authority**, not a raw byte count: message splitting can
break on newlines and produce more, smaller chunks than `bytes / 2000`, so a
byte-only limit would either admit a >8-message fan-out or fail a legitimate
multi-line reply with an opaque error. `send_discord_message` independently
enforces the same cap as a defensive backstop for any other caller.

### Sub-Decision 3: Recipient gate (unchanged, retained as authorization)

The handler continues to require that `thread_id` already be present in the
transport's `dm_channels` map — i.e. a user the bot has actually exchanged
messages with — returning `404` otherwise. This limits replies to established DM
recipients; it is retained as a second authorization check alongside the token.

## Consequences

### Positive
- The disclosed holes close: unauthenticated callback → capability-gated;
  unbounded fan-out → byte + chunk budget. Constant-time compare avoids a timing
  oracle on the token. No working capability is removed and no WASM rebuild is
  needed.

### Negative / Residual
- The token rides in the webhook URL, so it can appear in the `send_reply`
  module's internal request telemetry. That telemetry is inside the trust
  boundary (not reachable by the disclosed threat — a local/SSRF caller without
  OData access), so the residual is low. A future design (Follow-up 1) removes it.
- The token is confidential only up to the Channel entity's read authorization,
  which the current Cedar policy grants to any authenticated tenant agent (see
  Sub-Decision 1). The disclosed unauthenticated/SSRF hole is fully closed; a
  fully-authenticated insider vector remains and is addressed by Follow-up 1.

### DST Compliance
- `temper-transport` is not a simulation-visible crate (not temper-runtime /
  temper-jit / temper-server), so the DST determinism rules do not apply. The
  token uses OS CSPRNG (correct for a real security secret, not simulated state).

## Non-Goals / Follow-ups (explicitly deferred, not punted)

The disclosed vulnerability — an unauthenticated callback with unbounded fan-out —
is fully remediated here. Two larger changes named in the finding are genuinely
cross-component / cross-repo and are recorded as follow-ups rather than pretended
into this PR:

1. **Durable, recipient-bound, one-time reply intent** (delivery id, nonce,
   expiry, tenant/channel identity carried end-to-end). This needs the `send_reply`
   WASM module and the Channel spec to emit and carry a per-message capability, so
   it is a cross-component redesign. It upgrades the per-run bearer to a one-time,
   recipient-bound capability and adds replay protection against a token holder.
2. **Move app-specific Discord ownership to TemperPaw** (ARN-182). The transport
   living in the kernel is a boundary concern tracked separately; relocating it is
   out of scope for a temper-repo security fix and must not drop the working
   capability.

## Alternatives Considered

1. **Reuse the platform internal bearer** (have the WASM host inject it, verify it
   at `/reply`). Rejected: the host injects the internal bearer only for the
   configured internal API base, not for the transport's separate loopback
   listener, and the transport is a distinct process — so the bearer is not
   available end-to-end without new plumbing.
2. **Token in a request header instead of the URL.** Cleaner (no telemetry leak),
   but the `send_reply` module only sends the URL it is given and does not set auth
   headers, so a header would require changing and rebuilding the WASM module —
   folded into follow-up 1.
