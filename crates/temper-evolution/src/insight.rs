//! Product intelligence: compute insights from trajectory data.
//!
//! This module provides the logic for the trajectory analyzer sentinel:
//! detecting unmet intents, friction patterns, and workarounds from
//! production agent trajectories.

use crate::records::*;

/// Compute priority score for an insight based on volume, impact, and trend.
///
/// Formula: score = normalize(volume) * (1.0 - success_rate) * trend_multiplier
/// where trend_multiplier is 1.0 (stable), 1.2 (growing), 0.8 (declining)
pub fn compute_priority_score(signal: &InsightSignal) -> f64 {
    let volume_factor = (signal.volume as f64).log10().max(0.0) / 4.0; // normalize: 10000 -> 1.0
    let impact_factor = 1.0 - signal.success_rate;
    let trend_multiplier = match signal.trend.as_str() {
        "growing" => 1.2,
        "declining" => 0.8,
        _ => 1.0,
    };

    let raw_score = volume_factor * impact_factor * trend_multiplier;
    raw_score.clamp(0.0, 1.0)
}

/// Classify an insight based on the signal characteristics.
pub fn classify_insight(signal: &InsightSignal) -> InsightCategory {
    if signal.success_rate < 0.3 {
        // Most attempts fail → feature doesn't exist
        InsightCategory::UnmetIntent
    } else if signal.success_rate > 0.7 && signal.volume > 100 {
        // Succeeds but high volume suggests friction (agents doing extra work)
        InsightCategory::Friction
    } else {
        // Moderate success with patterns → likely workaround
        InsightCategory::Workaround
    }
}

/// Generate a product digest summary from ranked insights.
pub fn generate_digest(insights: &[InsightRecord]) -> String {
    let mut digest = String::new();
    digest.push_str("TEMPER PRODUCT INTELLIGENCE DIGEST\n");
    digest.push_str("===================================\n\n");

    let unmet: Vec<&InsightRecord> = insights.iter()
        .filter(|i| i.category == InsightCategory::UnmetIntent)
        .collect();

    let friction: Vec<&InsightRecord> = insights.iter()
        .filter(|i| i.category == InsightCategory::Friction)
        .collect();

    let workarounds: Vec<&InsightRecord> = insights.iter()
        .filter(|i| i.category == InsightCategory::Workaround)
        .collect();

    if !unmet.is_empty() {
        digest.push_str("UNMET INTENTS (users want this, system can't do it)\n\n");
        for (i, insight) in unmet.iter().enumerate() {
            digest.push_str(&format!(
                "  #{} {} — {} attempts, {:.0}% fail, trend: {}\n     → {}\n\n",
                i + 1,
                insight.signal.intent,
                insight.signal.volume,
                (1.0 - insight.signal.success_rate) * 100.0,
                insight.signal.trend,
                insight.recommendation,
            ));
        }
    }

    if !friction.is_empty() {
        digest.push_str("FRICTION (works but painful)\n\n");
        for (i, insight) in friction.iter().enumerate() {
            digest.push_str(&format!(
                "  #{} {} — {} trajectories\n     → {}\n\n",
                i + 1,
                insight.signal.intent,
                insight.signal.volume,
                insight.recommendation,
            ));
        }
    }

    if !workarounds.is_empty() {
        digest.push_str("WORKAROUNDS (agents hacking around gaps)\n\n");
        for (i, insight) in workarounds.iter().enumerate() {
            digest.push_str(&format!(
                "  #{} {} — {} trajectories\n     → {}\n\n",
                i + 1,
                insight.signal.intent,
                insight.signal.volume,
                insight.recommendation,
            ));
        }
    }

    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_score_high_volume_low_success() {
        let signal = InsightSignal {
            intent: "split order".into(),
            volume: 234,
            success_rate: 0.18,
            trend: "growing".into(),
            growth_rate: Some(0.12),
        };

        let score = compute_priority_score(&signal);
        assert!(score > 0.3, "high volume + low success should score high, got {score}");
    }

    #[test]
    fn test_priority_score_low_volume() {
        let signal = InsightSignal {
            intent: "rare thing".into(),
            volume: 5,
            success_rate: 0.1,
            trend: "stable".into(),
            growth_rate: None,
        };

        let score = compute_priority_score(&signal);
        assert!(score < 0.3, "low volume should score low, got {score}");
    }

    #[test]
    fn test_classify_unmet_intent() {
        let signal = InsightSignal {
            intent: "split order".into(),
            volume: 234,
            success_rate: 0.18,
            trend: "growing".into(),
            growth_rate: None,
        };
        assert_eq!(classify_insight(&signal), InsightCategory::UnmetIntent);
    }

    #[test]
    fn test_classify_friction() {
        let signal = InsightSignal {
            intent: "order history".into(),
            volume: 2341,
            success_rate: 0.85,
            trend: "stable".into(),
            growth_rate: None,
        };
        assert_eq!(classify_insight(&signal), InsightCategory::Friction);
    }

    #[test]
    fn test_classify_workaround() {
        let signal = InsightSignal {
            intent: "bulk update".into(),
            volume: 847,
            success_rate: 0.6,
            trend: "stable".into(),
            growth_rate: None,
        };
        assert_eq!(classify_insight(&signal), InsightCategory::Workaround);
    }

    #[test]
    fn test_generate_digest() {
        let insights = vec![
            InsightRecord {
                header: RecordHeader::new(RecordType::Insight, "test"),
                category: InsightCategory::UnmetIntent,
                signal: InsightSignal {
                    intent: "split order".into(),
                    volume: 234,
                    success_rate: 0.18,
                    trend: "growing".into(),
                    growth_rate: None,
                },
                recommendation: "Add SplitOrder action".into(),
                priority_score: 0.87,
            },
            InsightRecord {
                header: RecordHeader::new(RecordType::Insight, "test"),
                category: InsightCategory::Friction,
                signal: InsightSignal {
                    intent: "order history".into(),
                    volume: 2341,
                    success_rate: 0.85,
                    trend: "stable".into(),
                    growth_rate: None,
                },
                recommendation: "Add Customer→Orders NavigationProperty".into(),
                priority_score: 0.65,
            },
        ];

        let digest = generate_digest(&insights);
        assert!(digest.contains("UNMET INTENTS"));
        assert!(digest.contains("split order"));
        assert!(digest.contains("FRICTION"));
        assert!(digest.contains("order history"));
    }
}
