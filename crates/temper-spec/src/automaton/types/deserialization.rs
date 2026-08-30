//! Strict deserializers for schema types with narrow legacy compatibility.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use std::collections::BTreeMap;

use super::super::toml_parser::{deserialize_boolish, deserialize_copy_fields};
use super::{ActionParam, Effect, Integration, default_param_type, default_webhook};

impl<'de> Deserialize<'de> for ActionParam {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match toml::Value::deserialize(deserializer)? {
            toml::Value::String(name) => Ok(Self::Named(name)),
            toml::Value::Table(mut fields) => {
                for key in fields.keys() {
                    if !matches!(key.as_str(), "name" | "type" | "entity_type" | "nullable") {
                        return Err(de::Error::custom(format!(
                            "unknown action parameter field `{key}`"
                        )));
                    }
                }
                let name = take_string_field(&mut fields, "name")
                    .map_err(de::Error::custom)?
                    .ok_or_else(|| de::Error::missing_field("name"))?;
                let param_type = take_string_field(&mut fields, "type")
                    .map_err(de::Error::custom)?
                    .unwrap_or_else(default_param_type);
                let entity_type =
                    take_string_field(&mut fields, "entity_type").map_err(de::Error::custom)?;
                let nullable = match fields.remove("nullable") {
                    None => false,
                    Some(toml::Value::Boolean(value)) => value,
                    Some(_) => {
                        return Err(de::Error::custom(
                            "action parameter `nullable` must be a boolean",
                        ));
                    }
                };
                Ok(Self::Typed {
                    name,
                    param_type,
                    entity_type,
                    nullable,
                })
            }
            value => Err(de::Error::custom(format!(
                "action parameter must be a name string or typed table, got {value}"
            ))),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum EffectDefinition {
    #[serde(rename = "increment", alias = "IncrementCounter")]
    Increment {
        var: String,
        #[serde(default)]
        amount: Option<String>,
    },
    #[serde(rename = "decrement", alias = "DecrementCounter")]
    Decrement {
        var: String,
        #[serde(default)]
        amount: Option<String>,
    },
    #[serde(rename = "set_counter_from_param")]
    SetCounterFromParam {
        var: String,
        #[serde(default)]
        param: Option<String>,
    },
    #[serde(rename = "set_bool")]
    SetBool {
        var: String,
        #[serde(deserialize_with = "deserialize_boolish")]
        value: bool,
    },
    #[serde(rename = "emit", alias = "emit_event")]
    Emit { event: String },
    #[serde(rename = "list_append")]
    ListAppend {
        #[serde(alias = "list")]
        var: String,
    },
    #[serde(rename = "list_remove_at")]
    ListRemoveAt {
        #[serde(alias = "list")]
        var: String,
    },
    #[serde(rename = "trigger")]
    Trigger { name: String },
    #[serde(rename = "schedule")]
    Schedule { action: String, delay_seconds: u64 },
    #[serde(rename = "schedule_at")]
    ScheduleAt { action: String, field: String },
    #[serde(rename = "spawn", alias = "spawn_entity")]
    Spawn {
        entity_type: String,
        entity_id_source: String,
        #[serde(default)]
        initial_action: Option<String>,
        #[serde(default)]
        store_id_in: Option<String>,
        #[serde(default, deserialize_with = "deserialize_copy_fields")]
        copy_fields: Option<Vec<String>>,
    },
}

impl<'de> Deserialize<'de> for Effect {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match EffectDefinition::deserialize(deserializer)? {
            EffectDefinition::Increment { var, amount } => Self::Increment { var, amount },
            EffectDefinition::Decrement { var, amount } => Self::Decrement { var, amount },
            EffectDefinition::SetCounterFromParam { var, param } => {
                let param = param.unwrap_or_else(|| var.clone());
                Self::SetCounterFromParam { var, param }
            }
            EffectDefinition::SetBool { var, value } => Self::SetBool { var, value },
            EffectDefinition::Emit { event } => Self::Emit { event },
            EffectDefinition::ListAppend { var } => Self::ListAppend { var },
            EffectDefinition::ListRemoveAt { var } => Self::ListRemoveAt { var },
            EffectDefinition::Trigger { name } => Self::Trigger { name },
            EffectDefinition::Schedule {
                action,
                delay_seconds,
            } => Self::Schedule {
                action,
                delay_seconds,
            },
            EffectDefinition::ScheduleAt { action, field } => Self::ScheduleAt { action, field },
            EffectDefinition::Spawn {
                entity_type,
                entity_id_source,
                initial_action,
                store_id_in,
                copy_fields,
            } => Self::Spawn {
                entity_type,
                entity_id_source,
                initial_action,
                store_id_in,
                copy_fields,
            },
        })
    }
}

impl<'de> Deserialize<'de> for Integration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = toml::Value::deserialize(deserializer)?;
        let toml::Value::Table(mut fields) = value else {
            return Err(de::Error::custom("integration must be a TOML table"));
        };

        let name = take_string_field(&mut fields, "name")
            .map_err(de::Error::custom)?
            .ok_or_else(|| de::Error::missing_field("name"))?;
        let trigger = take_string_field(&mut fields, "trigger")
            .map_err(de::Error::custom)?
            .ok_or_else(|| de::Error::missing_field("trigger"))?;
        let integration_type = take_string_field(&mut fields, "type")
            .map_err(de::Error::custom)?
            .unwrap_or_else(default_webhook);
        let module = take_string_field(&mut fields, "module").map_err(de::Error::custom)?;
        let on_success = take_string_field(&mut fields, "on_success").map_err(de::Error::custom)?;
        let on_failure = take_string_field(&mut fields, "on_failure").map_err(de::Error::custom)?;
        if fields.remove("failure_routes").is_some() {
            return Err(de::Error::custom(
                "integration.failure_routes is reserved resolved metadata",
            ));
        }
        let llm = match fields.remove("llm") {
            None => false,
            Some(toml::Value::Boolean(value)) => value,
            Some(toml::Value::String(value)) if value.eq_ignore_ascii_case("true") => true,
            Some(toml::Value::String(value)) if value.eq_ignore_ascii_case("false") => false,
            Some(value) => {
                return Err(de::Error::custom(format!(
                    "integration field `llm` must be a boolean, got {value}"
                )));
            }
        };
        let mut config = match fields.remove("config") {
            None => BTreeMap::new(),
            Some(toml::Value::Table(config)) => config
                .into_iter()
                .map(|(key, value)| (key, integration_config_value(value)))
                .collect(),
            Some(value) => {
                BTreeMap::from([("config".to_string(), integration_config_value(value))])
            }
        };
        for (key, value) in fields {
            if config
                .insert(key.clone(), integration_config_value(value))
                .is_some()
            {
                return Err(de::Error::custom(format!(
                    "integration config key `{key}` declared twice"
                )));
            }
        }

        Ok(Self {
            name,
            trigger,
            integration_type,
            module,
            on_success,
            on_failure,
            failure_routes: Vec::new(),
            llm,
            config,
        })
    }
}

fn take_string_field(
    fields: &mut toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match fields.remove(key) {
        None => Ok(None),
        Some(toml::Value::String(value)) => Ok(Some(value)),
        Some(value) => Err(format!("field `{key}` must be a string, got {value}")),
    }
}

fn integration_config_value(value: toml::Value) -> String {
    match value {
        toml::Value::String(value) => value,
        value => value.to_string(),
    }
}
