//! Deterministic ordering for bundle lint findings.

use super::{BundleLintFinding, LintSeverity};

pub(super) fn sort_bundle_findings(findings: &mut [BundleLintFinding]) {
    findings.sort_by(|left, right| {
        let left_key = (
            &left.entity,
            matches!(left.severity, LintSeverity::Warning),
            &left.code,
            &left.message,
        );
        let right_key = (
            &right.entity,
            matches!(right.severity, LintSeverity::Warning),
            &right.code,
            &right.message,
        );
        left_key.cmp(&right_key)
    });
}
