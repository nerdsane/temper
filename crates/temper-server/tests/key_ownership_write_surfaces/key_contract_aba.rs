//! ABA-safe key-contract watermark regressions.

use super::*;

const DOC_IOA_WITHOUT_KEYS: &str = r#"
[automaton]
name = "Doc"
states = ["New", "Ready"]
initial = "New"

[[state]]
name = "WorkspaceId"
type = "string"
initial = ""

[[state]]
name = "Path"
type = "string"
initial = ""

[[action]]
name = "Create"
kind = "input"
from = ["New"]
to = "Ready"
params = ["WorkspaceId", "Path"]

[[action]]
name = "Rekey"
kind = "input"
from = ["Ready"]
to = "Ready"
params = ["WorkspaceId", "Path"]
"#;

async fn persist_doc_generation(
    platform: &temper_server::platform_store::SimPlatformStore,
    tenant: &TenantId,
    ioa_source: &str,
) -> Result<Vec<String>, String> {
    use temper_server::platform_store::{
        PlatformStore, SpecPublication, SpecPublicationMode, TenantConstraintsPublication,
        TenantPolicyPublication,
    };

    let content_hash = temper_store_turso::spec_content_hash(ioa_source);
    PlatformStore::publish_specs(
        platform,
        tenant.as_str(),
        &[SpecPublication {
            entity_type: "Doc",
            ioa_source,
            csdl_xml: CSDL_XML,
            content_hash: &content_hash,
        }],
        SpecPublicationMode::Replace,
        TenantConstraintsPublication::Preserve,
        TenantPolicyPublication::Preserve,
        None,
        None,
        &[],
    )
    .await
}

fn doc_publication_intent(ioa_source: &str) -> String {
    ServerState::spec_publication_intent(
        "test-doc-replace",
        [
            ("csdl", CSDL_XML.as_bytes()),
            ("spec:Doc", ioa_source.as_bytes()),
        ],
    )
}

fn unrelated_task_publication_intent() -> String {
    ServerState::spec_publication_intent(
        "test-task-merge",
        [
            ("csdl", CSDL_XML.as_bytes()),
            (
                "spec:Task",
                b"[automaton]\nname = \"Task\"\nstates = [\"Ready\"]\ninitial = \"Ready\"\n",
            ),
        ],
    )
}

async fn publish_doc_generation(
    server: &ServerState,
    platform: &temper_server::platform_store::SimPlatformStore,
    publication_guard: &mut temper_server::state::SpecPublicationGuard,
    tenant: &TenantId,
    ioa_source: &str,
) {
    server
        .arm_spec_publication(
            publication_guard,
            tenant,
            &doc_publication_intent(ioa_source),
        )
        .expect("arm Doc publication");
    persist_doc_generation(platform, tenant, ioa_source)
        .await
        .expect("publish durable Doc generation");
    let mut cutover = server
        .prepare_key_index_contracts_for_spec_activation(
            publication_guard,
            tenant,
            &[("Doc", ioa_source)],
        )
        .await
        .expect("prepare Doc contract");
    server
        .registry
        .write()
        .expect("registry lock")
        .try_register_tenant_with_reactions_constraints_and_key_epochs(
            tenant.as_str(),
            parse_csdl(CSDL_XML).expect("CSDL parse"),
            CSDL_XML.to_string(),
            &[("Doc", ioa_source)],
            Vec::new(),
            None,
            false,
            &cutover.activation_epochs,
        )
        .expect("publish live Doc generation");
    server
        .finish_key_index_contract_activation(publication_guard, tenant, &mut cutover)
        .await
        .expect("finish Doc contract");
    server
        .complete_spec_publication_retry(publication_guard, tenant)
        .expect("complete Doc publication");
}

