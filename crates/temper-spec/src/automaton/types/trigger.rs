//! `[[action.triggers]]`: the one way an action causes work elsewhere
//! (ADR-0046, ADR-0180).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::predicate::{Arg, Expr, Literal};

/// Kind of trigger: what kind of outgoing effect this trigger produces.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TriggerKind {
    /// Cross-entity action dispatch. Target is another entity's action.
    Entity,
    /// WASM module execution. Optionally dispatches `on_success` / `on_failure`
    /// actions on the source entity afterwards.
    Wasm,
    /// Native platform adapter execution. Optionally dispatches `on_success`
    /// / `on_failure` actions on the source entity afterwards.
    Adapter,
    /// Outbound HTTP webhook. Optionally dispatches `on_success` / `on_failure`
    /// actions on the source entity afterwards.
    Webhook,
    /// A platform hook registered by the host (for example `DispatchCallback`).
    Hook,
}

impl TriggerKind {
    /// Spec spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            TriggerKind::Entity => "entity",
            TriggerKind::Wasm => "wasm",
            TriggerKind::Adapter => "adapter",
            TriggerKind::Webhook => "webhook",
            TriggerKind::Hook => "hook",
        }
    }
}

/// Liveness expectation for a trigger (ADR-0046, minimal hook).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerLiveness {
    /// No liveness expectation. Verifier does not emit eventually-properties.
    None,
    /// Best-effort (default). Dispatcher attempts the trigger; liveness is not
    /// asserted by the verifier.
    #[default]
    BestEffort,
    /// Required. The composite verifier emits a `Property::eventually` that
    /// the target action fires following the source action. Assumes
    /// weakly-fair dispatch.
    Required,
}

/// How to resolve the target entity ID for a `kind = "entity"` trigger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetResolver {
    /// Read the target entity ID from a field on the source entity.
    Field {
        /// Field on the source entity holding the target's id.
        id_field: String,
    },
    /// Use the source entity's own ID as the target ID (self-trigger or
    /// same-ID cross-entity dispatch).
    SameId,
    /// Use a fixed entity ID literal.
    Static {
        /// The fixed entity ID to target.
        entity_id: String,
    },
    /// Read target ID from `id_field`; create the target if it does not exist.
    CreateIfMissing {
        /// Field name containing (or to receive) the target ID.
        id_field: String,
    },
    /// Generate a fresh `sim_uuid()` for each trigger dispatch. Correct
    /// resolver for pipeline chaining where each source commit spawns a new
    /// target instance. `sim_uuid()` (not `Uuid::new_v4`) is required for DST
    /// compliance across seeded runs.
    Create,
}

