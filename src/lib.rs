#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MonitorId(String);

impl MonitorId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MonitorId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug)]
pub struct MonitorSnapshot {
    pub id: MonitorId,
    pub name: String,
    pub brightness: i32,
}

#[derive(Debug)]
pub struct RefreshResult {
    pub generation: u64,
    pub snapshots: Vec<MonitorSnapshot>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct BrightnessUpdate {
    pub generation: u64,
    pub id: MonitorId,
    pub value: i32,
}

#[derive(Debug)]
pub struct ApplyOutcome {
    pub generation: u64,
    pub id: MonitorId,
    pub requested: i32,
    pub effective: Option<i32>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct ApplyReport {
    pub outcomes: Vec<ApplyOutcome>,
}

#[derive(Debug)]
pub enum MonitorError {
    /// Every discovery backend of the platform failed.
    DiscoveryFailed(String),
    StaleGeneration {
        requested: u64,
        current: u64,
    },
    UnknownMonitor(MonitorId),
}

impl std::fmt::Display for MonitorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DiscoveryFailed(details) => {
                write!(formatter, "monitor discovery failed: {details}")
            }
            Self::StaleGeneration { requested, current } => write!(
                formatter,
                "stale monitor generation {requested}; current generation is {current}"
            ),
            Self::UnknownMonitor(id) => write!(formatter, "unknown monitor id {id}"),
        }
    }
}

impl std::error::Error for MonitorError {}

mod controller;
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