/// An action must carry the exact table/epoch it used for evaluation through
/// the durable append. Re-reading the live table at append time would stamp an
/// old-A transition with the new A epoch after A -> none -> A and resurrect a
/// row purged by the intermediate empty contract.
#[tokio::test]
async fn delayed_actor_action_cannot_borrow_the_reactivated_a_epoch() {
    let (_guard, _clock, _ids) = install_deterministic_context(298);
    let sim = SimEventStore::no_faults(298);
    let events = BoxedEventStore::new(sim.clone());
    let mut original_table = TransitionTable::from_ioa_source(DOC_IOA);
    let signature_a = declared_key_set_signature(&original_table.keys);
    let old_epoch = events
        .activate_key_index_contract("default", "Doc", &signature_a, false)
        .await
        .expect("activate original A contract");
    events
        .mark_key_index_backfilled("default", "Doc", &signature_a)
        .await
        .expect("publish original A readiness");
    original_table.key_contract_activation_epoch = old_epoch;
    let table = Arc::new(RwLock::new(original_table));
    let actor = ActorSystem::new("arn238-actor-activation-epoch").spawn(
        EntityActor::with_persistence(
            "Doc",
            "actor-epoch-owner",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "actor-epoch-owner",
    );
    assert!(
        action(
            &actor,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/original"}),
        )
        .await
        .success
    );
    let persistence_id = "default:Doc:actor-epoch-owner";
    let journal_before = sim.dump_journal(persistence_id).len();
    let pause = sim.inject_precommit_append_pause(persistence_id);
    let action_future = action(
        &actor,
        "Rekey",
        serde_json::json!({"WorkspaceId": "ws", "Path": "/must-not-resurrect"}),
    );
    tokio::pin!(action_future);
    tokio::select! {
        response = &mut action_future => panic!("action crossed pre-commit barrier: {response:?}"),
        () = pause.wait_until_reached() => {}
    }

    let empty_table = TransitionTable::from_ioa_source(DOC_IOA_WITHOUT_KEYS);
    let signature_none = declared_key_set_signature(&empty_table.keys);
    let empty_epoch = events
        .activate_key_index_contract("default", "Doc", &signature_none, true)
        .await
        .expect("activate empty contract");
    let mut empty_table = empty_table;
    empty_table.key_contract_activation_epoch = empty_epoch;
    *table.write().expect("table lock") = empty_table;

    let mut reactivated_table = TransitionTable::from_ioa_source(DOC_IOA);
    let current_epoch = events
        .activate_key_index_contract("default", "Doc", &signature_a, false)
        .await
        .expect("reactivate A contract");
    assert!(current_epoch > old_epoch);
    reactivated_table.key_contract_activation_epoch = current_epoch;
    *table.write().expect("table lock") = reactivated_table;

    pause.resume();
    let response = action_future.await;
    assert!(
        !response.success,
        "stale evaluated action unexpectedly committed"
    );
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|error| error.contains("activation is stale")),
        "unexpected stale-action error: {:?}",
        response.error
    );
    assert_eq!(sim.dump_journal(persistence_id).len(), journal_before);
    assert_eq!(
        events
            .lookup_by_key(
                "default",
                "Doc",
                "path",
                &doc_key_hash("ws", "/must-not-resurrect"),
            )
            .await
            .expect("lookup rejected claim"),
        None
    );
}

