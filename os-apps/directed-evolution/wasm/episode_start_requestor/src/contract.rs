#[derive(Clone)]
struct EpisodeStartContract {
    direction_id: String,
    organism_id: String,
    parent_version_id: String,
    autonomy_lane: String,
    requested_by: String,
    started_by: String,
    adaptation_goal: String,
    human_notes: String,
    reason: String,
    selection_statement: String,
    proposed_constraints_json: String,
    contract_json: String,
    metrics: Vec<MetricPlan>,
    constraints: Vec<ConstraintPlan>,
    elimination_rules: Vec<RulePlan>,
    scoring_rules: Vec<ScoringRulePlan>,
    stages: Vec<StagePlan>,
}

impl EpisodeStartContract {
    fn with_defaults(mut self) -> Self {
        if self.metrics.is_empty() {
            self.metrics = default_metrics();
        }
        if self.constraints.is_empty() {
            self.constraints = constraints_from_proposed(&self.proposed_constraints_json);
        }
        if self.constraints.is_empty() {
            self.constraints = default_constraints();
        }
        if self.elimination_rules.is_empty() {
            self.elimination_rules = default_elimination_rules();
        }
        if self.scoring_rules.is_empty() {
            self.scoring_rules = default_scoring_rules();
        }
        if self.stages.is_empty() {
            self.stages = default_stages();
        }
        ensure_required_stages(&mut self.stages);
        self
    }

    fn validate(&self) -> Result<(), String> {
        required(self.direction_id.clone(), "direction_id")?;
        required(self.organism_id.clone(), "organism_id")?;
        required(self.parent_version_id.clone(), "parent_version_id")?;
        required(self.adaptation_goal.clone(), "adaptation_goal")?;
        if self.metrics.is_empty() {
            return Err("episode start contract requires at least one metric".to_string());
        }
        if self.constraints.is_empty() {
            return Err("episode start contract requires at least one viability constraint".to_string());
        }
        if self.stages.is_empty() {
            return Err("episode start contract requires at least one evaluation stage".to_string());
        }
        Ok(())
    }
}

#[derive(Clone)]
struct MetricPlan {
    name: String,
    kind: String,
    unit: String,
    higher_is_better: String,
    description: String,
}

#[derive(Clone)]
struct ConstraintPlan {
    statement: String,
    kind: String,
}

#[derive(Clone)]
struct RulePlan {
    statement: String,
    metric_names: Vec<String>,
    threshold_json: String,
}

#[derive(Clone)]
struct ScoringRulePlan {
    statement: String,
    metric_names: Vec<String>,
    weight: String,
}

#[derive(Clone)]
struct StagePlan {
    name: String,
    kind: String,
    executor: String,
    required_evidence: Vec<String>,
}

#[derive(Clone)]
struct MetricIds {
    pairs: Vec<(String, String)>,
}

impl MetricIds {
    fn ids(&self) -> Vec<String> {
        self.pairs.iter().map(|(_, id)| id.clone()).collect()
    }

    fn ids_for_names(&self, names: &[String]) -> Vec<String> {
        if names.is_empty() {
            return self.ids();
        }
        let mut ids = Vec::new();
        for name in names {
            if let Some((_, id)) = self
                .pairs
                .iter()
                .find(|(metric_name, _)| metric_name == name)
            {
                ids.push(id.clone());
            } else {
                ids.push(name.clone());
            }
        }
        ids
    }
}

fn metric_plans(fields: &Value) -> Vec<MetricPlan> {
    array_from_json_field(fields, &["MetricsJson"])
        .iter()
        .filter_map(|item| {
            let name = lookup_string_deep(item, &["name", "metric_name", "MetricName"]);
            (!name.trim().is_empty()).then(|| MetricPlan {
                name,
                kind: nonempty(
                    lookup_string_deep(item, &["kind", "metric_kind", "MetricKind"]),
                    "outcome".to_string(),
                ),
                unit: nonempty(lookup_string_deep(item, &["unit", "Unit"]), "score".to_string()),
                higher_is_better: nonempty(
                    lookup_string_deep(item, &["higher_is_better", "HigherIsBetter"]),
                    "true".to_string(),
                ),
                description: nonempty(
                    lookup_string_deep(item, &["description", "Description"]),
                    "Human/brain negotiated Directed Evolution metric.".to_string(),
                ),
            })
        })
        .collect()
}

