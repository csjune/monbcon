use std::collections::{HashMap, HashSet};
use std::mem;
use std::ptr;

use windows_sys::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};

use super::{MonitorError, wide_to_string, win32_status};

#[derive(Default)]
pub(super) struct ActiveDisplayPaths {
    pnp_by_gdi_name: HashMap<String, Vec<String>>,
    pnp_ids: HashSet<String>,
    complete: bool,
    pub(super) warnings: Vec<String>,
}

impl ActiveDisplayPaths {
    pub(super) fn pnp_ids_for_gdi_name(&self, gdi_name: &str) -> &[String] {
        self.pnp_by_gdi_name
            .get(&normalize_gdi_name(gdi_name))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(super) fn contains_pnp_id(&self, pnp_id: &str) -> bool {
        self.pnp_ids.contains(pnp_id)
    }

    pub(super) fn is_complete(&self) -> bool {
        self.complete
    }

    fn add_path(&mut self, gdi_name: &str, pnp_id: String) {
        self.pnp_ids.insert(pnp_id.clone());
        let pnp_ids = self
            .pnp_by_gdi_name
            .entry(normalize_gdi_name(gdi_name))
            .or_default();
        pnp_ids.push(pnp_id);
        pnp_ids.sort_unstable();
        pnp_ids.dedup();
    }
}

pub(super) fn active_display_paths() -> Result<ActiveDisplayPaths, MonitorError> {
    let paths = query_active_paths()?;
    let mut result = ActiveDisplayPaths {
        complete: true,
        ..ActiveDisplayPaths::default()
    };

    for path in paths {
        match active_display_path(path) {
            Ok((gdi_name, pnp_id)) => {
                result.add_path(&gdi_name, pnp_id);
            }
            Err(error) => {
                result.complete = false;
                result
                    .warnings
                    .push(format!("failed to inspect active display path: {error}"));
            }
        }
    }

    Ok(result)
}

fn query_active_paths() -> Result<Vec<DISPLAYCONFIG_PATH_INFO>, MonitorError> {
    let flags = QDC_ONLY_ACTIVE_PATHS;
    let mut path_count = 0;
    let mut mode_count = 0;

    for _ in 0..3 {
        let status =
            unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
        if status != ERROR_SUCCESS {
            return Err(win32_status("GetDisplayConfigBufferSizes failed", status));
        }

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let status = unsafe {
            QueryDisplayConfig(
                flags,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                ptr::null_mut(),
            )
        };

        if status == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if status != ERROR_SUCCESS {
            return Err(win32_status("QueryDisplayConfig failed", status));
        }

        paths.truncate(path_count as usize);
        return Ok(paths);
    }

    Err(win32_status(
        "QueryDisplayConfig remained unstable",
        ERROR_INSUFFICIENT_BUFFER,
    ))
}

fn active_display_path(path: DISPLAYCONFIG_PATH_INFO) -> Result<(String, String), MonitorError> {
    let gdi_name = active_display_gdi_name(path)?;
    let pnp_id = active_display_pnp_id(path)?;
    Ok((gdi_name, pnp_id))
}

fn active_display_gdi_name(path: DISPLAYCONFIG_PATH_INFO) -> Result<String, MonitorError> {
    let mut source_name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };

    let status = unsafe {
        DisplayConfigGetDeviceInfo(&mut source_name.header as *mut DISPLAYCONFIG_DEVICE_INFO_HEADER)
    };
    if status != ERROR_SUCCESS as i32 {
        return Err(win32_status(
            "DisplayConfigGetDeviceInfo source query failed",
            status as u32,
        ));
    }

    wide_to_string(&source_name.viewGdiDeviceName).ok_or_else(|| MonitorError::InvalidData {
        context: "invalid active display source name",
        details: format!("source id {}", path.sourceInfo.id),
    })
}

fn active_display_pnp_id(path: DISPLAYCONFIG_PATH_INFO) -> Result<String, MonitorError> {
    let mut target_name = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };

    let status = unsafe {
        DisplayConfigGetDeviceInfo(&mut target_name.header as *mut DISPLAYCONFIG_DEVICE_INFO_HEADER)
    };
    if status != ERROR_SUCCESS as i32 {
        return Err(win32_status(
            "DisplayConfigGetDeviceInfo target query failed",
            status as u32,
        ));
    }

    wide_to_string(&target_name.monitorDevicePath)
        .and_then(|path| pnp_id_from_monitor_device_path(&path))
        .ok_or_else(|| MonitorError::InvalidData {
            context: "invalid active monitor device path",
            details: format!("target id {}", path.targetInfo.id),
        })
}

fn pnp_id_from_monitor_device_path(path: &str) -> Option<String> {
    let uppercase_path = path.to_ascii_uppercase();
    let marker = "DISPLAY#";
    let start = uppercase_path.find(marker)? + marker.len();
    let rest = &path[start..];
    let mut parts = rest.split('#');
    let hardware_id = parts.next()?.trim();
    let instance_id = parts.next()?.trim();

    if hardware_id.is_empty() || instance_id.is_empty() {
        return None;
    }

    Some(normalize_pnp_id(&format!(
        "DISPLAY\\{hardware_id}\\{instance_id}"
    )))
}

pub(super) fn normalize_pnp_id(value: &str) -> String {
    value.replace(['#', '/'], "\\").to_ascii_uppercase()
}

fn normalize_gdi_name(value: &str) -> String {
    value.trim().to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveDisplayPaths, normalize_gdi_name, normalize_pnp_id, pnp_id_from_monitor_device_path,
    };

    #[test]
    fn monitor_device_paths_become_normalized_pnp_ids() {
        assert_eq!(
            pnp_id_from_monitor_device_path(r"\\?\DISPLAY#DEL40A9#5&12345&0&UID4352#{guid}")
                .as_deref(),
            Some(r"DISPLAY\DEL40A9\5&12345&0&UID4352")
        );
        assert_eq!(
            normalize_pnp_id("display#abc/instance"),
            r"DISPLAY\ABC\INSTANCE"
        );
        assert_eq!(normalize_gdi_name(r"\\.\display1"), r"\\.\DISPLAY1");
    }

    #[test]
    fn cloned_display_sources_keep_every_target() {
        let mut paths = ActiveDisplayPaths::default();
        paths.add_path(r"\\.\DISPLAY1", r"DISPLAY\B\2".into());
        paths.add_path(r"\\.\display1", r"DISPLAY\A\1".into());
        paths.add_path(r"\\.\DISPLAY1", r"DISPLAY\B\2".into());
        let targets = paths.pnp_ids_for_gdi_name(r"\\.\display1");

        assert_eq!(targets, [r"DISPLAY\A\1", r"DISPLAY\B\2"]);
    }
}