/// A coverage watermark is tied to a monotonic key-contract revision, not only
/// the signature text. Cycling A -> no keys -> A must invalidate the original
/// A watermark, and a backfill that started before either live change cannot
/// publish after the corresponding revision has advanced.
#[tokio::test]
async fn key_contract_revision_fences_aba_spec_cycles() {
    let (_guard, _clock, _ids) = install_deterministic_context(243);
    let sim = SimEventStore::no_faults(243);
    let events = BoxedEventStore::new(sim);
    let keyed_table = TransitionTable::from_ioa_source(DOC_IOA);
    let signature_a = declared_key_set_signature(&keyed_table.keys);
    let table = Arc::new(RwLock::new(keyed_table));
    let system = ActorSystem::new("arn238-key-contract-aba");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "doc-aba",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "doc-aba",
    );
    assert!(
        action(
            &actor,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/a"}),
        )
        .await
        .success
    );

    events
        .mark_key_index_backfilled("default", "Doc", &signature_a)
        .await
        .expect("mark A coverage");
    let revision_a = events
        .key_index_reconciliation_revision("default", "Doc")
        .await
        .expect("read A revision");
    assert_eq!(
        events
            .key_index_backfilled_types("default")
            .await
            .expect("read A coverage"),
        vec![("Doc".to_string(), signature_a.clone())]
    );

    let signature_without_keys =
        declared_key_set_signature(&TransitionTable::from_ioa_source(DOC_IOA_WITHOUT_KEYS).keys);
    let stale_no_key_repair_revision = events
        .begin_key_index_backfill("default", "Doc", &signature_without_keys)
        .await
        .expect("begin no-key repair");
    assert!(stale_no_key_repair_revision > revision_a);
    let concurrent_a = update(
        &actor,
        serde_json::json!({"Path": "/a-during-no-key-repair"}),
        false,
    )
    .await;
    assert!(concurrent_a.success, "concurrent A write failed");
    let revision_after_concurrent_a = events
        .key_index_reconciliation_revision("default", "Doc")
        .await
        .expect("read post-race A revision");
    assert!(revision_after_concurrent_a > stale_no_key_repair_revision);
    assert!(
        !events
            .mark_key_index_backfilled_if_revision(
                "default",
                "Doc",
                &signature_without_keys,
                stale_no_key_repair_revision,
            )
            .await
            .expect("reject mixed-contract repair"),
        "a live A write during a no-key repair must fence publication"
    );

    let without_keys = TransitionTable::from_ioa_source(DOC_IOA_WITHOUT_KEYS);
    *table.write().expect("table lock") = without_keys;
    let no_key_write = update(&actor, serde_json::json!({"Path": "/while-unkeyed"}), false).await;
    assert!(no_key_write.success, "unkeyed write failed");
    let revision_without_keys = events
        .key_index_reconciliation_revision("default", "Doc")
        .await
        .expect("read no-key revision");
    assert!(revision_without_keys > revision_after_concurrent_a);
    assert!(
        events
            .key_index_backfilled_types("default")
            .await
            .expect("read invalidated coverage")
            .is_empty(),
        "a live no-key write must invalidate the A watermark"
    );
    assert_eq!(
        events
            .lookup_by_key(
                "default",
                "Doc",
                "path",
                &doc_key_hash("ws", "/a-during-no-key-repair"),
            )
            .await
            .expect("released live-write key lookup"),
        None,
        "a live write under the empty contract must release prior ownership"
    );
    assert!(
        !events
            .mark_key_index_backfilled_if_revision("default", "Doc", &signature_a, revision_a,)
            .await
            .expect("reject stale A backfill"),
        "a backfill captured under the first A contract must be fenced"
    );

    let restored_table = TransitionTable::from_ioa_source(DOC_IOA);
    assert_eq!(
        declared_key_set_signature(&restored_table.keys),
        signature_a,
        "the restored contract intentionally reuses the original signature"
    );
    *table.write().expect("table lock") = restored_table;
    assert!(
        events
            .key_index_backfilled_types("default")
            .await
            .expect("read pre-write restored coverage")
            .is_empty(),
        "restoring the same signature in memory must not resurrect coverage"
    );
    let restored = update(&actor, serde_json::json!({"Path": "/restored-a"}), false).await;
    assert!(restored.success, "restored A write failed");
    let restored_revision = events
        .key_index_reconciliation_revision("default", "Doc")
        .await
        .expect("read restored A revision");
    assert!(restored_revision > revision_without_keys);
    assert!(
        !events
            .mark_key_index_backfilled_if_revision(
                "default",
                "Doc",
                &signature_without_keys,
                revision_without_keys,
            )
            .await
            .expect("reject stale no-key backfill"),
        "a backfill captured before A was restored must be fenced"
    );
    assert!(
        events
            .key_index_backfilled_types("default")
            .await
            .expect("read final coverage")
            .is_empty(),
        "the A -> no-key -> A cycle requires a fresh successful backfill"
    );
    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws", "/restored-a"),)
            .await
            .expect("restored A key lookup"),
        Some("doc-aba".to_string())
    );
}

