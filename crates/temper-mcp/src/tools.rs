//! Temper tool dispatch for MCP execute sandbox calls.

use monty::MontyObject;
use reqwest::Method;
use serde_json::Value;

use super::runtime::RuntimeContext;
use super::sandbox::{
    escape_odata_key, expect_json_object_arg, expect_string_arg, format_http_error,
};

impl RuntimeContext {
    pub(super) async fn dispatch_temper_method(
        &self,
        method: &str,
        args: &[MontyObject],
        kwargs: &[(MontyObject, MontyObject)],
    ) -> std::result::Result<Value, String> {
        if !kwargs.is_empty() {
            return Err(format!(
                "temper.{method} does not support keyword arguments in this MCP server"
            ));
        }

        // Dataclass method calls include self as the first arg.
        let args = if args.is_empty() { args } else { &args[1..] };

        match method {
            "list" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);

                let body = self
                    .temper_request(&tenant, Method::GET, format!("/tdata/{set}"), None)
                    .await?;
                Ok(body.get("value").cloned().unwrap_or(body))
            }
            "get" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let entity_id = expect_string_arg(args, 2, "entity_id", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);
                let key = escape_odata_key(&entity_id);

                self.temper_request(&tenant, Method::GET, format!("/tdata/{set}('{key}')"), None)
                    .await
            }
            "create" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let fields = expect_json_object_arg(args, 2, "fields", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);

                self.temper_request(
                    &tenant,
                    Method::POST,
                    format!("/tdata/{set}"),
                    Some(Value::Object(fields)),
                )
                .await
            }
            "action" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let entity_id = expect_string_arg(args, 2, "entity_id", method)?;
                let action_name = expect_string_arg(args, 3, "action_name", method)?;
                let body = expect_json_object_arg(args, 4, "body", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);
                let key = escape_odata_key(&entity_id);

                self.temper_request(
                    &tenant,
                    Method::POST,
                    format!("/tdata/{set}('{key}')/Temper.{action_name}"),
                    Some(Value::Object(body)),
                )
                .await
            }
            "patch" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let entity_id = expect_string_arg(args, 2, "entity_id", method)?;
                let fields = expect_json_object_arg(args, 3, "fields", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);
                let key = escape_odata_key(&entity_id);

                self.temper_request(
                    &tenant,
                    Method::PATCH,
                    format!("/tdata/{set}('{key}')"),
                    Some(Value::Object(fields)),
                )
                .await
            }
            _ => Err(format!(
                "unknown temper method '{method}'. Allowed methods: list, get, create, action, patch"
            )),
        }
    }

    fn resolve_entity_set(&self, tenant: &str, entity_or_set: &str) -> String {
        if let Some(metadata) = self.app_metadata.get(tenant) {
            if metadata.entity_set_to_type.contains_key(entity_or_set) {
                return entity_or_set.to_string();
            }
            if let Some(set) = metadata.entity_type_to_set.get(entity_or_set) {
                return set.clone();
            }
            let plural_guess = format!("{entity_or_set}s");
            if metadata.entity_set_to_type.contains_key(&plural_guess) {
                return plural_guess;
            }
        }
        entity_or_set.to_string()
    }

    async fn temper_request(
        &self,
        tenant: &str,
        method: Method,
        path: String,
        body: Option<Value>,
    ) -> std::result::Result<Value, String> {
        let url = format!("http://127.0.0.1:{}{path}", self.temper_port);
        let mut request = self
            .http
            .request(method, &url)
            .header("X-Tenant-Id", tenant)
            .header("Accept", "application/json");

        if let Some(ref payload) = body {
            request = request.json(payload);
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("failed to call Temper at {url}: {e}"))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("failed to read Temper response body: {e}"))?;

        if status.is_success() {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).or_else(|_| Ok(Value::String(text)));
        }

        Err(format_http_error(status, &text))
    }
}
