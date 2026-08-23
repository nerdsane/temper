// Least-privilege mutation grants for Directed Evolution's packaged WASM modules.
//
// The host derives both the exact Agent UID and the wasm_module role. Each
// module receives only the create targets and action/resource pairs exercised
// by its implementation. Read/list access comes from the app's shared policy.

permit(
  principal == Agent::"signal_observer",
  action == Action::"create",
  resource is WorkItem
) when {
  principal has role && principal.role == "wasm_module"
};

permit(
  principal == Agent::"signal_observer",
  action == Action::"QueueWorkItem",
  resource is WorkItem
) when {
  principal has role && principal.role == "wasm_module"
};

permit(
  principal == Agent::"episode_orchestrator",
  action == Action::"create",
  resource
) when {
  principal has role && principal.role == "wasm_module" &&
  (resource is Generation || resource is WorkItem)
};

permit(
  principal == Agent::"episode_orchestrator",
  action,
  resource
) when {
  principal has role && principal.role == "wasm_module" &&
  (
    (action == Action::"StartGeneration" && resource is Generation) ||
    (action == Action::"AddGeneration" && resource is Episode) ||
    (action == Action::"QueueWorkItem" && resource is WorkItem)
  )
};

permit(
  principal == Agent::"episode_start_requestor",
  action == Action::"create",
  resource
) when {
  principal has role && principal.role == "wasm_module" &&
  (
    resource is Episode ||
    resource is SelectionPressure ||
    resource is AdaptationGoal ||
    resource is MetricDefinition ||
    resource is ViabilityConstraint ||
    resource is EliminationRule ||
    resource is ScoringRule ||
    resource is EvaluationStage
  )
};

permit(
  principal == Agent::"episode_start_requestor",
  action,
  resource
) when {
  principal has role && principal.role == "wasm_module" &&
  (
    (
      action in [
        Action::"BeginEpisodeNegotiation",
        Action::"RecordEpisodeContract",
        Action::"StartEpisode"
      ] &&
      resource is Episode
    ) ||
    (action == Action::"SelectDirection" && resource is Direction) ||
    (
      action in [
        Action::"MarkEpisodeStartRequestStarted",
        Action::"FailEpisodeStartRequest"
      ] &&
      resource is EpisodeStartRequest
    ) ||
    (action == Action::"ActivateSelectionPressure" && resource is SelectionPressure) ||
    (action == Action::"ActivateAdaptationGoal" && resource is AdaptationGoal) ||
    (action == Action::"ActivateMetricDefinition" && resource is MetricDefinition) ||
    (action == Action::"ActivateViabilityConstraint" && resource is ViabilityConstraint) ||
    (action == Action::"ActivateEliminationRule" && resource is EliminationRule) ||
    (action == Action::"ActivateScoringRule" && resource is ScoringRule) ||
    (action == Action::"ActivateEvaluationStage" && resource is EvaluationStage)
  )
};

permit(
  principal == Agent::"work_item_result_router",
  action == Action::"create",
  resource
) when {
  principal has role && principal.role == "wasm_module" &&
  (
    resource is AdaptationGoal ||
    resource is Direction ||
    resource is EliminationRule ||
    resource is Episode ||
    resource is EvaluationStage ||
    resource is EvidenceArtifact ||
    resource is Generation ||
    resource is LineageEdge ||
    resource is Measurement ||
    resource is MetricDefinition ||
    resource is Mutation ||
    resource is OrganismVersion ||
    resource is Pressure ||
    resource is Promotion ||
    resource is ScoringRule ||
    resource is SelectionPressure ||
    resource is StageResult ||
    resource is Trial ||
    resource is Variant ||
    resource is ViabilityConstraint ||
    resource is WorkItem
  )
};

permit(
  principal == Agent::"work_item_result_router",
  action,
  resource
) when {
  principal has role && principal.role == "wasm_module" &&
  (
    (action == Action::"ActivateAdaptationGoal" && resource is AdaptationGoal) ||
    (
      action in [Action::"ProposeDirection", Action::"SelectDirection"] &&
      resource is Direction
    ) ||
    (action == Action::"ActivateEliminationRule" && resource is EliminationRule) ||
    (
      action in [
        Action::"AddGeneration",
        Action::"BeginEpisodeNegotiation",
        Action::"BeginEpisodeSelection",
        Action::"CompleteEpisode",
        Action::"FailEpisode",
        Action::"RecordEpisodeContract",
        Action::"RecordEpisodeWinner",
        Action::"StartEpisode"
      ] &&
      resource is Episode
    ) ||
    (action == Action::"ActivateEvaluationStage" && resource is EvaluationStage) ||
    (
      action in [Action::"LinkEvidenceArtifact", Action::"RecordEvidenceArtifact"] &&
      resource is EvidenceArtifact
    ) ||
    (
      action in [
        Action::"BeginGenerationSelection",
        Action::"CompleteGeneration",
        Action::"FailGeneration",
        Action::"RecordGeneratedVariant",
        Action::"RecordGenerationSurvivor",
        Action::"StartGeneration"
      ] &&
      resource is Generation
    ) ||
    (action == Action::"RecordLineageEdge" && resource is LineageEdge) ||
    (action == Action::"RecordMeasurement" && resource is Measurement) ||
    (action == Action::"ActivateMetricDefinition" && resource is MetricDefinition) ||
    (action == Action::"RecordMutation" && resource is Mutation) ||
    (
      action in [
        Action::"MarkOrganismVersionParent",
        Action::"SupersedeOrganismVersion"
      ] &&
      resource is OrganismVersion
    ) ||
    (action == Action::"RecordOrganismVersion" && resource is Organism) ||
    (
      action in [Action::"FramePressureAsDirection", Action::"InferPressure"] &&
      resource is Pressure
    ) ||
    (
      action in [
        Action::"PromoteWinner",
        Action::"RecordPromotionMaterialization",
        Action::"RecordPromotionMaterializationFailure"
      ] &&
      resource is Promotion
    ) ||
    (action == Action::"ActivateScoringRule" && resource is ScoringRule) ||
    (action == Action::"ActivateSelectionPressure" && resource is SelectionPressure) ||
    (
      action in [
        Action::"FailSignalObservation",
        Action::"IgnoreSignal",
        Action::"LinkSignalToPressure"
      ] &&
      resource is Signal
    ) ||
    (
      action in [
        Action::"EliminateStageResult",
        Action::"FailStageResult",
        Action::"PassStageResult",
        Action::"StartStageResult"
      ] &&
      resource is StageResult
    ) ||
    (
      action in [Action::"FailTrial", Action::"StartTrial", Action::"SucceedTrial"] &&
      resource is Trial
    ) ||
    (
      action in [
        Action::"EliminateVariant",
        Action::"FailVariant",
        Action::"MarkVariantActive",
        Action::"PromoteVariant",
        Action::"RecordVariantMutation",
        Action::"RecordVariantStageResult",
        Action::"SelectVariant"
      ] &&
      resource is Variant
    ) ||
    (action == Action::"ActivateViabilityConstraint" && resource is ViabilityConstraint) ||
    (action == Action::"QueueWorkItem" && resource is WorkItem)
  )
};
