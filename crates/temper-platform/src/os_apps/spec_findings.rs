//! What an installed app's specs lack: what an install refuses, and what the
//! kernel loads anyway.
//!
//! A spec with a liveness gap (ADR-0050), or a CSDL that declares a field
//! name the kernel owns, still installs. The install request is where the
//! author hears of it, once. A boot replays and restores the same specs for
//! every tenant, and logging there repeated every finding on every boot.

use temper_runtime::tenant::TenantId;

use crate::state::PlatformState;

/// Refuse a spec this install changes when installs refuse its shape, before
/// anything is written. A spec the tenant already runs, source for source, is
/// not refused: refusing it changes nothing the tenant runs, and it would
/// refuse every install whose closure holds that app. It is named in a
/// warning instead, as registry loading names a stored one. Boot replay skips
/// this gate.
pub(super) fn refusals(
    state: &PlatformState,
    tenant: &str,
    app: &str,
    specs: &[(String, String)],
) -> Result<(), String> {
    let tenant_id = TenantId::new(tenant);
    let registry = state.registry.read().expect("Spec registry lock poisoned");
    for (entity, source) in specs {
        let automaton = temper_spec::automaton::parse_automaton(source)
            .map_err(|e| format!("os-app '{app}' spec {entity} failed to parse: {e}"))?;
        let findings = temper_spec::automaton::lint_install_refusals(&automaton, entity);
        let runs = registry
            .get_spec(&tenant_id, entity)
            .is_some_and(|spec| spec.ioa_source == *source);
        if runs {
            for finding in findings {
                tracing::warn!(
                    tenant,
                    app,
                    entity_type = %entity,
                    code = %finding.code,
                    "stored spec an install would refuse: {}",
                    finding.message
                );
            }
        } else if let Some(finding) = findings.first() {
            return Err(format!("os-app '{app}' spec {entity}: {}", finding.message));
        }
    }
    Ok(())
}

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
