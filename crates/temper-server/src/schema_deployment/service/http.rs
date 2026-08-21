use super::*;

fn http_context(
    authenticated: Option<&AuthenticatedRequestContext>,
) -> Result<(String, SecurityContext), Box<Response>> {
    let authenticated = require_authenticated_context(authenticated)
        .map_err(IntoResponse::into_response)
        .map_err(Box::new)?;
    Ok((
        authenticated.tenant().as_str().to_string(),
        authenticated.security_context().clone(),
    ))
}

pub(crate) async fn submit_http(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::Json(request): axum::Json<SubmitSchemaBundleRequestV1>,
) -> Response {
    let (tenant, security) = match http_context(authenticated.as_deref()) {
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path((scope_id, digest)): Path<(String, String)>,
) -> Response {
    let (tenant, security) = match http_context(authenticated.as_deref()) {
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
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
    let (tenant, security) = match http_context(authenticated.as_deref()) {
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
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
    let (tenant, security) = match http_context(authenticated.as_deref()) {
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
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
    let (tenant, security) = match http_context(authenticated.as_deref()) {
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::Json(request): axum::Json<StartSchemaMigrationRequestV1>,
) -> Response {
    let (tenant, security) = match http_context(authenticated.as_deref()) {
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: HeaderMap,
    Path((scope_id, job_id)): Path<(String, String)>,
) -> Response {
    let (tenant, security) = match http_context(authenticated.as_deref()) {
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
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
    let (tenant, security) = match http_context(authenticated.as_deref()) {
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
