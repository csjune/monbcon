use std::collections::HashSet;
use std::fmt;

use super::platform::{self, BackendError};
use super::{
    ApplyOutcome, ApplyReport, BrightnessUpdate, MonitorError, MonitorId, MonitorSnapshot,
    RefreshResult,
};

/// A monitor whose brightness a platform backend can read and change.
pub(crate) trait AdjustableMonitor {
    type Error: fmt::Display;

    fn id(&self) -> &MonitorId;

    fn name(&self) -> &str;

    fn brightness(&self) -> i32;

    fn set_brightness(&mut self, percent: i32) -> Result<(), Self::Error>;
}

impl<M: AdjustableMonitor + ?Sized> AdjustableMonitor for Box<M> {
    type Error = M::Error;

    fn id(&self) -> &MonitorId {
        (**self).id()
    }

    fn name(&self) -> &str {
        (**self).name()
    }

    fn brightness(&self) -> i32 {
        (**self).brightness()
    }

    fn set_brightness(&mut self, percent: i32) -> Result<(), Self::Error> {
        (**self).set_brightness(percent)
    }
}

type PlatformMonitor = Box<dyn AdjustableMonitor<Error = BackendError>>;

/// What a platform found while discovering monitors through one or more
/// backends.
#[derive(Default)]
pub(crate) struct Discovery {
    monitors: Vec<PlatformMonitor>,
    warnings: Vec<String>,
    backend_failures: Vec<String>,
    any_backend_succeeded: bool,
}

impl Discovery {
    /// Records the result of one discovery backend. A failed backend becomes
    /// a warning; discovery fails only if every backend fails.
    pub(crate) fn backend<T>(
        &mut self,
        name: &str,
        result: Result<T, impl fmt::Display>,
    ) -> Option<T> {
        match result {
            Ok(value) => {
                self.any_backend_succeeded = true;
                Some(value)
            }
            Err(error) => {
                let error = format!("failed to refresh {name}: {error}");
                self.warnings.push(error.clone());
                self.backend_failures.push(error);
                None
            }
        }
    }

    pub(crate) fn add(&mut self, monitor: impl AdjustableMonitor<Error = BackendError> + 'static) {
        self.monitors.push(Box::new(monitor));
    }

    pub(crate) fn monitor_ids(&self) -> impl Iterator<Item = &MonitorId> {
        self.monitors.iter().map(|monitor| monitor.id())
    }

    pub(crate) fn warn(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }

    pub(crate) fn extend_warnings(&mut self, warnings: impl IntoIterator<Item = String>) {
        self.warnings.extend(warnings);
    }
}

pub struct MonitorController {
    monitors: Vec<PlatformMonitor>,
    generation: u64,
}

impl Default for MonitorController {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorController {
    pub fn new() -> Self {
        Self {
            monitors: Vec::new(),
            generation: 0,
        }
    }

    /// Discovers the current monitors and starts a new generation. On
    /// failure the previous monitors and generation stay in place.
    pub fn refresh(&mut self) -> Result<RefreshResult, MonitorError> {
        let Discovery {
            mut monitors,
            mut warnings,
            backend_failures,
            any_backend_succeeded,
        } = platform::discover();
        if !any_backend_succeeded {
            return Err(MonitorError::DiscoveryFailed(backend_failures.join("; ")));
        }

        retain_unique_ids(&mut monitors, &mut warnings);
        self.monitors = monitors;
        self.generation = self.generation.wrapping_add(1).max(1);
        let snapshots = self
            .monitors
            .iter()
            .map(|monitor| MonitorSnapshot {
                id: monitor.id().clone(),
                name: monitor.name().to_string(),
                brightness: monitor.brightness(),
            })
            .collect();

        Ok(RefreshResult {
            generation: self.generation,
            snapshots,
            warnings,
        })
    }

    pub fn apply(&mut self, updates: Vec<BrightnessUpdate>) -> ApplyReport {
        apply_updates(&mut self.monitors, self.generation, updates)
    }
}

fn retain_unique_ids<M: AdjustableMonitor>(monitors: &mut Vec<M>, warnings: &mut Vec<String>) {
    let mut monitor_ids = HashSet::new();
    monitors.retain(|monitor| {
        if monitor_ids.insert(monitor.id().clone()) {
            true
        } else {
            warnings.push(format!(
                "ignored duplicate monitor id {} ({})",
                monitor.id(),
                monitor.name()
            ));
            false
        }
    });
}

/// Applies brightness updates to the monitors of the current refresh
/// generation. Updates from an older generation, or for monitors that are no
/// longer present, are rejected without touching any hardware.
fn apply_updates<M: AdjustableMonitor>(
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
    use super::{AdjustableMonitor, apply_updates, retain_unique_ids};
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
        type Error = String;

        fn id(&self) -> &MonitorId {
            &self.id
        }

        fn name(&self) -> &str {
            "Fake"
        }

        fn brightness(&self) -> i32 {
            self.brightness
        }

        fn set_brightness(&mut self, percent: i32) -> Result<(), String> {
            if self.fails {
                return Err(format!("write failed for {}", self.id));
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

    #[test]
    fn duplicate_ids_keep_the_first_monitor() {
        let mut monitors = vec![
            Box::new(FakeMonitor::new("a", 10, false)),
            Box::new(FakeMonitor::new("b", 20, false)),
            Box::new(FakeMonitor::new("a", 30, false)),
        ];
        let mut warnings = Vec::new();
        retain_unique_ids(&mut monitors, &mut warnings);

        let kept = monitors
            .iter()
            .map(|monitor| monitor.brightness())
            .collect::<Vec<_>>();
        assert_eq!(kept, [10, 20]);
        assert_eq!(warnings.len(), 1);
    }
}