/// Contract activation must purge and fence A -> none -> A without relying on
/// either an entity write or the detached backfill between the two spec swaps.
/// Re-adding A must not resurrect its old watermark or prevent a new live owner
/// from claiming the released value.
#[tokio::test]
async fn empty_key_contract_backfill_fences_no_write_aba_and_releases_ownership() {
    let (_guard, _clock, _ids) = install_deterministic_context(295);
    let tenant = TenantId::default();
    let sim = SimEventStore::no_faults(295);
    let events = BoxedEventStore::new(sim);
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Doc", DOC_IOA)],
    );
    let mut server = ServerState::from_registry(
        ActorSystem::new("arn238-key-contract-no-write-aba"),
        registry,
    );
    server.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        events.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ));

    let initial_table = server
        .registry
        .read()
        .expect("registry lock")
        .get_table_live(&tenant, "Doc")
        .expect("Doc table");
    let original = server.actor_system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "doc-original-owner",
            initial_table,
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant(tenant.as_str()),
        "doc-original-owner",
    );
    assert!(
        action(
            &original,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/released"}),
        )
        .await
        .success
    );
    server.populate_key_index_from_snapshots(&tenant).await;
    let signature_a = declared_key_set_signature(&TransitionTable::from_ioa_source(DOC_IOA).keys);
    assert_eq!(
        events
            .key_index_backfilled_types(tenant.as_str())
            .await
            .expect("read initial watermark"),
        vec![("Doc".to_string(), signature_a.clone())]
    );

    let mut empty_publication = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire empty publication");
    server
        .arm_spec_publication(
            &mut empty_publication,
            &tenant,
            &doc_publication_intent(DOC_IOA_WITHOUT_KEYS),
        )
        .expect("arm empty publication");
    let mut empty_cutover = server
        .prepare_key_index_contracts_for_spec_activation(
            &empty_publication,
            &tenant,
            &[("Doc", DOC_IOA_WITHOUT_KEYS)],
        )
        .await
        .expect("prepare empty key contract");
    server
        .registry
        .write()
        .expect("registry lock")
        .try_register_tenant_with_reactions_constraints_and_key_epochs(
            tenant.as_str(),
            parse_csdl(CSDL_XML).expect("CSDL parse"),
            CSDL_XML.to_string(),
            &[("Doc", DOC_IOA_WITHOUT_KEYS)],
            Vec::new(),
            None,
            false,
            &empty_cutover.activation_epochs,
        )
        .expect("publish empty key contract");
    server
        .finish_key_index_contract_activation(&mut empty_publication, &tenant, &mut empty_cutover)
        .await
        .expect("finish empty key contract");
    server
        .complete_spec_publication_retry(&mut empty_publication, &tenant)
        .expect("complete empty publication");
    assert_eq!(
        events
            .lookup_by_key(
                tenant.as_str(),
                "Doc",
                "path",
                &doc_key_hash("ws", "/released"),
            )
            .await
            .expect("released key lookup"),
        None,
        "the empty contract must purge ownership without waiting for a live write"
    );
    assert_eq!(
        events
            .key_index_backfilled_types(tenant.as_str())
            .await
            .expect("read empty-contract watermark"),
        vec![(
            "Doc".to_string(),
            declared_key_set_signature(
                &TransitionTable::from_ioa_source(DOC_IOA_WITHOUT_KEYS).keys
            ),
        )],
        "the empty contract must become ready only after candidate repair"
    );

    drop(empty_publication);
    let mut restored_publication = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire restored publication");
    server
        .arm_spec_publication(
            &mut restored_publication,
            &tenant,
            &doc_publication_intent(DOC_IOA),
        )
        .expect("arm restored publication");
    let mut restored_cutover = server
        .prepare_key_index_contracts_for_spec_activation(
            &restored_publication,
            &tenant,
            &[("Doc", DOC_IOA)],
        )
        .await
        .expect("prepare restored A contract");
    server
        .registry
        .write()
        .expect("registry lock")
        .try_register_tenant_with_reactions_constraints_and_key_epochs(
            tenant.as_str(),
            parse_csdl(CSDL_XML).expect("CSDL parse"),
            CSDL_XML.to_string(),
            &[("Doc", DOC_IOA)],
            Vec::new(),
            None,
            false,
            &restored_cutover.activation_epochs,
        )
        .expect("publish restored A contract");
    server
        .finish_key_index_contract_activation(
            &mut restored_publication,
            &tenant,
            &mut restored_cutover,
        )
        .await
        .expect("finish restored A contract");
    server
        .complete_spec_publication_retry(&mut restored_publication, &tenant)
        .expect("complete restored publication");
    assert_eq!(
        events
            .key_index_backfilled_types(tenant.as_str())
            .await
            .expect("read pre-write restored watermark"),
        vec![("Doc".to_string(), signature_a)],
        "re-adding A must rebuild the original entity's ownership before opening writes"
    );
    let restored_table = server
        .registry
        .read()
        .expect("registry lock")
        .get_table_live(&tenant, "Doc")
        .expect("restored Doc table");
    let reclaimer = server.actor_system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "doc-new-owner",
            restored_table,
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant(tenant.as_str()),
        "doc-new-owner",
    );
    let reclaimed = action(
        &reclaimer,
        "Create",
        serde_json::json!({"WorkspaceId": "ws", "Path": "/released"}),
    )
    .await;
    assert!(!reclaimed.success, "duplicate claimant unexpectedly won");
    assert_eq!(
        events
            .lookup_by_key(
                tenant.as_str(),
                "Doc",
                "path",
                &doc_key_hash("ws", "/released"),
            )
            .await
            .expect("reclaimed key lookup"),
        Some("doc-original-owner".to_string())
    );
}

