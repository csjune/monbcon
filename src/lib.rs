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

#[cfg(windows)]
#[path = "windows/mod.rs"]
mod platform;

#[cfg(not(windows))]
mod platform {
    use std::fmt;

    use super::{ApplyOutcome, ApplyReport, BrightnessUpdate, RefreshResult};

    #[derive(Debug)]
    pub struct MonitorError;

    impl fmt::Display for MonitorError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "monitor brightness is only supported on Windows")
        }
    }

    impl std::error::Error for MonitorError {}

    pub struct MonitorController;

    impl Default for MonitorController {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MonitorController {
        pub fn new() -> Self {
            Self
        }

        pub fn refresh(&mut self) -> Result<RefreshResult, MonitorError> {
            Ok(RefreshResult {
                generation: 0,
                snapshots: Vec::new(),
                warnings: Vec::new(),
            })
        }

        pub fn apply(&mut self, updates: Vec<BrightnessUpdate>) -> ApplyReport {
            ApplyReport {
                outcomes: updates
                    .into_iter()
                    .map(|update| ApplyOutcome {
                        generation: update.generation,
                        id: update.id,
                        requested: update.value,
                        effective: None,
                        error: Some(MonitorError.to_string()),
                    })
                    .collect(),
            }
        }
    }
}

pub use platform::{MonitorController, MonitorError};
