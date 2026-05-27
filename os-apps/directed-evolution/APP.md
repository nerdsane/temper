# Directed Evolution

Directed Evolution is the control plane for evolving running apps as organisms.
It records the live state behind Mission Control: signals, pressures,
directions, episodes, generations, variants, evaluation stages, trials,
eliminations, selection, promotion, and lineage.

## When To Use

Use this app when real usage, observability, or agent-brain observation suggests
that an app should repair itself or grow new capability.

The app does not run Codex directly. It creates `WorkItem` entities. A local
TemperPaw worker claims those work items, runs Codex or another approved agent,
and writes structured results back through entity actions.

## Entity Groups

- **Organism and Lineage**: `Organism`, `OrganismVersion`, `LineageEdge`
- **Discovery**: `Signal`, `Pressure`, `Direction`
- **Episodes**: `Episode`, `Generation`, `Variant`, `Mutation`
- **Evaluation**: `AdaptationGoal`, `ViabilityConstraint`,
  `SelectionPressure`, `EvaluationStage`, `StageResult`,
  `MetricDefinition`, `Measurement`, `EliminationRule`, `ScoringRule`,
  `EvidenceArtifact`, `Trial`
- **Promotion and Autonomy**: `Promotion`, `AutonomyPolicy`
- **Execution Bridge**: `WorkItem`, `BrainRun`

## Flow

1. Signals are recorded from observability, app usage, simulated users, or brain
   observation.
2. A brain interprets signals into pressures.
3. A brain frames pressures into directions.
4. A human director or autonomy policy starts an episode.
5. Generations create variants through background Codex work items.
6. Variants pass through evaluation stages and trials.
7. Weak variants are eliminated with evidence.
8. A selector brain chooses a winner from stage results.
9. Promotion records the new organism version and lineage edge.

Mission Control should be able to explain the process by reading these entity
states alone.