/// Re-activating the exact durable spec advances only the writer epoch. Its
/// retained key-coverage proof must be fenced by revision CAS, not rebuilt by an
/// O(all entities) startup replay.
#[tokio::test]
async fn unchanged_ready_activation_reuses_coverage_without_replaying_entities() {
    let (_guard, _clock, _ids) = install_deterministic_context(296);
    let tenant = TenantId::default();
    let sim = SimEventStore::no_faults(296);
    let events = BoxedEventStore::new(sim.clone());
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Doc", DOC_IOA)],
    );
    let mut server = ServerState::from_registry(
        ActorSystem::new("arn238-retained-activation-coverage"),
        registry,
    );
    server.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        events.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ));

    let table = server
        .registry
        .read()
        .expect("registry lock")
        .get_table_live(&tenant, "Doc")
        .expect("Doc table");
    let actor = server.actor_system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "doc-retained",
            table,
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant(tenant.as_str()),
        "doc-retained",
    );
    assert!(
        action(
            &actor,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/retained"}),
        )
        .await
        .success
    );
    server.populate_key_index_from_snapshots(&tenant).await;

    // The first activation establishes the fingerprint/epoch and legitimately
    // performs a repair. The second activation is byte-for-byte identical.
    server
        .activate_registered_key_contracts(&tenant)
        .await
        .expect("establish initial activated contract");
    let persistence_id = format!("{tenant}:Doc:doc-retained");
    sim.fail_next_reads(&persistence_id, 1);

    server
        .activate_registered_key_contracts(&tenant)
        .await
        .expect("unchanged activation reuses retained coverage");

    let read_error = events
        .read_events(&persistence_id, 0)
        .await
        .expect_err("activation should not consume the injected entity read fault");
    assert!(
        read_error.to_string().contains("injected read failure"),
        "unexpected retained read fault: {read_error}"
    );
}

