use super::*;

fn http_context(
    headers: &HeaderMap,
    state: &ServerState,
    resolved_identity: Option<&ResolvedIdentity>,
) -> Result<(String, SecurityContext), Box<Response>> {
    let tenant = extract_tenant(headers, state)
        .map_err(IntoResponse::into_response)
        .map_err(Box::new)?;
    let security = resolved_identity.map_or_else(
        || security_context_from_headers(headers, None, None, None),
        |identity| {
            SecurityContext::from_resolved_identity(
                &identity.agent_instance_id,
                &identity.agent_type_name,
                None,
            )
        },
    );
    Ok((tenant.as_str().to_string(), security))
}

pub(crate) async fn submit_http(
    State(state): State<ServerState>,
    resolved_identity: Option<Extension<ResolvedIdentity>>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<SubmitSchemaBundleRequestV1>,
) -> Response {
    let (tenant, security) = match http_context(&headers, &state, resolved_identity.as_deref()) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    http_response(
        GovernedSchemaDeploymentService::new(&state)
            .submit(&tenant, &security, request)
            .await,
    )
}

pub(crate) async fn get_http(
    State(state): State<ServerState>,
    resolved_identity: Option<Extension<ResolvedIdentity>>,
    headers: HeaderMap,
    Path((scope_id, digest)): Path<(String, String)>,
) -> Response {
    let (tenant, security) = match http_context(&headers, &state, resolved_identity.as_deref()) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    http_response(
        GovernedSchemaDeploymentService::new(&state)
            .get(
                &tenant,
                &security,
                SchemaScope {
                    kind: SchemaScopeKind::Task,
                    id: scope_id,
                },
                &digest,
            )
            .await,
    )
}

pub(crate) async fn verify_http(
    State(state): State<ServerState>,
    resolved_identity: Option<Extension<ResolvedIdentity>>,
    headers: HeaderMap,
    Path((scope_id, digest)): Path<(String, String)>,
    axum::Json(mut request): axum::Json<VerifySchemaBundleRequestV1>,
) -> Response {
    if request.scope.id != scope_id || request.bundle_digest != digest {
        return http_response(Err(ServiceError::new(
            "scope_mismatch",
            "path and request identity differ",
            false,
        )));
    }
    let (tenant, security) = match http_context(&headers, &state, resolved_identity.as_deref()) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    request.scope.kind = "task".into();
    http_response(
        GovernedSchemaDeploymentService::new(&state)
            .verify(&tenant, &security, request)
            .await,
    )
}

pub(crate) async fn activate_http(
    State(state): State<ServerState>,
    resolved_identity: Option<Extension<ResolvedIdentity>>,
    headers: HeaderMap,
    Path((scope_id, digest)): Path<(String, String)>,
    axum::Json(mut request): axum::Json<ActivateSchemaBundleRequestV1>,
) -> Response {
    if request.scope.id != scope_id || request.bundle_digest != digest {
        return http_response(Err(ServiceError::new(
            "scope_mismatch",
            "path and request identity differ",
            false,
        )));
    }
    let (tenant, security) = match http_context(&headers, &state, resolved_identity.as_deref()) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    request.scope.kind = "task".into();
    http_response(
        GovernedSchemaDeploymentService::new(&state)
            .activate(&tenant, &security, request)
            .await,
    )
}

pub(crate) async fn retire_http(
    State(state): State<ServerState>,
    resolved_identity: Option<Extension<ResolvedIdentity>>,
    headers: HeaderMap,
    Path((scope_id, digest)): Path<(String, String)>,
    axum::Json(mut request): axum::Json<RetireSchemaBundleRequestV1>,
) -> Response {
    if request.scope.id != scope_id || request.bundle_digest != digest {
        return http_response(Err(ServiceError::new(
            "scope_mismatch",
            "path and request identity differ",
            false,
        )));
    }
    let (tenant, security) = match http_context(&headers, &state, resolved_identity.as_deref()) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    request.scope.kind = "task".into();
    http_response(
        GovernedSchemaDeploymentService::new(&state)
            .retire(&tenant, &security, request)
            .await,
    )
}

pub(crate) async fn start_migration_http(
    State(state): State<ServerState>,
    resolved_identity: Option<Extension<ResolvedIdentity>>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<StartSchemaMigrationRequestV1>,
) -> Response {
    let (tenant, security) = match http_context(&headers, &state, resolved_identity.as_deref()) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    migration_http_response(
        GovernedSchemaDeploymentService::new(&state)
            .start_migration(&tenant, &security, request)
            .await,
    )
}

pub(crate) async fn get_migration_http(
    State(state): State<ServerState>,
    resolved_identity: Option<Extension<ResolvedIdentity>>,
    headers: HeaderMap,
    Path((scope_id, job_id)): Path<(String, String)>,
) -> Response {
    let (tenant, security) = match http_context(&headers, &state, resolved_identity.as_deref()) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let request_id = headers
        .get("x-temper-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .unwrap_or(&job_id)
        .to_string();
    migration_http_response(
        GovernedSchemaDeploymentService::new(&state)
            .get_migration(
                &tenant,
                &security,
                GetSchemaMigrationRequestV1 {
                    request_id,
                    scope: SchemaScopeV1 {
                        kind: "task".into(),
                        id: scope_id,
                    },
                    job_id,
                },
            )
            .await,
    )
}

pub(crate) async fn retry_migration_http(
    State(state): State<ServerState>,
    resolved_identity: Option<Extension<ResolvedIdentity>>,
    headers: HeaderMap,
    Path((scope_id, job_id)): Path<(String, String)>,
    axum::Json(mut request): axum::Json<RetrySchemaMigrationRequestV1>,
) -> Response {
    if request.scope.id != scope_id || request.job_id != job_id {
        return migration_http_response(Err(ServiceError::new(
            "scope_mismatch",
            "path and request identity differ",
            false,
        )));
    }
    let (tenant, security) = match http_context(&headers, &state, resolved_identity.as_deref()) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    request.scope.kind = "task".into();
    migration_http_response(
        GovernedSchemaDeploymentService::new(&state)
            .retry_migration(&tenant, &security, request)
            .await,
    )
}
