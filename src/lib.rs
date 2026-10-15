#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

/// Identifies a monitor across refreshes.
///
/// The same monitor keeps its ID for as long as it stays connected the same
/// way, so IDs can be stored, for example to remember per-monitor settings.
/// The text is backend-specific and should be treated as opaque.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MonitorId(String);

impl MonitorId {
    /// Wraps a previously stored ID.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the ID as text, for example to store it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MonitorId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A monitor found by [`MonitorController::refresh`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MonitorSnapshot {
    /// The monitor's ID, used to address it in a [`BrightnessUpdate`].
    pub id: MonitorId,
    /// A human-readable name, such as the model name from the monitor.
    pub name: String,
    /// The brightness when the monitor was discovered, from 0 to 100.
    pub brightness: i32,
}

/// The monitors found by one [`MonitorController::refresh`].
#[derive(Debug)]
#[non_exhaustive]
pub struct RefreshResult {
    /// The generation this refresh started. Updates must carry it.
    pub generation: u64,
    /// Every monitor whose brightness can be controlled.
    pub snapshots: Vec<MonitorSnapshot>,
    /// Problems that didn't stop discovery, such as a monitor that couldn't
    /// be inspected or one discovery backend that failed.
    pub warnings: Vec<String>,
}

impl MonitorSnapshot {
    /// Creates a snapshot, for example to test code that consumes refresh
    /// results without real monitors.
    pub fn new(id: MonitorId, name: impl Into<String>, brightness: i32) -> Self {
        Self {
            id,
            name: name.into(),
            brightness,
        }
    }
}

impl RefreshResult {
    /// Creates a refresh result, for example to test code that consumes it
    /// without real monitors.
    pub fn new(generation: u64, snapshots: Vec<MonitorSnapshot>, warnings: Vec<String>) -> Self {
        Self {
            generation,
            snapshots,
            warnings,
        }
    }
}

/// A brightness change requested for one monitor.
#[derive(Clone, Debug)]
pub struct BrightnessUpdate {
    /// The [`RefreshResult::generation`] the update was made against.
    pub generation: u64,
    /// The monitor to change.
    pub id: MonitorId,
    /// The brightness to set, from 0 to 100. Values outside that range are
    /// clamped.
    pub value: i32,
}

/// What happened to one [`BrightnessUpdate`].
#[derive(Debug)]
#[non_exhaustive]
pub struct ApplyOutcome {
    /// The generation the update was made against.
    pub generation: u64,
    /// The monitor the update was for.
    pub id: MonitorId,
    /// The requested brightness after clamping to 0–100.
    pub requested: i32,
    /// The monitor's brightness afterwards: the requested value on success,
    /// or the last known value if the monitor rejected the change. `None`
    /// when the update was rejected before reaching any hardware.
    pub effective: Option<i32>,
    /// Why the update failed, if it did.
    pub error: Option<ApplyError>,
}

impl ApplyOutcome {
    /// Creates an outcome, for example to test code that consumes apply
    /// reports without real monitors.
    pub fn new(
        generation: u64,
        id: MonitorId,
        requested: i32,
        effective: Option<i32>,
        error: Option<ApplyError>,
    ) -> Self {
        Self {
            generation,
            id,
            requested,
            effective,
            error,
        }
    }
}

/// The result of [`MonitorController::apply`].
#[derive(Debug)]
#[non_exhaustive]
pub struct ApplyReport {
    /// One outcome per update, in the order the updates were given.
    pub outcomes: Vec<ApplyOutcome>,
}

impl ApplyReport {
    /// Creates a report, for example to test code that consumes it without
    /// real monitors.
    pub fn new(outcomes: Vec<ApplyOutcome>) -> Self {
        Self { outcomes }
    }
}

/// Why [`MonitorController::refresh`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum MonitorError {
    /// Every discovery backend of the platform failed.
    DiscoveryFailed(String),
}

impl std::fmt::Display for MonitorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DiscoveryFailed(details) => {
                write!(formatter, "monitor discovery failed: {details}")
            }
        }
    }
}

impl std::error::Error for MonitorError {}

/// Why a [`BrightnessUpdate`] failed.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ApplyError {
    /// The update was made against an older refresh. No hardware was touched.
    StaleGeneration {
        /// The generation the update carried.
        requested: u64,
        /// The generation of the latest refresh.
        current: u64,
    },
    /// No monitor of the current refresh has the update's ID. No hardware
    /// was touched.
    UnknownMonitor,
    /// The monitor rejected the change or couldn't be reached.
    Backend(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleGeneration { requested, current } => write!(
                formatter,
                "stale monitor generation {requested}; current generation is {current}"
            ),
            Self::UnknownMonitor => write!(formatter, "unknown monitor id"),
            Self::Backend(details) => write!(formatter, "{details}"),
        }
    }
}

impl std::error::Error for ApplyError {}

mod controller;
mod parallel;
#[cfg(any(windows, target_os = "linux"))]
mod scale;

#[cfg(windows)]
#[path = "windows/mod.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod platform;

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::controller::Discovery;

    pub(crate) type BackendError = std::convert::Infallible;

    pub(crate) fn discover() -> Discovery {
        let mut discovery = Discovery::default();
        discovery.backend::<()>(
            "monitors",
            Err("monitor brightness is only supported on Windows and Linux"),
        );
        discovery
    }
}

pub use controller::MonitorController;
