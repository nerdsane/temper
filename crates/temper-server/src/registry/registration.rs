//! Convenience entry points for tenant registration.

use super::{CsdlDocument, RegistryError, SpecRegistry};
use crate::trigger::types::ReactionRule;
use temper_runtime::tenant::TenantId;

impl SpecRegistry {
    /// Register a tenant with its CSDL document and IOA specs.
    ///
    /// `ioa_sources` maps entity type name to IOA TOML source string.
    /// Each source is parsed into an [`Automaton`] and compiled into a
    /// [`TransitionTable`].
    ///
    /// If the tenant already exists, existing entity tables are hot-swapped
    /// via their [`SwapController`] so that live actors see the new table on
    /// their next action dispatch — no restart required. New entities are
    /// added; entities not in the new spec set are removed.
    pub fn register_tenant(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
    ) {
        self.try_register_tenant_with_reactions_and_constraints(
            tenant,
            csdl,
            csdl_xml,
            ioa_sources,
            Vec::new(),
            None,
            false,
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Fallible variant of [`register_tenant`](Self::register_tenant).
    pub fn try_register_tenant(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
    ) -> Result<(), RegistryError> {
        self.try_register_tenant_with_reactions_and_constraints(
            tenant,
            csdl,
            csdl_xml,
            ioa_sources,
            Vec::new(),
            None,
            false,
        )
    }

    /// Register a tenant with CSDL, IOA specs, reaction rules, and optional
    /// cross-entity invariant definitions.
    pub fn register_tenant_with_reactions_and_constraints(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
        reactions: Vec<ReactionRule>,
        cross_invariants_source: Option<String>,
    ) {
        self.try_register_tenant_with_reactions_and_constraints(
            tenant,
            csdl,
            csdl_xml,
            ioa_sources,
            reactions,
            cross_invariants_source,
            false,
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Register a tenant with CSDL, IOA specs, and reaction rules.
    pub fn register_tenant_with_reactions(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
        reactions: Vec<ReactionRule>,
    ) {
        self.try_register_tenant_with_reactions_and_constraints(
            tenant,
            csdl,
            csdl_xml,
            ioa_sources,
            reactions,
            None,
            false,
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Fallible variant of [`register_tenant_with_reactions`](Self::register_tenant_with_reactions).
    pub fn try_register_tenant_with_reactions(
        &mut self,
        tenant: impl Into<TenantId>,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: &[(&str, &str)],
        reactions: Vec<ReactionRule>,
    ) -> Result<(), RegistryError> {
        self.try_register_tenant_with_reactions_and_constraints(
            tenant,
            csdl,
            csdl_xml,
            ioa_sources,
            reactions,
            None,
            false,
        )
    }
}
