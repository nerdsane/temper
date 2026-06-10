//! Per-backend trait implementations, split out of `storage/mod.rs` for
//! navigability. Traits and types implemented here live in the parent module.
//!
//! This module is a re-export hub: it gathers the parent module's items plus
//! the external store crates and forwards them to the per-backend submodules,
//! which each pull the shared surface in via `use super::*`. The blanket
//! `allow(unused_imports)` covers re-exports a given submodule doesn't consume.
#![allow(unused_imports)]

pub(super) use std::collections::BTreeMap;
pub(super) use std::sync::Arc;
pub(super) use std::time::Duration;

pub(super) use sqlx::PgPool;
pub(super) use temper_runtime::persistence::{EventStore, PersistenceEnvelope, PersistenceError};
pub(super) use temper_store_postgres::*;
pub(super) use temper_store_turso::{PolicyRow as TursoPolicyRow, store::TrajectoryStats, *};

pub(super) use super::*;
pub(super) use crate::platform_store::PlatformStore;
#[cfg(feature = "sim")]
pub(super) use crate::platform_store::SimPlatformStore;
pub(super) use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};

mod convert;
mod lifecycle_stores;
mod metadata_stores;
mod query_trajectory;
