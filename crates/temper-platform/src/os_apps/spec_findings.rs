//! What an installed app's specs lack that the kernel loads anyway.
//!
//! A spec with a liveness gap (ADR-0050), or a CSDL that declares a field
//! name the kernel owns, still installs. The install request is where the
//! author hears of it, once. A boot replays and restores the same specs for
//! every tenant, and logging there repeated every finding on every boot.

/// Log at warn each liveness gap in `specs` and each kernel-owned property
/// `csdl` declares, for the install of `app` into `tenant`.
pub(super) fn log(tenant: &str, app: &str, specs: &[(String, String)], csdl: Option<&str>) {
    for (entity, source) in specs {
        let Ok(automaton) = temper_spec::automaton::parse_automaton(source) else {
            continue;
        };
        let Err(gaps) = automaton.validate_liveness_coverage() else {
            continue;
        };
        for gap in gaps {
            tracing::warn!(
                tenant,
                app,
                entity,
                state = %gap.state,
                "liveness coverage missing (ADR-0050): non-terminal state has no state_timeout and is not in allow_indefinite_states"
            );
        }
    }
    let Some(Ok(csdl)) = csdl.map(temper_spec::csdl::parse_csdl) else {
        return;
    };
    for property in temper_server::registry::server_derived_csdl_properties(&csdl) {
        tracing::warn!(
            tenant,
            app,
            property,
            "CSDL declares a server-derived field name; the actor will overwrite it, so this \
             property reads differently over OData than through the entity actor"
        );
    }
}