/// Durable publication and runtime cutover are one per-tenant critical
/// section. A second publication cannot persist B while A still owns the
/// coordinator, so durable specs, the live registry, and the active key
/// contract finish on the same generation.
#[tokio::test]
async fn concurrent_publications_serialize_persistence_through_runtime_cutover() {
    use temper_server::platform_store::{PlatformStore, SimPlatformStore};

    let (_guard, _clock, _ids) = install_deterministic_context(297);
    let tenant = TenantId::default();
    let events = SimEventStore::no_faults(297);
    let platform = Arc::new(SimPlatformStore::no_faults(297));
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Doc", DOC_IOA)],
    );
    let mut server = ServerState::from_registry(
        ActorSystem::new("arn238-serialized-spec-publications"),
        registry,
    );
    server.set_storage_stack(StorageStack::from_sim(events, Some(Arc::clone(&platform))));

    let mut first_guard = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire first publication");
    let second_error = match server.begin_spec_publication(&tenant).await {
        Ok(_) => panic!("second publication crossed the first tenant guard"),
        Err(error) => error,
    };
    assert!(second_error.contains("runtime generation is busy"));
    assert!(
        PlatformStore::load_specs(platform.as_ref())
            .await
            .expect("load pre-publication specs")
            .is_empty(),
        "a waiting publication must not mutate durable specs"
    );

    publish_doc_generation(
        &server,
        platform.as_ref(),
        &mut first_guard,
        &tenant,
        DOC_IOA,
    )
    .await;
    drop(first_guard);
    let mut second_guard = server
        .begin_spec_publication(&tenant)
        .await
        .expect("retry second publication after first cutover");
    publish_doc_generation(
        &server,
        platform.as_ref(),
        &mut second_guard,
        &tenant,
        DOC_IOA_WITHOUT_KEYS,
    )
    .await;

    let durable = PlatformStore::load_specs(platform.as_ref())
        .await
        .expect("load final durable specs");
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0].ioa_source, DOC_IOA_WITHOUT_KEYS);
    let live_table = server
        .registry
        .read()
        .expect("registry lock")
        .get_table(&tenant, "Doc")
        .expect("live Doc table");
    assert!(live_table.keys.is_empty(), "live registry must finish on B");
    assert_eq!(
        server
            .storage_stack
            .as_ref()
            .expect("storage")
            .events
            .key_index_backfilled_types(tenant.as_str())
            .await
            .expect("final readiness"),
        vec![(
            "Doc".to_string(),
            declared_key_set_signature(
                &TransitionTable::from_ioa_source(DOC_IOA_WITHOUT_KEYS).keys
            )
        )],
        "active ownership readiness must finish on B"
    );
}

/// Tenant requests hold the read side of the publication barrier for their
/// entire handler. A publication fails with a bounded retry signal while an
/// entered request owns the generation, later requests fail admission while
/// the writer owns the generation, and an abandoned
/// publication remains explicitly gated until its exact retry completes.
#[tokio::test]
async fn tenant_request_generation_barrier_is_bounded_and_fail_closed() {
    let (_guard, _clock, _ids) = install_deterministic_context(300);
    let tenant = TenantId::default();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(CSDL_XML).expect("CSDL parse"),
        CSDL_XML.to_string(),
        &[("Doc", DOC_IOA)],
    );
    let server = ServerState::from_registry(
        ActorSystem::new("arn238-tenant-generation-barrier"),
        registry,
    );
    let request_generation = server.begin_tenant_request(&tenant).await;
    let busy_error = match server.begin_spec_publication(&tenant).await {
        Ok(_) => panic!("publication crossed an entered tenant request"),
        Err(error) => error,
    };
    assert!(busy_error.contains("runtime generation is busy"));
    drop(request_generation);

    let mut interrupted = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire publication after request exits");
    let intent = doc_publication_intent(DOC_IOA);
    server
        .arm_spec_publication(&mut interrupted, &tenant, &intent)
        .expect("arm interrupted publication");
    assert!(
        server.try_begin_tenant_request(&tenant).await.is_none(),
        "request admission must be nonblocking while publication owns the generation"
    );
    drop(interrupted);

    let sticky_request = server
        .try_begin_tenant_request(&tenant)
        .await
        .expect("writer guard unwound");
    assert!(
        server.spec_publication_gated(&tenant),
        "an outcome-ambiguous publication must still return retryable service-unavailable"
    );
    drop(sticky_request);

    let mut different = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire different-intent attempt");
    let different_error = server
        .arm_spec_publication(
            &mut different,
            &tenant,
            &doc_publication_intent(DOC_IOA_WITHOUT_KEYS),
        )
        .expect_err("a different generation cannot discharge sticky publication debt");
    assert!(different_error.contains("retry its exact runtime generation"));
    drop(different);
    assert!(server.spec_publication_gated(&tenant));

    let mut retry = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire exact retry");
    server
        .arm_spec_publication(&mut retry, &tenant, &intent)
        .expect("arm exact retry");
    let mut cutover = server
        .prepare_key_index_contracts_for_spec_activation(&retry, &tenant, &[("Doc", DOC_IOA)])
        .await
        .expect("prepare no-store cutover");
    server
        .finish_key_index_contract_activation(&mut retry, &tenant, &mut cutover)
        .await
        .expect("finish no-store cutover");
    server
        .complete_spec_publication_retry(&mut retry, &tenant)
        .expect("complete exact retry");
    assert!(!server.spec_publication_gated(&tenant));
    drop(retry);
    assert!(server.try_begin_tenant_request(&tenant).await.is_some());
}

