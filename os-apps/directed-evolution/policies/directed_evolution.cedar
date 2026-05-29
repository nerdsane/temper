// Directed Evolution development policy.
//
// v1 separates observation privileges from mutation privileges. Any caller may
// read Mission Control state, but state changes require a platform system
// principal, a worker brain, or a human/supervisor/codex agent acting through
// the Directed Evolution control plane. Promotion writes are represented as an
// explicit action/resource matrix rather than a blanket entity permit.

permit(principal is Admin, action, resource);
permit(principal is System, action, resource);

permit(principal, action in [Action::"read", Action::"list"], resource);

permit(
  principal is Agent,
  action == Action::"create",
  resource
) when {
  ["system", "worker", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"ActivateOrganism",
    Action::"RecordOrganismVersion",
    Action::"ArchiveOrganism"
  ],
  resource is Organism
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"MarkOrganismVersionParent",
    Action::"SupersedeOrganismVersion",
    Action::"RollbackOrganismVersion"
  ],
  resource is OrganismVersion
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [Action::"RecordLineageEdge", Action::"ArchiveLineageEdge"],
  resource is LineageEdge
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"RecordSignal",
    Action::"LinkSignalToPressure",
    Action::"FailSignalObservation",
    Action::"IgnoreSignal",
    Action::"ArchiveSignal"
  ],
  resource is Signal
) when {
  ["system", "worker", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"InferPressure",
    Action::"FramePressureAsDirection",
    Action::"DismissPressure",
    Action::"ArchivePressure"
  ],
  resource is Pressure
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"ProposeDirection",
    Action::"SelectDirection",
    Action::"DismissDirection",
    Action::"ArchiveDirection"
  ],
  resource is Direction
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"BeginEpisodeNegotiation",
    Action::"RecordEpisodeContract",
    Action::"PlanEpisode",
    Action::"StartEpisode",
    Action::"AddGeneration",
    Action::"PauseEpisode",
    Action::"ResumeEpisode",
    Action::"BeginEpisodeSelection",
    Action::"RecordEpisodeWinner",
    Action::"CompleteEpisode",
    Action::"StopEpisode",
    Action::"FailEpisode"
  ],
  resource is Episode
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"StartGeneration",
    Action::"RecordGeneratedVariant",
    Action::"RecordGenerationSurvivor",
    Action::"BeginGenerationSelection",
    Action::"CompleteGeneration",
    Action::"FailGeneration"
  ],
  resource is Generation
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"RecordVariantMutation",
    Action::"StartVariantBuild",
    Action::"MarkVariantActive",
    Action::"RecordVariantStageResult",
    Action::"EliminateVariant",
    Action::"SelectVariant",
    Action::"RecordVariantNotSelected",
    Action::"PromoteVariant",
    Action::"FailVariant"
  ],
  resource is Variant
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"RecordMutation",
    Action::"RecordMutationWithPatch",
    Action::"ArchiveMutation"
  ],
  resource is Mutation
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [Action::"ActivateAdaptationGoal", Action::"ArchiveAdaptationGoal"],
  resource is AdaptationGoal
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"ActivateViabilityConstraint",
    Action::"PinViabilityConstraint",
    Action::"ArchiveViabilityConstraint"
  ],
  resource is ViabilityConstraint
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [Action::"ActivateSelectionPressure", Action::"ArchiveSelectionPressure"],
  resource is SelectionPressure
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"ActivateEvaluationStage",
    Action::"ActivateEvaluationStageWithEvaluator",
    Action::"ArchiveEvaluationStage"
  ],
  resource is EvaluationStage
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"PlanStageResult",
    Action::"StartStageResult",
    Action::"PassStageResult",
    Action::"FailStageResult",
    Action::"EliminateStageResult"
  ],
  resource is StageResult
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"ActivateMetricDefinition",
    Action::"ActivateMetricDefinitionWithProvenance",
    Action::"ArchiveMetricDefinition"
  ],
  resource is MetricDefinition
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [Action::"RecordMeasurement", Action::"ArchiveMeasurement"],
  resource is Measurement
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [Action::"ActivateEliminationRule", Action::"ArchiveEliminationRule"],
  resource is EliminationRule
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [Action::"ActivateScoringRule", Action::"ArchiveScoringRule"],
  resource is ScoringRule
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"RecordEvidenceArtifact",
    Action::"RecordEvidenceSummary",
    Action::"LinkEvidenceArtifact",
    Action::"ArchiveEvidenceArtifact"
  ],
  resource is EvidenceArtifact
) when {
  ["system", "worker", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"ActivateSelectionProtocol",
    Action::"FreezeSelectionProtocol",
    Action::"ArchiveSelectionProtocol"
  ],
  resource is SelectionProtocol
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"ActivateSimulatedUserPlan",
    Action::"FreezeSimulatedUserPlan",
    Action::"ArchiveSimulatedUserPlan"
  ],
  resource is SimulatedUserPlan
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"StartTrial",
    Action::"SucceedTrial",
    Action::"FailTrial",
    Action::"ArchiveTrial"
  ],
  resource is Trial
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"PromoteWinner",
    Action::"RecordPromotionMaterialization",
    Action::"FailPromotionMaterialization",
    Action::"RollbackPromotion",
    Action::"FailPromotion"
  ],
  resource is Promotion
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"ActivateAutonomyPolicy",
    Action::"UpdateAutonomyPolicy",
    Action::"RetireAutonomyPolicy"
  ],
  resource is AutonomyPolicy
) when {
  ["system", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"QueueWorkItem",
    Action::"ClaimWorkItem",
    Action::"StartWorkItem",
    Action::"SucceedWorkItem",
    Action::"FailWorkItem",
    Action::"CancelWorkItem"
  ],
  resource is WorkItem
) when {
  ["system", "worker", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action in [
    Action::"StartBrainRun",
    Action::"SucceedBrainRun",
    Action::"FailBrainRun",
    Action::"CancelBrainRun"
  ],
  resource is BrainRun
) when {
  ["system", "worker", "codex", "supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal is Agent,
  action == Action::"http_call",
  resource is HttpEndpoint
) when {
  ["signal_observer", "episode_orchestrator", "work_item_result_router"].contains(context.module) &&
  ["127.0.0.1", "localhost"].contains(context.domain)
};
