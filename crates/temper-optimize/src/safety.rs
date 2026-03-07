//! Safety invariant checker for optimization recommendations.
//!
//! Before any optimization is applied to the running system, it must pass
//! through the safety checker. This ensures Tier 3 optimizations remain
//! provably transparent to correctness:
//!
//! - `Risk::None` -- always safe; purely performance-oriented.
//! - `Risk::Low` -- safe only when the estimated improvement exceeds a
//!   minimum threshold (10%), ensuring the risk is worth taking.
//! - `Risk::Medium` -- requires shadow testing; never auto-approved.

use crate::types::{OptimizationRecommendation, Risk};

/// Validates that optimization recommendations are safe to apply.
#[derive(Debug, Default)]
pub struct SafetyChecker;

/// Result of a safety validation check.
#[derive(Debug, Clone)]
pub struct SafetyResult {
    /// Whether the recommendation is safe to apply.
    pub is_safe: bool,
    /// Human-readable explanation of the decision.
    pub reason: String,
}

impl SafetyChecker {
    /// Validate whether an optimization recommendation is safe to apply.
    ///
    /// # Rules
    ///
    /// - **`Risk::None`** -- Always safe. These optimizations are purely
    ///   additive (e.g., cache warming) and cannot affect correctness.
    ///
    /// - **`Risk::Low`** -- Safe only when `estimated_improvement > 0.1`.
    ///   Low-risk changes (e.g., TTL adjustments) are acceptable when the
    ///   expected benefit justifies the minor behavioural change.
    ///
    /// - **`Risk::Medium`** -- Always unsafe for auto-application. These
    ///   changes (e.g., shard rebalancing) require shadow testing before
    ///   they can be rolled out to production.
    pub fn validate(recommendation: &OptimizationRecommendation) -> SafetyResult {
        match recommendation.risk {
            Risk::None => SafetyResult {
                is_safe: true,
                reason: "No risk: optimization is purely additive and safe to apply.".to_string(),
            },
            Risk::Low => {
                if recommendation.estimated_improvement > 0.1 {
                    SafetyResult {
                        is_safe: true,
                        reason: format!(
                            "Low risk accepted: estimated improvement ({:.1}%) exceeds minimum threshold (10%).",
                            recommendation.estimated_improvement * 100.0
                        ),
                    }
                } else {
                    SafetyResult {
                        is_safe: false,
                        reason: format!(
                            "Low risk rejected: estimated improvement ({:.1}%) does not exceed minimum threshold (10%).",
                            recommendation.estimated_improvement * 100.0
                        ),
                    }
                }
            }
            Risk::Medium => SafetyResult {
                is_safe: false,
                reason: "Medium risk: requires shadow testing before production application."
                    .to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{OptAction, OptCategory, OptimizationRecommendation, Risk};

    fn make_rec(risk: Risk, improvement: f64) -> OptimizationRecommendation {
        OptimizationRecommendation {
            optimizer: "test".to_string(),
            description: "test".to_string(),
            category: OptCategory::CachePolicy,
            estimated_improvement: improvement,
            risk,
            action: OptAction::UpdateCacheTtl {
                key_pattern: "*".to_string(),
                ttl_seconds: 60,
            },
        }
    }

    #[test]
    fn no_risk_always_safe() {
        let rec = make_rec(Risk::None, 0.0);
        let result = SafetyChecker::validate(&rec);
        assert!(result.is_safe);
    }

    #[test]
    fn low_risk_above_threshold() {
        let rec = make_rec(Risk::Low, 0.2);
        let result = SafetyChecker::validate(&rec);
        assert!(result.is_safe);
    }

    #[test]
    fn low_risk_below_threshold() {
        let rec = make_rec(Risk::Low, 0.05);
        let result = SafetyChecker::validate(&rec);
        assert!(!result.is_safe);
    }

    #[test]
    fn low_risk_at_threshold() {
        let rec = make_rec(Risk::Low, 0.1);
        let result = SafetyChecker::validate(&rec);
        assert!(!result.is_safe);
    }

    #[test]
    fn medium_risk_always_unsafe() {
        let rec = make_rec(Risk::Medium, 0.5);
        let result = SafetyChecker::validate(&rec);
        assert!(!result.is_safe);
    }
}