/// Once durable publication starts, every error is outcome-ambiguous. A
/// committed generation whose activation fails, a definitely pre-commit retry
/// failure, and a commit whose acknowledgement is lost must all leave actor
/// spawn and writes fail-closed until one serialized retry finishes the exact
/// durable generation.
#[tokio::test]
async fn persist_first_activation_failures_remain_gated_until_successful_retry() {
    use temper_server::platform_store::{PlatformStore, SimPlatformStore};

    let (_guard, _clock, _ids) = install_deterministic_context(299);
    let tenant = TenantId::default();
    let sim = SimEventStore::no_faults(299);
    let platform = Arc::new(SimPlatformStore::no_faults(299));
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(CSDL_XML).expect("CSDL parse"),
        CSDL_XML.to_string(),
        &[("Doc", DOC_IOA)],
    );
    let mut server = ServerState::from_registry(
        ActorSystem::new("arn238-persist-first-activation-failure"),
        registry,
    );
    server.set_storage_stack(StorageStack::from_sim(
        sim.clone(),
        Some(Arc::clone(&platform)),
    ));

    let mut initial = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire initial publication");
    publish_doc_generation(&server, platform.as_ref(), &mut initial, &tenant, DOC_IOA).await;
    drop(initial);
    assert!(
        server
            .get_or_spawn_tenant_actor(&tenant, "Doc", "before-failure")
            .is_some(),
        "the completed initial generation must accept actor spawn"
    );

    let mut failed_activation = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire changed publication");
    server
        .arm_spec_publication(
            &mut failed_activation,
            &tenant,
            &doc_publication_intent(DOC_IOA_WITHOUT_KEYS),
        )
        .expect("arm changed generation");
    persist_doc_generation(platform.as_ref(), &tenant, DOC_IOA_WITHOUT_KEYS)
        .await
        .expect("commit changed durable generation");
    sim.fail_next_key_activations(1);
    let activation_error = server
        .prepare_key_index_contracts_for_spec_activation(
            &failed_activation,
            &tenant,
            &[("Doc", DOC_IOA_WITHOUT_KEYS)],
        )
        .await
        .expect_err("injected key-contract activation must fail");
    assert!(activation_error.contains("injected key activation failure"));
    drop(failed_activation);
    assert!(
        server
            .get_or_spawn_tenant_actor(&tenant, "Doc", "after-activation-failure")
            .is_none(),
        "post-commit activation failure must keep actor spawn gated"
    );

    let mut unrelated_retry = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire unrelated retry");
    let unrelated_error = server
        .arm_spec_publication(
            &mut unrelated_retry,
            &tenant,
            &unrelated_task_publication_intent(),
        )
        .expect_err("an unrelated partial publication cannot inherit Doc's gate");
    assert!(unrelated_error.contains("retry its exact runtime generation"));
    drop(unrelated_retry);
    assert!(
        server
            .get_or_spawn_tenant_actor(&tenant, "Doc", "after-unrelated-retry")
            .is_none(),
        "an unrelated Task merge must not discharge the unresolved Doc generation"
    );
    let durable_after_unrelated = PlatformStore::load_specs(platform.as_ref())
        .await
        .expect("load durable generation after rejected unrelated retry");
    assert_eq!(durable_after_unrelated.len(), 1);
    assert_eq!(durable_after_unrelated[0].ioa_source, DOC_IOA_WITHOUT_KEYS);

    let mut precommit_retry = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire pre-commit retry");
    server
        .arm_spec_publication(
            &mut precommit_retry,
            &tenant,
            &doc_publication_intent(DOC_IOA_WITHOUT_KEYS),
        )
        .expect("arm pre-commit retry");
    platform.fail_next_spec_publications(1);
    persist_doc_generation(platform.as_ref(), &tenant, DOC_IOA_WITHOUT_KEYS)
        .await
        .expect_err("inject pre-commit retry failure");
    drop(precommit_retry);
    assert!(
        server
            .get_or_spawn_tenant_actor(&tenant, "Doc", "after-precommit-failure")
            .is_none(),
        "a failed retry cannot clear an inherited unresolved gate"
    );

    let mut ambiguous_retry = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire ambiguous retry");
    server
        .arm_spec_publication(
            &mut ambiguous_retry,
            &tenant,
            &doc_publication_intent(DOC_IOA_WITHOUT_KEYS),
        )
        .expect("arm ambiguous retry");
    platform.fail_next_spec_publications_after_commit(1);
    let ambiguous_error = persist_doc_generation(platform.as_ref(), &tenant, DOC_IOA_WITHOUT_KEYS)
        .await
        .expect_err("inject lost commit acknowledgement");
    assert!(ambiguous_error.contains("post-commit publication failure"));
    drop(ambiguous_retry);
    assert!(
        server
            .get_or_spawn_tenant_actor(&tenant, "Doc", "after-ambiguous-failure")
            .is_none(),
        "an outcome-ambiguous commit must keep actor spawn gated"
    );
    let durable = PlatformStore::load_specs(platform.as_ref())
        .await
        .expect("load committed generation after ambiguous error");
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0].ioa_source, DOC_IOA_WITHOUT_KEYS);
    assert!(
        !server
            .registry
            .read()
            .expect("registry lock")
            .get_table(&tenant, "Doc")
            .expect("old live Doc table")
            .keys
            .is_empty(),
        "the live registry must remain on the old generation while gated"
    );

    let mut recovered = server
        .begin_spec_publication(&tenant)
        .await
        .expect("acquire successful retry");
    publish_doc_generation(
        &server,
        platform.as_ref(),
        &mut recovered,
        &tenant,
        DOC_IOA_WITHOUT_KEYS,
    )
    .await;
    drop(recovered);
    assert!(
        server
            .registry
            .read()
            .expect("registry lock")
            .get_table(&tenant, "Doc")
            .expect("recovered live Doc table")
            .keys
            .is_empty(),
        "successful retry must cut over the exact durable generation"
    );
    assert!(
        server
            .get_or_spawn_tenant_actor(&tenant, "Doc", "after-recovery")
            .is_some(),
        "successful activation and readiness publication must reopen actor spawn"
    );
}

