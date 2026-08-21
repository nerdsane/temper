use chrono::{DateTime, Utc};
use serde_json::Value;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use crate::request_context::AgentContext;

use super::{DispatchError, ServerState};

const RATE_LIMIT_ENTITY_TYPE: &str = "RateLimit";
const RATE_LIMIT_ACTION_CLASS_WRITE: &str = "write";

#[derive(Debug, Clone)]
pub(crate) struct RuntimeRateLimitBucket {
    owner_id: String,
    action_class: String,
    tokens: i64,
    capacity: i64,
    refill_per_second: i64,
    last_refill_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct CommonsRateLimitExceeded {
    pub(crate) owner_id: String,
    pub(crate) action_class: String,
    pub(crate) retry_after_secs: Option<i64>,
}

#[derive(Debug, Clone)]
pub(crate) enum CommonsRateLimitError {
    Exceeded(CommonsRateLimitExceeded),
    Internal(String),
}

impl std::fmt::Display for CommonsRateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommonsRateLimitError::Exceeded(exceeded) => {
                if let Some(retry_after) = exceeded.retry_after_secs {
                    write!(
                        f,
                        "rate limit exceeded for owner '{}' action class '{}' (retry after {retry_after}s)",
                        exceeded.owner_id, exceeded.action_class
                    )
                } else {
                    write!(
                        f,
                        "rate limit exceeded for owner '{}' action class '{}'",
                        exceeded.owner_id, exceeded.action_class
                    )
                }
            }
            CommonsRateLimitError::Internal(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for CommonsRateLimitError {}

impl ServerState {
    pub(crate) fn clear_commons_rate_limit_cache(&self) {
        if let Ok(mut buckets) = self.commons_rate_limit_buckets.lock() {
            buckets.clear();
        }
    }

    pub(crate) fn commons_write_action_class(&self) -> &'static str {
        RATE_LIMIT_ACTION_CLASS_WRITE
    }

    pub(crate) fn commons_rate_limit_entity_id(owner_id: &str, action_class: &str) -> String {
        format!(
            "rl-{}-{}",
            rate_limit_id_slug(owner_id),
            rate_limit_id_slug(action_class)
        )
    }

    pub(crate) async fn consume_commons_rate_limit_token(
        &self,
        tenant: &TenantId,
        owner_id: &str,
        action_class: &str,
    ) -> Result<(), CommonsRateLimitError> {
        if owner_id.trim().is_empty() || action_class.trim().is_empty() {
            return Ok(());
        }

        if !self
            .is_entity_type_governed(tenant, RATE_LIMIT_ENTITY_TYPE)
            .map_err(CommonsRateLimitError::Internal)?
        {
            return Ok(());
        }

        let bucket_id = Self::commons_rate_limit_entity_id(owner_id, action_class);
        let cache_key = format!("{tenant}:{owner_id}:{action_class}");
        if !self
            .commons_rate_limit_buckets
            .lock()
            .map_err(|e| CommonsRateLimitError::Internal(format!("rate-limit lock poisoned: {e}")))?
            .contains_key(&cache_key)
        {
            let Some(bucket) = self
                .load_runtime_rate_limit_bucket(tenant, &bucket_id)
                .await?
            else {
                return Ok(());
            };
            self.commons_rate_limit_buckets
                .lock()
                .map_err(|e| {
                    CommonsRateLimitError::Internal(format!("rate-limit lock poisoned: {e}"))
                })?
                .entry(cache_key.clone())
                .or_insert(bucket);
        }

        let (tokens_after_consume, refill_time, bucket_before_consume) = {
            let mut buckets = self.commons_rate_limit_buckets.lock().map_err(|e| {
                CommonsRateLimitError::Internal(format!("rate-limit lock poisoned: {e}"))
            })?;
            let bucket = buckets.get_mut(&cache_key).ok_or_else(|| {
                CommonsRateLimitError::Internal(format!(
                    "rate-limit bucket cache miss for {owner_id}/{action_class}"
                ))
            })?;
            bucket.refill_to_now();
            if bucket.tokens <= 0 {
                return Err(CommonsRateLimitError::Exceeded(bucket.exceeded()));
            }
            let bucket_before_consume = bucket.clone();
            bucket.tokens -= 1;
            (bucket.tokens, bucket.last_refill_at, bucket_before_consume)
        };

        let mut agent = AgentContext::for_service("commons-rate-limit");
        agent.idempotency_key = Some(format!(
            "commons-rate-limit:{tenant}:{bucket_id}:{}:{tokens_after_consume}",
            refill_time.timestamp_millis(),
        ));
        let result = self
            .dispatch_tenant_action_core(
                tenant,
                RATE_LIMIT_ENTITY_TYPE,
                &bucket_id,
                "Consume",
                serde_json::json!({
                    "Tokens": tokens_after_consume,
                    "LastRefillAt": refill_time.to_rfc3339(),
                }),
                &agent,
                false,
                None,
                None,
            )
            .await
            .map_err(rate_limit_dispatch_error);

        let response = match result {
            Ok(response) => response,
            Err(err) => {
                self.restore_runtime_rate_limit_bucket(&cache_key, bucket_before_consume);
                return Err(err);
            }
        };

        if response.success {
            Ok(())
        } else {
            self.restore_runtime_rate_limit_bucket(&cache_key, bucket_before_consume);
            Err(CommonsRateLimitError::Internal(
                response
                    .error
                    .unwrap_or_else(|| format!("failed to consume rate-limit bucket {bucket_id}")),
            ))
        }
    }

    fn restore_runtime_rate_limit_bucket(&self, cache_key: &str, bucket: RuntimeRateLimitBucket) {
        if let Ok(mut buckets) = self.commons_rate_limit_buckets.lock() {
            buckets.insert(cache_key.to_string(), bucket);
        }
    }

    async fn load_runtime_rate_limit_bucket(
        &self,
        tenant: &TenantId,
        bucket_id: &str,
    ) -> Result<Option<RuntimeRateLimitBucket>, CommonsRateLimitError> {
        if !self
            .ensure_entity_loaded(tenant, RATE_LIMIT_ENTITY_TYPE, bucket_id)
            .await
        {
            return Ok(None);
        }

        let state = self
            .get_tenant_entity_state(tenant, RATE_LIMIT_ENTITY_TYPE, bucket_id)
            .await
            .map_err(|e| {
                CommonsRateLimitError::Internal(format!(
                    "failed to load rate-limit bucket {bucket_id}: {e}"
                ))
            })?;

        if state.state.status != "Active" {
            return Ok(None);
        }

        RuntimeRateLimitBucket::from_fields(&state.state.fields).map(Some)
    }
}

impl RuntimeRateLimitBucket {
    fn from_fields(fields: &Value) -> Result<Self, CommonsRateLimitError> {
        let owner_id = read_string(fields, "OwnerId")
            .ok_or_else(|| CommonsRateLimitError::Internal("RateLimit.OwnerId missing".into()))?;
        let action_class = read_string(fields, "ActionClass").ok_or_else(|| {
            CommonsRateLimitError::Internal("RateLimit.ActionClass missing".into())
        })?;
        let capacity = read_i64(fields, "Capacity")
            .ok_or_else(|| CommonsRateLimitError::Internal("RateLimit.Capacity missing".into()))?
            .max(0);
        let tokens = read_i64(fields, "Tokens")
            .ok_or_else(|| CommonsRateLimitError::Internal("RateLimit.Tokens missing".into()))?
            .clamp(0, capacity);
        let refill_per_second = read_i64(fields, "RefillPerSecond")
            .ok_or_else(|| {
                CommonsRateLimitError::Internal("RateLimit.RefillPerSecond missing".into())
            })?
            .max(0);
        let last_refill_at = read_string(fields, "LastRefillAt")
            .and_then(|raw| DateTime::parse_from_rfc3339(&raw).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(sim_now);

        Ok(Self {
            owner_id,
            action_class,
            tokens,
            capacity,
            refill_per_second,
            last_refill_at,
        })
    }

    fn refill_to_now(&mut self) {
        let now = sim_now();
        let elapsed_secs = (now - self.last_refill_at).num_seconds().max(0);
        if elapsed_secs > 0 && self.refill_per_second > 0 {
            let refill = elapsed_secs.saturating_mul(self.refill_per_second);
            self.tokens = self.tokens.saturating_add(refill).min(self.capacity);
        }
        self.last_refill_at = now;
    }

    fn exceeded(&self) -> CommonsRateLimitExceeded {
        let retry_after_secs = if self.refill_per_second > 0 {
            Some((1 + self.refill_per_second - 1) / self.refill_per_second)
        } else {
            None
        };
        CommonsRateLimitExceeded {
            owner_id: self.owner_id.clone(),
            action_class: self.action_class.clone(),
            retry_after_secs,
        }
    }
}

fn read_string(fields: &Value, key: &str) -> Option<String> {
    fields
        .get(key)
        .or_else(|| fields.get(key.to_ascii_lowercase()))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn read_i64(fields: &Value, key: &str) -> Option<i64> {
    let value = fields
        .get(key)
        .or_else(|| fields.get(key.to_ascii_lowercase()))?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))
        .or_else(|| value.as_f64().map(|v| v.floor() as i64))
        .or_else(|| value.as_str().and_then(|v| v.parse::<i64>().ok()))
}

fn rate_limit_id_slug(raw: &str) -> String {
    let mut slug = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '-' | '_') {
            slug.push(ch);
        } else {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

fn rate_limit_dispatch_error(error: DispatchError) -> CommonsRateLimitError {
    CommonsRateLimitError::Internal(format!("failed to persist rate-limit consume: {error}"))
}