fn constraint_plans(fields: &Value, direction_fields: &Value) -> Vec<ConstraintPlan> {
    let mut constraints = constraints_from_json(&field_str(fields, &["ViabilityConstraintsJson"]));
    if constraints.is_empty() {
        constraints = constraints_from_proposed(&field_str(
            direction_fields,
            &["ProposedViabilityConstraintsJson"],
        ));
    }
    constraints
}

fn constraints_from_json(raw: &str) -> Vec<ConstraintPlan> {
    parse_json_array(raw)
        .iter()
        .filter_map(|item| {
            if let Some(statement) = item.as_str() {
                return Some(ConstraintPlan {
                    statement: statement.to_string(),
                    kind: "viability".to_string(),
                });
            }
            let statement = lookup_string_deep(item, &["statement", "ConstraintStatement"]);
            (!statement.trim().is_empty()).then(|| ConstraintPlan {
                statement,
                kind: nonempty(
                    lookup_string_deep(item, &["kind", "ConstraintKind"]),
                    "viability".to_string(),
                ),
            })
        })
        .collect()
}

fn constraints_from_proposed(raw: &str) -> Vec<ConstraintPlan> {
    constraints_from_json(raw)
}

fn elimination_rule_plans(fields: &Value) -> Vec<RulePlan> {
    array_from_json_field(fields, &["EliminationRulesJson"])
        .iter()
        .filter_map(|item| {
            let statement = lookup_string_deep(item, &["statement", "RuleStatement"]);
            (!statement.trim().is_empty()).then(|| RulePlan {
                statement,
                metric_names: string_array_from_value(lookup_value_deep(
                    item,
                    &["metric_names", "metric_ids", "MetricIds"],
                )),
                threshold_json: json_value_string(
                    lookup_value_deep(item, &["threshold", "threshold_json", "ThresholdJson"]),
                    json!({}),
                ),
            })
        })
        .collect()
}

fn scoring_rule_plans(fields: &Value) -> Vec<ScoringRulePlan> {
    array_from_json_field(fields, &["ScoringRulesJson"])
        .iter()
        .filter_map(|item| {
            let statement = lookup_string_deep(item, &["statement", "RuleStatement"]);
            (!statement.trim().is_empty()).then(|| ScoringRulePlan {
                statement,
                metric_names: string_array_from_value(lookup_value_deep(
                    item,
                    &["metric_names", "metric_ids", "MetricIds"],
                )),
                weight: nonempty(lookup_string_deep(item, &["weight", "Weight"]), "1.0".to_string()),
            })
        })
        .collect()
}

fn evaluation_stage_plans(fields: &Value) -> Vec<StagePlan> {
    array_from_json_field(fields, &["EvaluationStagesJson"])
        .iter()
        .filter_map(|item| {
            let name = lookup_string_deep(item, &["name", "stage_name", "StageName"]);
            (!name.trim().is_empty()).then(|| StagePlan {
                name,
                kind: nonempty(
                    lookup_string_deep(item, &["kind", "stage_kind", "StageKind"]),
                    "reviewer".to_string(),
                ),
                executor: nonempty(
                    lookup_string_deep(item, &["executor", "executor_kind", "ExecutorKind"]),
                    "codex".to_string(),
                ),
                required_evidence: string_array_from_value(lookup_value_deep(
                    item,
                    &["required_evidence", "RequiredEvidence"],
                )),
            })
        })
        .collect()
}

fn default_metrics() -> Vec<MetricPlan> {
    vec![
        MetricPlan {
            name: "baseline_regression_count".to_string(),
            kind: "regression".to_string(),
            unit: "count".to_string(),
            higher_is_better: "false".to_string(),
            description: "Number of baseline Agent Answers behaviors regressed by the variant.".to_string(),
        },
        MetricPlan {
            name: "simulated_user_goal_success".to_string(),
            kind: "simulated_user".to_string(),
            unit: "boolean".to_string(),
            higher_is_better: "true".to_string(),
            description: "Whether AI simulated users observe the Adaptation Goal succeeding.".to_string(),
        },
    ]
}