/// An outgoing trigger declared inline on an `Action` (ADR-0046).
///
/// Fields are a superset across kinds; parse-time validation requires each
/// kind's fields and rejects the fields of other kinds:
/// - `Entity`: `target_entity`, `target_action`, `resolve_target`; optionally
///   `principal`, `guard`, `args`, `liveness`, `drop_ok`.
/// - `Wasm`: `module`; optionally `on_success` (one action or a list),
///   `on_failure`, `config`, `llm`.
/// - `Adapter`: `adapter`; optionally `on_success` (one action or a list),
///   `on_failure`, `config`, `llm`.
/// - `Webhook`: `url`, `method`; optionally `headers`, `body_template`,
///   `on_success`, `on_failure`, `config`.
/// - `Hook`: `hook`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionTrigger {
    /// The trigger's identifier: the reaction it makes is named
    /// `{type}:{action}:{name}`, and logs name it. Words for people go in `says`.
    pub name: String,
    /// Words for people reading the app, such as the answer a branch gives
    /// ("CI passed"). Display only: the kernel never reads it, and `name`
    /// stays the trigger's identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub says: Option<String>,
    /// Discriminator — determines which field set applies.
    pub kind: TriggerKind,
    /// Optional principal (names a registered `AgentType`). When `None`, the
    /// trigger inherits the invoking principal's `SecurityContext`. When
    /// `Some`, a synthetic `SecurityContext` is built with every Cedar
    /// attribute (`role`, `agent_type`, `id`) populated from this name.
    /// Parse-time verification fails if the named `AgentType` is not
    /// registered in the tenant.
    #[serde(default)]
    pub principal: Option<String>,
    /// Firing predicate over the source entity's post-action status and
    /// fields. `None` fires on every commit of the source action.
    #[serde(default)]
    pub guard: Option<Expr>,
    /// Liveness expectation (see `TriggerLiveness`).
    #[serde(default)]
    pub liveness: TriggerLiveness,
    /// Marks this reaction as an intentional best-effort drop (ADR-0150).
    ///
    /// When `true`, the composite verifier's `no_dropped_reaction` property
    /// does not flag this trigger as a violation if its target action is not
    /// enabled from the target's current state. Use it for reactions that are
    /// *meant* to be skipped when the target is not ready (e.g. a notification
    /// that is meaningless once an entity is archived). Defaults to `false`:
    /// a dropped reaction is treated as a bug unless explicitly opted out.
    #[serde(default)]
    pub drop_ok: bool,
    /// Marks this trigger as an LLM call for observability. The dispatcher
    /// promotes matching spans to LLM-kind root spans so `gen_ai.*` content
    /// surfaces correctly in observability backends (e.g., Datadog LLM Obs).
    #[serde(default)]
    pub llm: bool,

    // ─── Entity-kind fields ─────────────────────────────────────────────
    /// Target entity type (required for `Entity` kind).
    #[serde(default)]
    pub target_entity: Option<String>,
    /// Target action to dispatch (required for `Entity` kind).
    #[serde(default)]
    pub target_action: Option<String>,
    /// Parameters passed to the target action. Each value is a literal
    /// (`'text'`, an integer, `true`, `false`, `null`) or the name of a field
    /// of the source entity (`Id` is its id), read after the source commits.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, Arg>,
    /// How to find the target entity ID (required for `Entity` kind).
    #[serde(default)]
    pub resolve_target: Option<TargetResolver>,

    // ─── Wasm-kind fields ───────────────────────────────────────────────
    /// WASM module name (required for `Wasm` kind).
    #[serde(default)]
    pub module: Option<String>,
    /// The actions a successful call may take on the source entity, in the
    /// order the kernel tries them: it sends the first that the record takes
    /// with the call's values (its guard may read them as `params.p`).
    /// Written as one name or a list. Empty when the trigger names none.
    #[serde(default, with = "one_or_many")]
    pub on_success: Vec<String>,
    /// Action to dispatch on the source entity on failed execution.
    #[serde(default)]
    pub on_failure: Option<String>,
    /// Arbitrary config passed to the module or adapter at invocation time.
    /// Common keys: `url`, `method`, `headers`, `api_key_ref`.
    #[serde(default)]
    pub config: BTreeMap<String, String>,

    // ─── Adapter-kind fields ───────────────────────────────────────────
    /// Native adapter key (required for `Adapter` kind).
    #[serde(default)]
    pub adapter: Option<String>,

    // ─── Webhook-kind fields ────────────────────────────────────────────
    /// Outbound HTTP URL (required for `Webhook` kind).
    #[serde(default)]
    pub url: Option<String>,
    /// HTTP method (required for `Webhook` kind — typically POST/PUT/PATCH).
    #[serde(default)]
    pub method: Option<String>,
    /// HTTP headers. Values may contain `{secret:key}` templates resolved
    /// from tenant-scoped secret storage.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Template for the HTTP request body. `${field}` placeholders are
    /// resolved from the source entity's post-action fields.
    #[serde(default)]
    pub body_template: Option<String>,

    // ─── Hook-kind fields ───────────────────────────────────────────────
    /// Name of the platform hook to run (required for `Hook` kind).
    #[serde(default)]
    pub hook: Option<String>,
}

impl ActionTrigger {
    /// The source statuses this trigger can fire in, read from its guard's
    /// `status == 'S'` / `status in [...]` conjuncts. `None` when the guard
    /// does not pin `status`.
    pub fn status_filter(&self) -> Option<Vec<String>> {
        let statuses = self.guard.as_ref()?.required_statuses()?;
        Some(statuses.into_iter().map(str::to_string).collect())
    }