/// Duplicate rows must be rejected before a publication mutates any durable
/// generation. Otherwise persistence can commit a last-wins row that runtime
/// activation rejects as structurally invalid.
#[tokio::test]
async fn duplicate_spec_publication_is_rejected_without_mutating_durable_generation() {
    use temper_server::platform_store::{
        PlatformStore, SimPlatformStore, SpecPublication, SpecPublicationMode,
        TenantConstraintsPublication, TenantPolicyPublication,
    };

    let platform = SimPlatformStore::no_faults(298);
    let original_hash = temper_store_turso::spec_content_hash(DOC_IOA);
    PlatformStore::publish_specs(
        &platform,
        "default",
        &[SpecPublication {
            entity_type: "Doc",
            ioa_source: DOC_IOA,
            csdl_xml: CSDL_XML,
            content_hash: &original_hash,
        }],
        SpecPublicationMode::Replace,
        TenantConstraintsPublication::Preserve,
        TenantPolicyPublication::Preserve,
        None,
        None,
        &[],
    )
    .await
    .expect("seed original generation");
    let replacement_hash = temper_store_turso::spec_content_hash(DOC_IOA_WITHOUT_KEYS);
    let error = PlatformStore::publish_specs(
        &platform,
        "default",
        &[
            SpecPublication {
                entity_type: "Doc",
                ioa_source: DOC_IOA,
                csdl_xml: CSDL_XML,
                content_hash: &original_hash,
            },
            SpecPublication {
                entity_type: "Doc",
                ioa_source: DOC_IOA_WITHOUT_KEYS,
                csdl_xml: CSDL_XML,
                content_hash: &replacement_hash,
            },
        ],
        SpecPublicationMode::Replace,
        TenantConstraintsPublication::Preserve,
        TenantPolicyPublication::Preserve,
        None,
        None,
        &[],
    )
    .await
    .expect_err("duplicate publication must fail preflight");
    assert!(error.contains("duplicate entity type Doc"));
    let durable = PlatformStore::load_specs(&platform)
        .await
        .expect("load durable generation after rejection");
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0].ioa_source, DOC_IOA);
}
