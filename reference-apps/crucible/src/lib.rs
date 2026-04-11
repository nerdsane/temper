//! Reference agent-runtime control plane (Crucible).
//!
//! Demonstrates cross-field validation via `[[field_invariant]]` and
//! parent-field `[[cross_invariant]]` lookups by re-implementing the
//! Environment slice of Anthropic's Claude Managed Agents API on top
//! of Temper. See `specs/` for the I/O Automaton specs and CSDL model,
//! and `tests/` for DST, verification cascade, and HTTP validation tests.
//!
//! See `docs/adrs/0041-ioa-field-invariants.md` and
//! `docs/adrs/0042-crucible-reference-app.md` for the design decisions.
