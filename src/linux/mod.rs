mod backlight;
mod ddc;
mod drm;
mod edid;
mod i2c;

use std::collections::HashSet;
use std::fmt;
use std::io;

use super::controller::Discovery;

#[derive(Debug)]
pub(crate) enum BackendError {
    Io {
        context: String,
        source: io::Error,
    },
    Device {
        context: &'static str,
        details: String,
    },
}

impl BackendError {
    pub(super) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(formatter, "{context}: {source}"),
            Self::Device { context, details } => write!(formatter, "{context}: {details}"),
        }
    }
}

impl std::error::Error for BackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Device { .. } => None,
        }
    }
}

pub(crate) fn discover() -> Discovery {
    let mut discovery = Discovery::default();
    let connectors = match drm::connectors() {
        Ok(connectors) => connectors,
        Err(error) => {
            discovery.warn(format!("failed to query DRM connectors: {error}"));
            drm::Connectors::default()
        }
    };

    let mut claimed = Vec::new();
    if let Some(ddc) = discovery.backend("DDC monitors", ddc::discover(&connectors)) {
        discovery.extend_warnings(ddc.warnings);
        claimed = ddc.claimed;
        for monitor in ddc.monitors {
            discovery.add(monitor);
        }
    }

    let ddc_monitor_ids = discovery.monitor_ids().cloned().collect::<HashSet<_>>();
    if let Some(backlight) = discovery.backend(
        "backlight devices",
        backlight::discover(&connectors, &ddc_monitor_ids),
    ) {
        discovery.extend_warnings(backlight.warnings);
        for monitor in backlight.monitors {
            discovery.add(monitor);
        }
    }

    for claimed in claimed {
        if !discovery.monitor_ids().any(|id| id == &claimed.id) {
            discovery.warn(format!(
                "couldn't control {} on i2c-{}: its DDC/CI address is owned by another \
                 kernel driver",
                claimed.name, claimed.bus
            ));
        }
    }

    discovery
}
