use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use wmi::{WMIConnection, WMIError};

use super::BackendError;
use super::display_config::{ActiveDisplayPaths, normalize_pnp_id};
use crate::MonitorId;
use crate::controller::AdjustableMonitor;

const WBEM_E_NOT_SUPPORTED: i32 = 0x8004_100C_u32 as i32;

pub(super) struct WmiMonitor {
    id: MonitorId,
    pnp_id: String,
    name: String,
    brightness: i32,
    instance_path: String,
}

impl WmiMonitor {
    pub(super) fn pnp_id(&self) -> &str {
        &self.pnp_id
    }
}

impl AdjustableMonitor for WmiMonitor {
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
        let connection = wmi_connection("ROOT\\WMI")?;
        let input = WmiSetBrightnessInput {
            Timeout: 0,
            Brightness: percent as u8,
        };
        let output: Option<WmiSetBrightnessOutput> = connection
            .exec_instance_method::<WmiMonitorBrightnessMethods, _>(
                &self.instance_path,
                "WmiSetBrightness",
                input,
            )
            .map_err(|error| BackendError::wmi("WmiSetBrightness failed", error))?;

        if let Some(code) = output.and_then(|output| failed_return_value(output.ReturnValue)) {
            return Err(BackendError::Win32 {
                context: "WmiSetBrightness failed",
                code,
            });
        }

        self.brightness = percent;
        Ok(())
    }
}

pub(super) struct WmiDiscovery {
    pub(super) monitors: Vec<WmiMonitor>,
    pub(super) warnings: Vec<String>,
}

pub(super) fn discover(
    active_paths: Option<&ActiveDisplayPaths>,
) -> Result<WmiDiscovery, BackendError> {
    let connection = wmi_connection("ROOT\\WMI")?;
    let brightness_monitors: Vec<WmiMonitorBrightness> = match connection.raw_query(
        "SELECT InstanceName, Active, CurrentBrightness FROM WmiMonitorBrightness WHERE Active = TRUE",
    ) {
        Ok(monitors) => monitors,
        // Systems without an integrated panel, such as desktops, have no
        // brightness provider and reject the query as unsupported.
        Err(error) if is_not_supported(&error) => {
            return Ok(WmiDiscovery {
                monitors: Vec::new(),
                warnings: Vec::new(),
            });
        }
        Err(error) => return Err(BackendError::wmi("WMI brightness query failed", error)),
    };
    let brightness_methods: Vec<WmiMonitorBrightnessMethodsInstance> = connection
        .raw_query(
            "SELECT InstanceName, Active, __PATH FROM WmiMonitorBrightnessMethods WHERE Active = TRUE",
        )
        .map_err(|error| BackendError::wmi("WMI brightness methods query failed", error))?;
    let method_paths: HashMap<String, String> = brightness_methods
        .into_iter()
        .filter(|monitor| monitor.Active)
        .map(|monitor| (monitor.InstanceName, monitor.path))
        .collect();
    let mut warnings = Vec::new();
    let friendly_names = match wmi_monitor_names(&connection) {
        Ok(names) => names,
        Err(error) => {
            warnings.push(format!("failed to query WMI monitor names: {error}"));
            HashMap::new()
        }
    };
    let mut candidates = Vec::new();

    for monitor in brightness_monitors
        .into_iter()
        .filter(|monitor| monitor.Active)
    {
        let Some(instance_path) = method_paths.get(&monitor.InstanceName).cloned() else {
            warnings.push(format!(
                "missing WMI brightness method for {}",
                monitor.InstanceName
            ));
            continue;
        };
        let Some(pnp_id) = wmi_instance_to_pnp_id(&monitor.InstanceName) else {
            warnings.push(format!(
                "invalid WMI monitor instance name {}",
                monitor.InstanceName
            ));
            continue;
        };
        if active_paths.is_some_and(|paths| !paths.contains_pnp_id(&pnp_id)) {
            continue;
        }
        candidates.push((monitor, pnp_id, instance_path));
    }

    let unnamed = candidates
        .iter()
        .filter(|(monitor, _, _)| !friendly_names.contains_key(&monitor.InstanceName))
        .map(|(_, pnp_id, _)| pnp_id.as_str());
    let pnp_names = match pnp_names_for(unnamed) {
        Ok(names) => names,
        Err(error) => {
            warnings.push(format!("failed to query PNP monitor names: {error}"));
            HashMap::new()
        }
    };

    let monitors = candidates
        .into_iter()
        .map(|(monitor, pnp_id, instance_path)| {
            let name = friendly_names
                .get(&monitor.InstanceName)
                .cloned()
                .or_else(|| pnp_names.get(&pnp_id).cloned())
                .unwrap_or_else(|| "Integrated Monitor".into());

            WmiMonitor {
                id: MonitorId::new(format!("wmi:{}", normalize_pnp_id(&monitor.InstanceName))),
                pnp_id,
                name,
                brightness: monitor.CurrentBrightness as i32,
                instance_path,
            }
        })
        .collect();

    Ok(WmiDiscovery { monitors, warnings })
}

fn wmi_monitor_names(connection: &WMIConnection) -> Result<HashMap<String, String>, BackendError> {
    let monitors = connection
        .raw_query::<WmiMonitorId>(
            "SELECT InstanceName, Active, UserFriendlyName FROM WmiMonitorID WHERE Active = TRUE",
        )
        .map_err(|error| BackendError::wmi("WMI monitor name query failed", error))?;

    Ok(monitors
        .into_iter()
        .filter(|monitor| monitor.Active)
        .filter_map(|monitor| {
            friendly_name_from_wmi(&monitor.UserFriendlyName)
                .map(|name| (monitor.InstanceName, name))
        })
        .collect())
}

