//! Reference agent-runtime control plane (Crucible).
//!
//! Demonstrates cross-field validation via `[[field_invariant]]` and
//! parent-field `[[cross_invariant]]` lookups by re-implementing the
//! Environment slice of Anthropic's Claude Managed Agents API on top
//! of Temper. See `specs/` for the I/O Automaton specs and CSDL model,
//! and `tests/` for DST, verification cascade, and HTTP validation tests.
//!
//! Phase 4 (ADR-0046) adds the [`chat`] module: a small, purpose-built
//! agent loop that reads Session history from the SessionEvent feed,
//! calls Anthropic's public Messages API, and writes the reply back as
//! a new batch of events plus the matching `StartSession`/`IdleSession`
//! bound-action calls. The binary `crucible-chat` wires the module into
//! a `clap`-driven CLI.
//!
//! See `docs/adrs/0041-ioa-field-invariants.md`,
//! `docs/adrs/0042-crucible-reference-app.md`, and
//! `docs/adrs/0046-crucible-agent-loop-phase-4.md` for the design decisions.

pub mod chat;
