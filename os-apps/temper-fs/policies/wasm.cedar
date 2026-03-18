// TemperFS WASM Module Authorization Policies

// Allow blob_adapter to make HTTP calls (blob storage upload/download)
permit(
    principal is Agent,
    action == Action::"http_call",
    resource is HttpEndpoint
) when {
    context.module == "blob_adapter"
};

// Allow blob_adapter to access secrets (blob_endpoint, blob_bucket)
permit(
    principal is Agent,
    action == Action::"access_secret",
    resource is Secret
) when {
    context.module == "blob_adapter"
};
