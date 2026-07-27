//! Extension point for platform-level custom effects.
//!
//! Platform entities (temper-system) use `effect = "trigger Foo"` in their
//! IOA specs. The dispatch pipeline calls this handler for each custom
//! effect after WASM/adapter integration dispatch.
//!
//! This trait lives in `temper-server` so that `temper-platform` can implement
//! it without introducing a circular dependency (platform depends on server,
//! not the reverse).

/// Handler for custom effects triggered by entity state machine transitions.
///
/// Implementations registered on [`ServerState::custom_effect_handler`] are
/// called by the post-dispatch effects pipeline for every custom effect name.
#[async_trait::async_trait]
pub trait CustomEffectHandler: Send + Sync {
    /// Handle a custom effect.
    ///
    /// - `effect_name`: the effect trigger name (e.g. "GenerateCedarPolicy")
    /// - `entity_type`: the entity type that transitioned
    /// - `entity_id`: the entity instance ID
    /// - `entity_fields`: merged entity state fields (includes all params from all prior actions)
    /// - `server`: server state for dispatch, authz, etc.
    async fn handle(
        &self,
        effect_name: &str,
        entity_type: &str,
        entity_id: &str,
        entity_fields: &serde_json::Value,
        server: &super::ServerState,
    ) -> Result<(), String>;
}