    /// Keys this trigger sets that its kind does not read.
    pub(crate) fn foreign_keys(&self) -> Vec<&'static str> {
        use TriggerKind::*;
        let set: [(&'static str, bool, &[TriggerKind]); 17] = [
            ("principal", self.principal.is_some(), &[Entity]),
            ("guard", self.guard.is_some(), &[Entity]),
            (
                "liveness",
                self.liveness != TriggerLiveness::default(),
                &[Entity],
            ),
            ("drop_ok", self.drop_ok, &[Entity]),
            ("llm", self.llm, &[Wasm, Adapter]),
            ("target_entity", self.target_entity.is_some(), &[Entity]),
            ("target_action", self.target_action.is_some(), &[Entity]),
            ("args", !self.args.is_empty(), &[Entity]),
            ("resolve_target", self.resolve_target.is_some(), &[Entity]),
            ("module", self.module.is_some(), &[Wasm]),
            (
                "on_success",
                !self.on_success.is_empty(),
                &[Wasm, Adapter, Webhook],
            ),
            (
                "on_failure",
                self.on_failure.is_some(),
                &[Wasm, Adapter, Webhook],
            ),
            ("config", !self.config.is_empty(), &[Wasm, Adapter, Webhook]),
            ("adapter", self.adapter.is_some(), &[Adapter]),
            ("url", self.url.is_some(), &[Webhook]),
            ("method", self.method.is_some(), &[Webhook]),
            ("hook", self.hook.is_some(), &[Hook]),
        ];
        let mut foreign: Vec<&'static str> = set
            .into_iter()
            .filter(|(_, present, kinds)| *present && !kinds.contains(&self.kind))
            .map(|(key, _, _)| key)
            .collect();
        if self.kind != Webhook {
            if !self.headers.is_empty() {
                foreign.push("headers");
            }
            if self.body_template.is_some() {
                foreign.push("body_template");
            }
        }
        foreign
    }
}

/// `on_success` as written: one action name, or a list of them. None, as
/// before lists, when it names none.
pub(crate) mod one_or_many {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<String>, D::Error> {
        Ok(match Option::<OneOrMany>::deserialize(deserializer)? {
            None => Vec::new(),
            Some(OneOrMany::One(name)) => vec![name],
            Some(OneOrMany::Many(names)) => names,
        })
    }

    pub(crate) fn serialize<S: Serializer>(
        names: &[String],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match names {
            [] => serializer.serialize_none(),
            [name] => serializer.serialize_str(name),
            names => names.serialize(serializer),
        }
    }
}

/// The parameters `args` pass, read against an entity: a literal is its JSON
/// value, `Id` (or `id`) the entity's id, and any other name that field of
/// `fields`. Returns the parameters and the names of fields that were
/// absent, whose keys are left out.
pub fn resolve_args<'a>(
    args: &'a BTreeMap<String, Arg>,
    entity_id: &str,
    fields: &serde_json::Value,
) -> (serde_json::Map<String, serde_json::Value>, Vec<&'a str>) {
    let mut params = serde_json::Map::new();
    let mut missing = Vec::new();
    for (key, arg) in args {
        let value = match arg {
            Arg::Lit(Literal::Int(n)) => serde_json::Value::from(*n),
            Arg::Lit(Literal::Str(s)) => serde_json::Value::String(s.clone()),
            Arg::Lit(Literal::Bool(b)) => serde_json::Value::Bool(*b),
            Arg::Lit(Literal::Null) => serde_json::Value::Null,
            Arg::Var(name) if matches!(name.as_str(), "Id" | "id") => {
                serde_json::Value::String(entity_id.to_string())
            }
            Arg::Var(name) | Arg::Param(name) => match fields.get(name) {
                Some(value) => value.clone(),
                None => {
                    missing.push(name.as_str());
                    continue;
                }
            },
        };
        params.insert(key.clone(), value);
    }
    debug_assert!(params.len() + missing.len() == args.len());
    (params, missing)
}