/// Monitor names from `Win32_PnPEntity`, kept for the life of the process
/// because the query takes hundreds of milliseconds. `None` marks a monitor
/// the query didn't name, so it isn't queried for again.
static PNP_NAMES: Mutex<BTreeMap<String, Option<String>>> = Mutex::new(BTreeMap::new());

/// Looks up the PNP names of `pnp_ids`, querying only when one of them
/// hasn't been seen before.
fn pnp_names_for<'a>(
    pnp_ids: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<String, String>, BackendError> {
    let pnp_ids = pnp_ids.into_iter().collect::<Vec<_>>();
    let mut cache = PNP_NAMES.lock().unwrap_or_else(PoisonError::into_inner);
    if pnp_ids.iter().any(|pnp_id| !cache.contains_key(*pnp_id)) {
        let names = pnp_monitor_names()?;
        cache.extend(names.into_iter().map(|(pnp_id, name)| (pnp_id, Some(name))));
        for pnp_id in &pnp_ids {
            cache.entry(pnp_id.to_string()).or_insert(None);
        }
    }

    Ok(pnp_ids
        .into_iter()
        .filter_map(|pnp_id| Some((pnp_id.to_string(), cache.get(pnp_id)?.clone()?)))
        .collect())
}

fn pnp_monitor_names() -> Result<HashMap<String, String>, BackendError> {
    let connection = wmi_connection("ROOT\\CIMV2")?;
    let monitors = connection
        .raw_query::<Win32PnpEntity>(
            "SELECT Name, PNPDeviceID FROM Win32_PnPEntity WHERE PNPClass = 'Monitor'",
        )
        .map_err(|error| BackendError::wmi("PNP monitor name query failed", error))?;

    Ok(monitors
        .into_iter()
        .filter_map(|monitor| {
            Some((
                normalize_pnp_id(&monitor.PNPDeviceID?),
                monitor.Name?.trim().to_string(),
            ))
        })
        .filter(|(_, name)| !name.is_empty())
        .collect())
}

fn is_not_supported(error: &WMIError) -> bool {
    matches!(error, WMIError::HResultError { hres } if *hres == WBEM_E_NOT_SUPPORTED)
}

fn wmi_connection(namespace: &str) -> Result<WMIConnection, BackendError> {
    WMIConnection::with_namespace_path(namespace)
        .map_err(|error| BackendError::wmi("WMI connection failed", error))
}

fn friendly_name_from_wmi(name: &Option<Vec<u16>>) -> Option<String> {
    let name = name.as_ref()?;
    let end = name
        .iter()
        .position(|&character| character == 0)
        .unwrap_or(name.len());
    let value = String::from_utf16_lossy(&name[..end]).trim().to_string();

    if value.is_empty() { None } else { Some(value) }
}

fn wmi_instance_to_pnp_id(instance_name: &str) -> Option<String> {
    let base = instance_name
        .rsplit_once('_')
        .filter(|(_, suffix)| suffix.chars().all(|character| character.is_ascii_digit()))
        .map_or(instance_name, |(base, _)| base);

    if base.is_empty() {
        None
    } else {
        Some(normalize_pnp_id(base))
    }
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct WmiMonitorBrightness {
    InstanceName: String,
    Active: bool,
    CurrentBrightness: u8,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct WmiMonitorBrightnessMethodsInstance {
    InstanceName: String,
    Active: bool,
    #[serde(rename = "__PATH")]
    path: String,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct WmiMonitorId {
    InstanceName: String,
    Active: bool,
    UserFriendlyName: Option<Vec<u16>>,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct Win32PnpEntity {
    Name: Option<String>,
    PNPDeviceID: Option<String>,
}

#[derive(Deserialize)]
#[allow(non_camel_case_types)]
struct WmiMonitorBrightnessMethods;

#[derive(Serialize)]
#[allow(non_snake_case)]
struct WmiSetBrightnessInput {
    Timeout: u32,
    Brightness: u8,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct WmiSetBrightnessOutput {
    // Some integrated-display drivers omit this otherwise standard field.
    #[serde(default)]
    ReturnValue: Option<u32>,
}

fn failed_return_value(return_value: Option<u32>) -> Option<u32> {
    return_value.filter(|&code| code != 0)
}

#[cfg(test)]
mod tests {
    use wmi::WMIError;

    use super::{
        WBEM_E_NOT_SUPPORTED, failed_return_value, friendly_name_from_wmi, is_not_supported,
        wmi_instance_to_pnp_id,
    };

    #[test]
    fn only_unsupported_queries_mean_no_integrated_panel() {
        assert!(is_not_supported(&WMIError::HResultError {
            hres: WBEM_E_NOT_SUPPORTED
        }));
        assert!(!is_not_supported(&WMIError::HResultError {
            hres: 0x8004_1001_u32 as i32
        }));
        assert!(!is_not_supported(&WMIError::ResultEmpty));
    }

    #[test]
    fn missing_wmi_return_value_is_treated_as_success() {
        assert_eq!(failed_return_value(None), None);
        assert_eq!(failed_return_value(Some(0)), None);
        assert_eq!(failed_return_value(Some(1)), Some(1));
    }

    #[test]
    fn wmi_names_and_instance_ids_are_normalized() {
        assert_eq!(
            friendly_name_from_wmi(&Some(vec![68, 101, 108, 108, 0, 0])).as_deref(),
            Some("Dell")
        );
        assert_eq!(
            wmi_instance_to_pnp_id("DISPLAY\\BOE1234\\4&ABCD&0&UID265988_0").as_deref(),
            Some("DISPLAY\\BOE1234\\4&ABCD&0&UID265988")
        );
    }
}
