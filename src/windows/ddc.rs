use std::collections::HashSet;
use std::mem;
use std::ptr;

use windows_sys::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetMonitorBrightness, GetMonitorCapabilities,
    GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR, MC_CAPS_BRIGHTNESS,
    PHYSICAL_MONITOR, SetMonitorBrightness,
};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, GetLastError, LPARAM, RECT,
};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::core::BOOL;

use super::display_config::ActiveDisplayPaths;
use super::{BackendError, last_win32_error, wide_to_string};
use crate::MonitorId;
use crate::controller::AdjustableMonitor;
use crate::parallel::map_parallel;
use crate::scale::{percent_to_raw, raw_to_percent};

pub(super) struct DdcDiscovery {
    pub(super) monitors: Vec<DdcMonitor>,
    pub(super) warnings: Vec<DdcWarning>,
}

pub(super) struct DdcWarning {
    pub(super) pnp_ids: Vec<String>,
    pub(super) message: String,
}

impl DdcWarning {
    fn inspection_failure(
        display_name: &str,
        pnp_ids: &[String],
        error: impl std::fmt::Display,
    ) -> Self {
        Self {
            pnp_ids: pnp_ids.to_vec(),
            message: format!("failed to inspect {display_name}: {error}"),
        }
    }

    pub(super) fn is_covered_by_wmi(&self, wmi_pnp_ids: &HashSet<String>) -> bool {
        covered_by_wmi(&self.pnp_ids, wmi_pnp_ids)
    }
}

/// Whether every display behind a GDI device is controlled through WMI.
fn covered_by_wmi(pnp_ids: &[String], wmi_pnp_ids: &HashSet<String>) -> bool {
    !pnp_ids.is_empty() && pnp_ids.iter().all(|id| wmi_pnp_ids.contains(id))
}

pub(super) struct DdcMonitor {
    id: MonitorId,
    name: String,
    brightness: i32,
    physical_monitor: PhysicalMonitorHandle,
    min: u32,
    max: u32,
}

impl AdjustableMonitor for DdcMonitor {
    type Error = BackendError;

    fn id(&self) -> &MonitorId {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn brightness(&self) -> i32 {
        self.brightness
    }

    fn set_brightness(&mut self, percent: i32) -> Result<(), BackendError> {
        let percent = percent.clamp(0, 100);
        let raw = percent_to_raw(percent, self.min, self.max);
        let ok = unsafe { SetMonitorBrightness(self.physical_monitor.handle(), raw) };

        if ok == 0 {
            return Err(last_win32_error("SetMonitorBrightness failed"));
        }

        self.brightness = percent;
        Ok(())
    }
}

struct PhysicalMonitorHandle {
    monitor: PHYSICAL_MONITOR,
}

impl PhysicalMonitorHandle {
    fn new(monitor: PHYSICAL_MONITOR) -> Self {
        Self { monitor }
    }

    fn handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.monitor.hPhysicalMonitor
    }

    fn description(&self) -> [u16; 128] {
        unsafe { ptr::addr_of!(self.monitor.szPhysicalMonitorDescription).read_unaligned() }
    }
}

impl Drop for PhysicalMonitorHandle {
    fn drop(&mut self) {
        unsafe {
            DestroyPhysicalMonitors(1, &self.monitor);
        }
    }
}

// The handle is only used by one thread at a time; the Monitor Configuration
// API doesn't tie it to the thread that opened it.
unsafe impl Send for PhysicalMonitorHandle {}

/// Stands in for a display whose GDI device name couldn't be read.
const UNKNOWN_DISPLAY: &str = "UNKNOWN-DISPLAY";

/// A physical monitor waiting to have its brightness read.
struct PhysicalProbe<'a> {
    /// The `HMONITOR` value, kept only to build IDs.
    hmonitor: usize,
    /// The GDI device name, such as `\\.\DISPLAY1`, if it could be read.
    display_name: Option<String>,
    pnp_ids: &'a [String],
    index: usize,
    monitor: PhysicalMonitorHandle,
}

