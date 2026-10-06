mod backlight;
mod ddc;
mod drm;
mod edid;
mod i2c;

use std::collections::HashSet;
use std::fmt;
use std::io;

use self::backlight::BacklightMonitor;
use self::ddc::DdcMonitor;
use super::apply::{self, AdjustableMonitor};
use super::{ApplyReport, BrightnessUpdate, MonitorId, MonitorSnapshot, RefreshResult};

#[derive(Debug)]
pub enum MonitorError {
    Io {
        context: String,
        source: io::Error,
    },
    Ddc {
        context: &'static str,
        details: String,
    },
    Backlight {
        context: &'static str,
        details: String,
    },
    InvalidData {
        context: &'static str,
        details: String,
    },
    StaleGeneration {
        requested: u64,
        current: u64,
    },
    UnknownMonitor(MonitorId),
}

impl MonitorError {
    pub(super) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

impl fmt::Display for MonitorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(formatter, "{context}: {source}"),
            Self::Ddc { context, details } => write!(formatter, "{context}: {details}"),
            Self::Backlight { context, details } => write!(formatter, "{context}: {details}"),
            Self::InvalidData { context, details } => write!(formatter, "{context}: {details}"),
            Self::StaleGeneration { requested, current } => write!(
                formatter,
                "stale monitor generation {requested}; current generation is {current}"
            ),
            Self::UnknownMonitor(id) => write!(formatter, "unknown monitor id {id}"),
        }
    }
}

impl std::error::Error for MonitorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

enum Monitor {
    Ddc(DdcMonitor),
    Backlight(BacklightMonitor),
}

impl Monitor {
    fn name(&self) -> &str {
        match self {
            Self::Ddc(monitor) => monitor.name(),
            Self::Backlight(monitor) => monitor.name(),
        }
    }
}

impl AdjustableMonitor for Monitor {
    fn id(&self) -> &MonitorId {
        match self {
            Self::Ddc(monitor) => monitor.id(),
            Self::Backlight(monitor) => monitor.id(),
        }
    }

    fn brightness(&self) -> i32 {
        match self {
            Self::Ddc(monitor) => monitor.brightness(),
            Self::Backlight(monitor) => monitor.brightness(),
        }
    }

    fn set_brightness(&mut self, percent: i32) -> Result<(), MonitorError> {
        match self {
            Self::Ddc(monitor) => monitor.set_brightness(percent),
            Self::Backlight(monitor) => monitor.set_brightness(percent),
        }
    }
}

pub struct MonitorController {
    monitors: Vec<Monitor>,
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

    pub fn refresh(&mut self) -> Result<RefreshResult, MonitorError> {
        let mut warnings = Vec::new();
        let connectors = match drm::connectors() {
            Ok(connectors) => connectors,
            Err(error) => {
                warnings.push(format!("failed to query DRM connectors: {error}"));
                drm::Connectors::default()
            }
        };

        // Release the previous generation's device handles before probing the
        // same buses again.
        self.monitors.clear();

        let mut monitors = Vec::new();
        let mut successful_backends = 0;
        let mut backend_errors = Vec::new();
        let mut claimed = Vec::new();

        match ddc::discover(&connectors) {
            Ok(discovery) => {
                successful_backends += 1;
                warnings.extend(discovery.warnings);
                claimed = discovery.claimed;
                monitors.extend(discovery.monitors.into_iter().map(Monitor::Ddc));
            }
            Err(error) => {
                let error = format!("failed to refresh DDC monitors: {error}");
                warnings.push(error.clone());
                backend_errors.push(error);
            }
        }

        let ddc_monitor_ids = monitors
            .iter()
            .map(|monitor| monitor.id().clone())
            .collect::<HashSet<_>>();
        match backlight::discover(&connectors, &ddc_monitor_ids) {
            Ok(discovery) => {
                successful_backends += 1;
                warnings.extend(discovery.warnings);
                monitors.extend(discovery.monitors.into_iter().map(Monitor::Backlight));
            }
            Err(error) => {
                let error = format!("failed to refresh backlight devices: {error}");
                warnings.push(error.clone());
                backend_errors.push(error);
            }
        }

        for claimed in claimed {
            if !monitors.iter().any(|monitor| monitor.id() == &claimed.id) {
                warnings.push(format!(
                    "couldn't control {} on i2c-{}: its DDC/CI address is owned by another \
                     kernel driver",
                    claimed.name, claimed.bus
                ));
            }
        }

        if successful_backends == 0 {
            return Err(MonitorError::InvalidData {
                context: "monitor discovery failed",
                details: backend_errors.join("; "),
            });
        }

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
        apply::apply_updates(&mut self.monitors, self.generation, updates)
    }
}

pub(super) fn raw_to_percent(value: u32, min: u32, max: u32) -> i32 {
    if max <= min {
        return 100;
    }
    (((value.saturating_sub(min)) as f64 / (max - min) as f64) * 100.0)
        .round()
        .clamp(0.0, 100.0) as i32
}

pub(super) fn percent_to_raw(percent: i32, min: u32, max: u32) -> u32 {
    let range = u64::from(max.saturating_sub(min));
    min + ((percent.clamp(0, 100) as u64 * range + 50) / 100) as u32
}

#[cfg(test)]
mod tests {
    use super::{percent_to_raw, raw_to_percent};

    #[test]
    fn brightness_conversion_respects_ranges() {
        assert_eq!(raw_to_percent(0, 0, 100), 0);
        assert_eq!(raw_to_percent(37, 0, 100), 37);
        assert_eq!(raw_to_percent(1, 1, 19200), 0);
        assert_eq!(raw_to_percent(19200, 1, 19200), 100);
        assert_eq!(raw_to_percent(5, 5, 5), 100);
        assert_eq!(percent_to_raw(0, 1, 19200), 1);
        assert_eq!(percent_to_raw(50, 0, 100), 50);
        assert_eq!(percent_to_raw(100, 1, 19200), 19200);
        assert_eq!(percent_to_raw(100, 0, u32::MAX), u32::MAX);
    }
}
