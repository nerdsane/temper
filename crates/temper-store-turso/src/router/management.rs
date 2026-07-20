//! Tenant connection and provisioning helpers.

use super::*;

impl TenantStoreRouter {
    /// Load all tenant registry rows from the platform DB.
    pub(super) async fn load_tenant_registry(
        &self,
    ) -> Result<Vec<TenantRegistryRow>, PersistenceError> {
        let conn = self.platform.connection().map_err(storage_error)?;
        let mut rows = conn
            .query(
                "SELECT tenant_id, turso_db_url, turso_auth_token, status
                 FROM tenant_registry ORDER BY tenant_id",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TenantRegistryRow {
                tenant_id: row.get::<String>(0).map_err(storage_error)?,
                turso_db_url: row.get::<String>(1).map_err(storage_error)?,
                turso_auth_token: row.get::<Option<String>>(2).ok().flatten(),
                status: row.get::<String>(3).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Pre-connect to all active tenants in the registry.
    pub(super) async fn connect_registered_tenants(&self) -> Result<(), PersistenceError> {
        let registry = self.load_tenant_registry().await?;
        let mut tenants = self.tenants.write().await;

        for entry in &registry {
            if entry.status != "active" {
                continue;
            }
            match TursoEventStore::new(&entry.turso_db_url, entry.turso_auth_token.as_deref()).await
            {
                Ok(store) => {
                    tenants.insert(entry.tenant_id.clone(), store);
                    info!(tenant = %entry.tenant_id, "Connected to tenant database");
                }
                Err(e) => {
                    warn!(
                        tenant = %entry.tenant_id,
                        error = %e,
                        "Failed to connect to tenant database, skipping"
                    );
                }
            }
        }
        Ok(())
    }

    /// Connect to a tenant from the registry (not cached yet).
    pub(super) async fn connect_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<TursoEventStore, PersistenceError> {
        let registry = self.load_tenant_registry().await?;
        let entry = registry
            .iter()
            .find(|r| r.tenant_id == tenant_id && r.status == "active")
            .ok_or_else(|| {
                PersistenceError::Storage(format!("tenant '{tenant_id}' not found in registry"))
            })?;

        let store =
            TursoEventStore::new(&entry.turso_db_url, entry.turso_auth_token.as_deref()).await?;
        self.tenants
            .write()
            .await
            .insert(tenant_id.to_string(), store.clone());

        info!(tenant = tenant_id, "Connected to tenant database on demand");
        Ok(store)
    }

    /// Provision a new database for a tenant.
    ///
    /// In local mode: creates a `file:{base_dir}/{tenant_id}.db` SQLite file.
    /// In cloud mode: calls the Turso Cloud API to create a database.
    pub(super) async fn provision_database(
        &self,
        tenant_id: &str,
    ) -> Result<(String, Option<String>), PersistenceError> {
        #[cfg(feature = "cloud")]
        if let (Some(api_token), Some(org)) = (&self.turso_api_token, &self.turso_org) {
            return self
                .provision_cloud_database(tenant_id, api_token, org)
                .await;
        }

        // Local mode: create a file-based database.
        let base_dir = self.local_base_dir.as_deref().unwrap_or(".temper/tenants");

        std::fs::create_dir_all(base_dir).map_err(|e| {
            PersistenceError::Storage(format!("failed to create tenant directory {base_dir}: {e}"))
        })?;

        let db_url = format!("file:{base_dir}/{tenant_id}.db");
        info!(tenant_id, db_url, "Provisioned local tenant database");
        Ok((db_url, None))
    }

    /// Provision a database via the Turso Cloud Platform API.
    #[cfg(feature = "cloud")]
    pub(super) async fn provision_cloud_database(
        &self,
        tenant_id: &str,
        api_token: &str,
        org: &str,
    ) -> Result<(String, Option<String>), PersistenceError> {
        let client = reqwest::Client::new();

        // Sanitize tenant ID for use as a database name (alphanumeric + hyphens).
        let db_name = format!("temper-{tenant_id}");

        let mut body = serde_json::json!({
            "name": db_name,
        });
        if let Some(group) = &self.turso_group {
            body["group"] = serde_json::Value::String(group.clone());
        }

        // Create the database.
        let resp = client
            .post(format!(
                "{}/v1/organizations/{org}/databases",
                self.turso_api_base_url
            ))
            .bearer_auth(api_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| PersistenceError::Storage(format!("Turso API request failed: {e}")))?;

        let hostname = if resp.status() == reqwest::StatusCode::CONFLICT {
            // DB already exists: recover by reading its hostname and creating a fresh token.
            let lookup_resp = client
                .get(format!(
                    "{}/v1/organizations/{org}/databases/{db_name}",
                    self.turso_api_base_url
                ))
                .bearer_auth(api_token)
                .send()
                .await
                .map_err(|e| {
                    PersistenceError::Storage(format!("Turso API lookup request failed: {e}"))
                })?;

            if !lookup_resp.status().is_success() {
                let status = lookup_resp.status();
                let body_text = lookup_resp
                    .text()
                    .await
                    .unwrap_or_else(|_| "<no body>".to_string());
                return Err(PersistenceError::Storage(format!(
                    "Turso API database lookup returned {status}: {body_text}"
                )));
            }

            let lookup_json: serde_json::Value = lookup_resp.json().await.map_err(|e| {
                PersistenceError::Storage(format!("Turso API database lookup parse: {e}"))
            })?;

            lookup_json["database"]["Hostname"]
                .as_str()
                .or_else(|| lookup_json["database"]["hostname"].as_str())
                .ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "Turso API missing hostname in lookup response: {lookup_json}"
                    ))
                })?
                .to_string()
        } else {
            if !resp.status().is_success() {
                let status = resp.status();
                let body_text = resp
                    .text()
                    .await
                    .unwrap_or_else(|_| "<no body>".to_string());
                return Err(PersistenceError::Storage(format!(
                    "Turso API returned {status}: {body_text}"
                )));
            }

            let create_resp: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| PersistenceError::Storage(format!("Turso API response parse: {e}")))?;

            create_resp["database"]["Hostname"]
                .as_str()
                .or_else(|| create_resp["database"]["hostname"].as_str())
                .ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "Turso API missing hostname in response: {create_resp}"
                    ))
                })?
                .to_string()
        };

        let db_url = format!("libsql://{hostname}");

        // Create an auth token for the database (new or existing).
        let token_resp = client
            .post(format!(
                "{}/v1/organizations/{org}/databases/{db_name}/auth/tokens",
                self.turso_api_base_url
            ))
            .bearer_auth(api_token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| {
                PersistenceError::Storage(format!("Turso API token request failed: {e}"))
            })?;

        if !token_resp.status().is_success() {
            let status = token_resp.status();
            let body_text = token_resp
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(PersistenceError::Storage(format!(
                "Turso API token creation returned {status}: {body_text}"
            )));
        }

        let token_json: serde_json::Value = token_resp
            .json()
            .await
            .map_err(|e| PersistenceError::Storage(format!("Turso token parse: {e}")))?;

        let auth_token = token_json["jwt"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "Turso API missing jwt in token response: {token_json}"
                ))
            })?;

        info!(tenant_id, db_url, "Provisioned Turso Cloud tenant database");
        Ok((db_url, Some(auth_token)))
    }

    /// List all connected tenant IDs (cached connections only).
    pub async fn connected_tenants(&self) -> Vec<String> {
        self.tenants.read().await.keys().cloned().collect()
    }
}