fn default_constraints() -> Vec<ConstraintPlan> {
    vec![
        ConstraintPlan {
            statement: "Existing Question and Answer lifecycle actions must keep working.".to_string(),
            kind: "regression".to_string(),
        },
        ConstraintPlan {
            statement: "Variants must not modify evaluators, selection pressure, or viability constraints.".to_string(),
            kind: "evaluator-boundary".to_string(),
        },
    ]
}

fn default_elimination_rules() -> Vec<RulePlan> {
    vec![RulePlan {
        statement: "Eliminate variants that fail review, regress baseline behavior, or fail the AI simulated-user trial.".to_string(),
        metric_names: Vec::new(),
        threshold_json: json!({
            "baseline_regression_count": 0,
            "simulated_user_goal_success": true,
        })
        .to_string(),
    }]
}

fn default_scoring_rules() -> Vec<ScoringRulePlan> {
    vec![ScoringRulePlan {
        statement: "Prefer the variant that most strongly satisfies the Adaptation Goal without violating Viability Constraints.".to_string(),
        metric_names: Vec::new(),
        weight: "1.0".to_string(),
    }]
}

fn default_stages() -> Vec<StagePlan> {
    vec![
        StagePlan {
            name: "Code and spec review".to_string(),
            kind: "reviewer".to_string(),
            executor: "codex".to_string(),
            required_evidence: vec![
                "changed_files".to_string(),
                "verification_notes".to_string(),
            ],
        },
        StagePlan {
            name: "AI simulated user trial".to_string(),
            kind: "simulated_user".to_string(),
            executor: "codex".to_string(),
            required_evidence: vec![
                "simulated_user_trace".to_string(),
                "datadog_evidence_scope".to_string(),
            ],
        },
    ]
}

fn ensure_required_stages(stages: &mut Vec<StagePlan>) {
    let has_review = stages
        .iter()
        .any(|stage| !stage.kind.to_ascii_lowercase().contains("simulated"));
    let has_simulated_user = stages
        .iter()
        .any(|stage| stage.kind.to_ascii_lowercase().contains("simulated"));
    if !has_review {
        stages.push(default_stages()[0].clone());
    }
    if !has_simulated_user {
        stages.push(default_stages()[1].clone());
    }
}

fn array_from_json_field(fields: &Value, keys: &[&str]) -> Vec<Value> {
    parse_json_array(&field_str(fields, keys))
}

fn parse_json_array(raw: &str) -> Vec<Value> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn string_array_from_value(value: Option<Value>) -> Vec<String> {
    value
        .and_then(|value| {
            value.as_array().map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        item.as_str()
                            .map(str::to_string)
                            .or_else(|| Some(item.to_string()))
                    })
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default()
}

fn json_value_string(value: Option<Value>, fallback: Value) -> String {
    value.unwrap_or(fallback).to_string()
}

fn human_notes_with_contract(human_notes: &str, contract_json: &str) -> String {
    if contract_json.trim().is_empty() {
        return human_notes.to_string();
    }
    if human_notes.trim().is_empty() {
        return format!("ContractJson: {contract_json}");
    }
    format!("{human_notes}\nContractJson: {contract_json}")
}

fn required(value: String, name: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err(format!("{name} is required"))
    } else {
        Ok(value)
    }
}

fn nonempty(value: String, fallback: String) -> String {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stages_preserve_review_and_simulated_user() {
        let mut stages = vec![StagePlan {
            name: "Custom review".to_string(),
            kind: "reviewer".to_string(),
            executor: "codex".to_string(),
            required_evidence: Vec::new(),
        }];
        ensure_required_stages(&mut stages);
        assert!(stages.iter().any(|stage| stage.kind == "reviewer"));
        assert!(
            stages
                .iter()
                .any(|stage| stage.kind.to_ascii_lowercase().contains("simulated"))
        );
    }

    #[test]
    fn rule_metric_names_map_to_created_ids() {
        let metric_ids = MetricIds {
            pairs: vec![("clarity".to_string(), "metric-1".to_string())],
        };
        assert_eq!(
            metric_ids.ids_for_names(&["clarity".to_string()]),
            vec!["metric-1".to_string()]
        );
    }
}