/// Discovers DDC/CI monitors, skipping displays that are all controlled
/// through WMI.
pub(super) fn discover(
    paths: &ActiveDisplayPaths,
    wmi_pnp_ids: &HashSet<String>,
) -> Result<DdcDiscovery, BackendError> {
    let hmonitors = enumerate_display_monitors()?;
    let mut probes = Vec::new();
    let mut warnings = Vec::new();

    for hmonitor in hmonitors {
        match physical_monitors(hmonitor, paths, wmi_pnp_ids) {
            Ok(found) => probes.extend(found),
            Err(warning) => warnings.push(warning),
        }
    }

    // Each DDC/CI read blocks for about 100ms, so monitors are read at once.
    let mut monitors = Vec::new();
    let results = map_parallel(probes, |probe| {
        let display_name = probe.display_name.clone();
        let pnp_ids = probe.pnp_ids;
        build_monitor(probe).map_err(|error| {
            let display_name = display_name.as_deref().unwrap_or(UNKNOWN_DISPLAY);
            DdcWarning::inspection_failure(display_name, pnp_ids, error)
        })
    });
    for result in results {
        match result {
            Ok(Some(monitor)) => monitors.push(monitor),
            Ok(None) => {}
            Err(warning) => warnings.push(warning),
        }
    }

    Ok(DdcDiscovery { monitors, warnings })
}

fn enumerate_display_monitors() -> Result<Vec<HMONITOR>, BackendError> {
    let mut monitors = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(enum_monitor),
            &mut monitors as *mut Vec<HMONITOR> as LPARAM,
        )
    };

    if ok == 0 {
        Err(last_win32_error("EnumDisplayMonitors failed"))
    } else {
        Ok(monitors)
    }
}

fn physical_monitors<'a>(
    hmonitor: HMONITOR,
    paths: &'a ActiveDisplayPaths,
    wmi_pnp_ids: &HashSet<String>,
) -> Result<Vec<PhysicalProbe<'a>>, DdcWarning> {
    let display_name = display_name(hmonitor);
    let pnp_ids = display_name
        .as_deref()
        .map_or(&[][..], |name| paths.pnp_ids_for_gdi_name(name));
    if covered_by_wmi(pnp_ids, wmi_pnp_ids) {
        return Ok(Vec::new());
    }
    let label = display_name.as_deref().unwrap_or(UNKNOWN_DISPLAY);
    let mut count = 0;
    let ok = unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor, &mut count) };
    if ok == 0 {
        let error = last_win32_error("GetNumberOfPhysicalMonitorsFromHMONITOR failed");
        return Err(DdcWarning::inspection_failure(label, pnp_ids, error));
    }
    if count == 0 {
        return Ok(Vec::new());
    }

    let mut physical_monitors = vec![PHYSICAL_MONITOR::default(); count as usize];
    let ok =
        unsafe { GetPhysicalMonitorsFromHMONITOR(hmonitor, count, physical_monitors.as_mut_ptr()) };
    if ok == 0 {
        let error = last_win32_error("GetPhysicalMonitorsFromHMONITOR failed");
        return Err(DdcWarning::inspection_failure(label, pnp_ids, error));
    }

    Ok(physical_monitors
        .into_iter()
        .enumerate()
        .map(|(index, monitor)| PhysicalProbe {
            hmonitor: hmonitor as usize,
            display_name: display_name.clone(),
            pnp_ids,
            index,
            monitor: PhysicalMonitorHandle::new(monitor),
        })
        .collect())
}

