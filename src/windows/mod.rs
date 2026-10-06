mod ddc;
mod display_config;
mod wmi;

use std::collections::HashSet;
use std::fmt;

use windows_sys::Win32::Foundation::GetLastError;

use super::controller::Discovery;

#[derive(Debug)]
pub(crate) enum BackendError {
    Win32 {
        context: &'static str,
        code: u32,
    },
    Wmi {
        context: &'static str,
        details: String,
    },
    InvalidData {
        context: &'static str,
        details: String,
    },
}

impl BackendError {
    pub(super) fn wmi(context: &'static str, error: impl fmt::Display) -> Self {
        Self::Wmi {
            context,
            details: error.to_string(),
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Win32 { context, code } => {
                write!(formatter, "{context} (win32 error {code})")
            }
            Self::Wmi { context, details } => write!(formatter, "{context}: {details}"),
            Self::InvalidData { context, details } => write!(formatter, "{context}: {details}"),
        }
    }
}

impl std::error::Error for BackendError {}

pub(crate) fn discover() -> Discovery {
    let mut discovery = Discovery::default();
    let active_paths = match display_config::active_display_paths() {
        Ok(paths) => paths,
        Err(error) => {
            discovery.warn(format!("failed to query active display paths: {error}"));
            display_config::ActiveDisplayPaths::default()
        }
    };
    discovery.extend_warnings(active_paths.warnings.iter().cloned());

    let mut ddc_warnings = Vec::new();
    if let Some(ddc) = discovery.backend("DDC monitors", ddc::discover(&active_paths)) {
        ddc_warnings = ddc.warnings;
        for monitor in ddc.monitors {
            discovery.add(monitor);
        }
    }

    let mut wmi_pnp_ids = HashSet::new();
    let active_filter = active_paths.is_complete().then_some(&active_paths);
    if let Some(wmi) = discovery.backend("WMI monitors", wmi::discover(active_filter)) {
        discovery.extend_warnings(wmi.warnings);
        for monitor in wmi.monitors {
            wmi_pnp_ids.insert(monitor.pnp_id().to_string());
            discovery.add(monitor);
        }
    }

    discovery.extend_warnings(
        ddc_warnings
            .into_iter()
            .filter(|warning| !warning.is_covered_by_wmi(&wmi_pnp_ids))
            .map(|warning| warning.message),
    );

    discovery
}

pub(super) fn last_win32_error(context: &'static str) -> BackendError {
    BackendError::Win32 {
        context,
        code: unsafe { GetLastError() },
    }
}

pub(super) fn win32_status(context: &'static str, code: u32) -> BackendError {
    BackendError::Win32 { context, code }
}

pub(super) fn wide_to_string(buffer: &[u16]) -> Option<String> {
    let end = buffer.iter().position(|&character| character == 0)?;
    if end == 0 {
        return None;
    }

    Some(String::from_utf16_lossy(&buffer[..end]))
}
