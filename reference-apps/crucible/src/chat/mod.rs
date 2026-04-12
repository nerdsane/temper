//! Crucible Phase 4 — agent chat loop.
//!
//! This module contains all the glue that turns Crucible's Phase 0–3
//! control-plane (Environment / ManagedAgent / Session / SessionEvent)
//! into a working agent that responds to user messages by calling
//! Anthropic's public Messages API.
//!
//! The loop is deliberately small:
//!
//! 1. [`temper_client`] — thin `reqwest`-based OData client scoped to
//!    the handful of Crucible endpoints we need.
//! 2. [`anthropic`] — Anthropic Messages-API client + the neutral
//!    `Model` trait + a deterministic mock provider gated on
//!    `CRUCIBLE_RESPONDER_MODE=mock` + provider selection
//!    (`model_from_env`).
//! 3. [`openai`] — OpenAI Chat Completions client that implements the
//!    same `Model` trait; selected via
//!    `CRUCIBLE_RESPONDER_PROVIDER=openai` + `OPENAI_API_KEY`.
//! 4. [`responder`] — the turn loop itself: read history, call the
//!    model, write the reply as SessionEvents, drive lifecycle.
//! 5. [`seed`] — fixture helper that POSTs an Environment, a
//!    ManagedAgent, and a Session so the binary has something to talk
//!    to.
//!
//! Nothing in this module touches `temper-server`, `temper-spec`, or
//! any other simulation-visible crate. It is a pure HTTP client.

pub mod anthropic;
pub mod openai;
pub mod responder;
pub mod seed;
pub mod temper_client;
pub mod tools;