fn build_monitor(probe: PhysicalProbe) -> Result<Option<DdcMonitor>, BackendError> {
    let PhysicalProbe {
        hmonitor,
        display_name,
        pnp_ids,
        index: physical_index,
        monitor: physical_monitor,
    } = probe;
    let mut min = 0;
    let mut current = 0;
    let mut max = 0;
    let ok = unsafe {
        GetMonitorBrightness(physical_monitor.handle(), &mut min, &mut current, &mut max)
    };

    if ok == 0 {
        let code = unsafe { GetLastError() };
        match classify_brightness_error(code) {
            BrightnessErrorKind::Unsupported => return Ok(None),
            BrightnessErrorKind::Failed => {}
        }
        // Fetching the capabilities string takes seconds, so only consult
        // it to tell a monitor without brightness control from a failure.
        if lacks_brightness_capability(&physical_monitor) {
            return Ok(None);
        }
        return Err(BackendError::Win32 {
            context: "GetMonitorBrightness failed",
            code,
        });
    }
    if max <= min {
        return Err(BackendError::InvalidData {
            context: "invalid DDC brightness range",
            details: format!("minimum {min}, maximum {max}"),
        });
    }

    let description = physical_monitor.description();
    let name = wide_to_string(&description)
        .or_else(|| display_name.clone())
        .unwrap_or_else(|| UNKNOWN_DISPLAY.into());
    let id = MonitorId::new(match (pnp_ids, display_name) {
        ([pnp_id], _) => format!("ddc:{pnp_id}:{physical_index}"),
        ([], Some(display_name)) => format!(
            "ddc-gdi:{}:{physical_index}:{}",
            display_name.to_ascii_uppercase(),
            name.to_ascii_uppercase()
        ),
        ([], None) => format!(
            "ddc-hmonitor:{hmonitor:X}:{physical_index}:{}",
            name.to_ascii_uppercase()
        ),
        (pnp_ids, _) => format!(
            "ddc-clone:{}:{physical_index}:{}",
            pnp_ids.join("|"),
            name.to_ascii_uppercase()
        ),
    });

    Ok(Some(DdcMonitor {
        id,
        name,
        brightness: raw_to_percent(current, min, max),
        physical_monitor,
        min,
        max,
    }))
}

fn lacks_brightness_capability(physical_monitor: &PhysicalMonitorHandle) -> bool {
    let mut capabilities = 0;
    let mut color_temperatures = 0;
    let ok = unsafe {
        GetMonitorCapabilities(
            physical_monitor.handle(),
            &mut capabilities,
            &mut color_temperatures,
        )
    };
    ok != 0 && capabilities & MC_CAPS_BRIGHTNESS == 0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrightnessErrorKind {
    Unsupported,
    Failed,
}

fn classify_brightness_error(code: u32) -> BrightnessErrorKind {
    if matches!(code, ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED) {
        BrightnessErrorKind::Unsupported
    } else {
        BrightnessErrorKind::Failed
    }
}

unsafe extern "system" fn enum_monitor(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let monitors = unsafe { &mut *(data as *mut Vec<HMONITOR>) };
    monitors.push(hmonitor);
    1
}

fn display_name(hmonitor: HMONITOR) -> Option<String> {
    unsafe {
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: mem::size_of::<MONITORINFOEXW>() as u32,
                rcMonitor: RECT::default(),
                rcWork: RECT::default(),
                dwFlags: 0,
            },
            szDevice: [0; 32],
        };

        let ok = GetMonitorInfoW(
            hmonitor,
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        );
        if ok == 0 {
            return None;
        }

        wide_to_string(&info.szDevice)
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::Foundation::{
        ERROR_GEN_FAILURE, ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED,
    };

    use std::collections::HashSet;

    use super::{BrightnessErrorKind, classify_brightness_error, covered_by_wmi};

    #[test]
    fn only_displays_fully_controlled_by_wmi_skip_ddc() {
        let wmi_pnp_ids = HashSet::from([r"DISPLAY\PANEL\1".to_string()]);
        let panel = [r"DISPLAY\PANEL\1".to_string()];
        let cloned = [
            r"DISPLAY\PANEL\1".to_string(),
            r"DISPLAY\DELL\2".to_string(),
        ];

        assert!(covered_by_wmi(&panel, &wmi_pnp_ids));
        assert!(!covered_by_wmi(&cloned, &wmi_pnp_ids));
        assert!(!covered_by_wmi(&[], &wmi_pnp_ids));
    }

    #[test]
    fn only_explicit_unsupported_errors_hide_a_monitor() {
        assert_eq!(
            classify_brightness_error(ERROR_INVALID_FUNCTION),
            BrightnessErrorKind::Unsupported
        );
        assert_eq!(
            classify_brightness_error(ERROR_NOT_SUPPORTED),
            BrightnessErrorKind::Unsupported
        );
        assert_eq!(
            classify_brightness_error(ERROR_GEN_FAILURE),
            BrightnessErrorKind::Failed
        );
    }
}
