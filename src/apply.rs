use super::platform::MonitorError;
use super::{ApplyOutcome, ApplyReport, BrightnessUpdate, MonitorId};

/// A monitor whose brightness a platform backend can change.
pub(crate) trait AdjustableMonitor {
    fn id(&self) -> &MonitorId;

    fn brightness(&self) -> i32;

    fn set_brightness(&mut self, percent: i32) -> Result<(), MonitorError>;
}

/// Applies brightness updates to the monitors of the current refresh
/// generation. Updates from an older generation, or for monitors that are no
/// longer present, are rejected without touching any hardware.
pub(crate) fn apply_updates<M: AdjustableMonitor>(
    monitors: &mut [M],
    generation: u64,
    updates: Vec<BrightnessUpdate>,
) -> ApplyReport {
    let outcomes = updates
        .into_iter()
        .map(|update| {
            let requested = update.value.clamp(0, 100);
            if update.generation != generation {
                return ApplyOutcome {
                    generation: update.generation,
                    id: update.id,
                    requested,
                    effective: None,
                    error: Some(
                        MonitorError::StaleGeneration {
                            requested: update.generation,
                            current: generation,
                        }
                        .to_string(),
                    ),
                };
            }
            let Some(monitor) = monitors
                .iter_mut()
                .find(|monitor| monitor.id() == &update.id)
            else {
                let error = MonitorError::UnknownMonitor(update.id.clone()).to_string();
                return ApplyOutcome {
                    generation: update.generation,
                    id: update.id,
                    requested,
                    effective: None,
                    error: Some(error),
                };
            };

            let previous = monitor.brightness();
            let error = monitor
                .set_brightness(requested)
                .err()
                .map(|error| error.to_string());

            ApplyOutcome {
                generation: update.generation,
                id: update.id,
                requested,
                effective: Some(if error.is_some() { previous } else { requested }),
                error,
            }
        })
        .collect();

    ApplyReport { outcomes }
}

#[cfg(test)]
mod tests {
    use super::{AdjustableMonitor, apply_updates};
    use crate::platform::MonitorError;
    use crate::{BrightnessUpdate, MonitorId};

    struct FakeMonitor {
        id: MonitorId,
        brightness: i32,
        fails: bool,
    }

    impl FakeMonitor {
        fn new(id: &str, brightness: i32, fails: bool) -> Self {
            Self {
                id: MonitorId::new(id),
                brightness,
                fails,
            }
        }
    }

    impl AdjustableMonitor for FakeMonitor {
        fn id(&self) -> &MonitorId {
            &self.id
        }

        fn brightness(&self) -> i32 {
            self.brightness
        }

        fn set_brightness(&mut self, percent: i32) -> Result<(), MonitorError> {
            if self.fails {
                return Err(MonitorError::InvalidData {
                    context: "write failed",
                    details: self.id.to_string(),
                });
            }
            self.brightness = percent;
            Ok(())
        }
    }

    fn update(generation: u64, id: &str, value: i32) -> BrightnessUpdate {
        BrightnessUpdate {
            generation,
            id: MonitorId::new(id),
            value,
        }
    }

    #[test]
    fn applies_clamped_values_to_current_monitors() {
        let mut monitors = [FakeMonitor::new("a", 20, false)];
        let report = apply_updates(&mut monitors, 3, vec![update(3, "a", 140)]);

        let outcome = &report.outcomes[0];
        assert_eq!(outcome.requested, 100);
        assert_eq!(outcome.effective, Some(100));
        assert!(outcome.error.is_none());
        assert_eq!(monitors[0].brightness, 100);
    }

    #[test]
    fn failed_writes_report_the_previous_brightness() {
        let mut monitors = [FakeMonitor::new("a", 20, true)];
        let report = apply_updates(&mut monitors, 3, vec![update(3, "a", 60)]);

        let outcome = &report.outcomes[0];
        assert_eq!(outcome.effective, Some(20));
        assert!(outcome.error.is_some());
    }

    #[test]
    fn stale_and_unknown_updates_leave_monitors_untouched() {
        let mut monitors = [FakeMonitor::new("a", 20, false)];
        let report = apply_updates(
            &mut monitors,
            3,
            vec![update(2, "a", 60), update(3, "missing", 60)],
        );

        assert!(
            report
                .outcomes
                .iter()
                .all(|outcome| outcome.effective.is_none() && outcome.error.is_some())
        );
        assert_eq!(monitors[0].brightness, 20);
    }
}
