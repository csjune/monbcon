//! Other crates build results through constructors, since the result types
//! are `#[non_exhaustive]`.

use monbcon::{ApplyError, ApplyOutcome, ApplyReport, MonitorId, MonitorSnapshot, RefreshResult};

#[test]
fn refresh_results_can_be_built_outside_the_crate() {
    let snapshot = MonitorSnapshot::new(MonitorId::new("ddc:test"), "Test Monitor", 40);
    let result = RefreshResult::new(3, vec![snapshot], vec!["warning".into()]);

    assert_eq!(result.generation, 3);
    assert_eq!(result.snapshots[0].id.as_str(), "ddc:test");
    assert_eq!(result.snapshots[0].name, "Test Monitor");
    assert_eq!(result.snapshots[0].brightness, 40);
    assert_eq!(result.warnings, ["warning"]);
}

#[test]
fn apply_reports_can_be_built_outside_the_crate() {
    let failed = ApplyOutcome::new(
        3,
        MonitorId::new("ddc:test"),
        60,
        Some(40),
        Some(ApplyError::Backend("write failed".into())),
    );
    let report = ApplyReport::new(vec![failed]);

    let outcome = &report.outcomes[0];
    assert_eq!(outcome.requested, 60);
    assert_eq!(outcome.effective, Some(40));
    assert_eq!(
        outcome.error,
        Some(ApplyError::Backend("write failed".into()))
    );
}
