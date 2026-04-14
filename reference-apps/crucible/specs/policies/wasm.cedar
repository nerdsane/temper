// Cedar policy — WASM module authorization for cron scheduling
//
// Permits the Crucible scheduler and trigger WASM modules to make
// HTTP calls (re-entrant to Temper OData + observe endpoints) and
// access secrets.

permit(
    principal is Agent,
    action == Action::"http_call",
    resource is HttpEndpoint
) when {
    ["crucible_cron_trigger", "crucible_scheduler_check", "crucible_scheduler_heartbeat"].contains(context.module)
};

permit(
    principal is Agent,
    action == Action::"access_secret",
    resource is Secret
) when {
    ["crucible_cron_trigger", "crucible_scheduler_check", "crucible_scheduler_heartbeat"].contains(context.module)
};

// The heartbeat WASM uses the observe long-poll endpoint to sleep.
permit(
    principal,
    action == Action::"read_entities",
    resource is Entity
);
