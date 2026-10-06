use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::ddc;
use super::drm::{ConnectorMatch, Connectors};
use super::edid;
use super::{MonitorError, MonitorId, percent_to_raw, raw_to_percent};

const BACKLIGHT_CLASS_PATH: &str = "/sys/class/backlight";
const SUBSYSTEM: &str = "backlight";

pub(super) struct BacklightDiscovery {
    pub(super) monitors: Vec<BacklightMonitor>,
    pub(super) warnings: Vec<String>,
}

pub(super) struct BacklightMonitor {
    id: MonitorId,
    name: String,
    brightness: i32,
    device_name: String,
    path: PathBuf,
    min: u32,
    max: u32,
}

impl BacklightMonitor {
    fn new(
        id: MonitorId,
        name: String,
        device_name: String,
        path: PathBuf,
        values: &BacklightValues,
        min: u32,
    ) -> Self {
        Self {
            id,
            name,
            brightness: raw_to_percent(values.current.max(min), min, values.max),
            device_name,
            path,
            min,
            max: values.max,
        }
    }

    pub(super) fn id(&self) -> &MonitorId {
        &self.id
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn brightness(&self) -> i32 {
        self.brightness
    }

    pub(super) fn set_brightness(&mut self, percent: i32) -> Result<(), MonitorError> {
        let percent = percent.clamp(0, 100);
        let raw = percent_to_raw(percent, self.min, self.max);

        // logind lets the active session change the backlight without root.
        // Fall back to sysfs for setups where the user owns the device.
        if let Err(logind_error) = set_with_logind(&self.device_name, raw) {
            fs::write(self.path.join("brightness"), raw.to_string()).map_err(|sysfs_error| {
                MonitorError::Backlight {
                    context: "failed to set backlight brightness",
                    details: format!(
                        "{}: logind: {logind_error}; sysfs: {sysfs_error}",
                        self.device_name
                    ),
                }
            })?;
        }

        self.brightness = percent;
        Ok(())
    }
}

fn set_with_logind(device_name: &str, raw: u32) -> zbus::Result<()> {
    let connection = zbus::blocking::Connection::system()?;
    connection.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1/session/auto",
        Some("org.freedesktop.login1.Session"),
        "SetBrightness",
        &(SUBSYSTEM, device_name, raw),
    )?;
    Ok(())
}

/// Discovers internal panels and monitors exposed by the out-of-tree
/// ddcci-backlight driver. `ddc_monitor_ids` holds the monitors the DDC
/// backend already controls, so they are not listed twice.
pub(super) fn discover(
    connectors: &Connectors,
    ddc_monitor_ids: &HashSet<MonitorId>,
) -> Result<BacklightDiscovery, MonitorError> {
    let entries = match fs::read_dir(BACKLIGHT_CLASS_PATH) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(BacklightDiscovery {
                monitors: Vec::new(),
                warnings: Vec::new(),
            });
        }
        Err(error) => {
            return Err(MonitorError::io(
                format!("failed to read {BACKLIGHT_CLASS_PATH}"),
                error,
            ));
        }
    };

    let mut panels = Vec::new();
    let mut monitors = Vec::new();
    let mut warnings = Vec::new();
    for entry in entries.flatten() {
        let Some(device_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let path = entry.path();
        // ddcci-backlight can leave a dangling class link behind after its
        // I2C client is deleted; such a device no longer exists.
        let Ok(device_path) = fs::canonicalize(&path) else {
            continue;
        };
        let result = match i2c_bus_of(&device_path) {
            Some(bus) => external_monitor(&path, &device_name, bus, connectors, ddc_monitor_ids)
                .map(|monitor| monitors.extend(monitor)),
            None => read_values(&path).map(|values| {
                panels.push(Panel {
                    device_name: device_name.clone(),
                    path,
                    device_path,
                    values,
                });
            }),
        };
        if let Err(error) = result {
            warnings.push(format!(
                "failed to inspect backlight {device_name}: {error}"
            ));
        }
    }

    monitors.extend(internal_monitors(panels, connectors));
    Ok(BacklightDiscovery { monitors, warnings })
}

struct Panel {
    device_name: String,
    path: PathBuf,
    device_path: PathBuf,
    values: BacklightValues,
}

fn internal_monitors(mut panels: Vec<Panel>, connectors: &Connectors) -> Vec<BacklightMonitor> {
    // Laptops often expose the same panel through several interfaces; like
    // systemd, keep only the most preferred kind.
    let Some(best_priority) = panels.iter().map(|panel| panel.values.priority).min() else {
        return Vec::new();
    };
    panels.retain(|panel| panel.values.priority == best_priority);
    panels.sort_by(|left, right| left.device_name.cmp(&right.device_name));

    let panel_count = panels.len();
    panels
        .into_iter()
        .filter_map(|panel| {
            let connector_name = match connectors.match_device(&panel.device_path) {
                ConnectorMatch::Active(connector) => Some(connector.name.clone()),
                ConnectorMatch::Inactive => return None,
                ConnectorMatch::Unknown => None,
            };
            let name = match (panel_count, connector_name) {
                (1, _) => "Built-in Display".to_string(),
                (_, Some(connector_name)) => format!("Built-in Display ({connector_name})"),
                (_, None) => format!("Built-in Display ({})", panel.device_name),
            };
            // A raw value of 0 switches some panels off entirely.
            let min = u32::from(panel.values.max > 1);
            Some(BacklightMonitor::new(
                MonitorId::new(format!("backlight:{}", panel.device_name)),
                name,
                panel.device_name,
                panel.path,
                &panel.values,
                min,
            ))
        })
        .collect()
}

fn external_monitor(
    path: &Path,
    device_name: &str,
    bus: u32,
    connectors: &Connectors,
    ddc_monitor_ids: &HashSet<MonitorId>,
) -> Result<Option<BacklightMonitor>, MonitorError> {
    let edid_bytes = ddc::read_edid(bus)
        .map_err(|error| MonitorError::io(format!("failed to read EDID on i2c-{bus}"), error))?;
    let edid = edid::parse(&edid_bytes).ok_or_else(|| MonitorError::InvalidData {
        context: "invalid EDID",
        details: format!("i2c-{bus}"),
    })?;
    let connector_name = match connectors.match_edid(&edid_bytes) {
        ConnectorMatch::Active(connector) => Some(connector.name.as_str()),
        ConnectorMatch::Inactive => return Ok(None),
        ConnectorMatch::Unknown => None,
    };
    let id = ddc::monitor_id(&edid, bus, connector_name);
    if ddc_monitor_ids.contains(&id) {
        return Ok(None);
    }

    let values = read_values(path)?;
    Ok(Some(BacklightMonitor::new(
        id,
        edid.display_name(),
        device_name.to_string(),
        path.to_path_buf(),
        &values,
        0,
    )))
}

/// Returns the I2C bus a backlight device hangs off. Only external monitors
/// driven by ddcci-backlight live under an I2C adapter; panel backlights
/// belong to a GPU connector or the ACPI platform.
fn i2c_bus_of(device_path: &Path) -> Option<u32> {
    device_path.components().find_map(|component| {
        component
            .as_os_str()
            .to_str()?
            .strip_prefix("i2c-")?
            .parse()
            .ok()
    })
}

struct BacklightValues {
    priority: u8,
    current: u32,
    max: u32,
}

fn read_values(path: &Path) -> Result<BacklightValues, MonitorError> {
    let max = read_u32(&path.join("max_brightness"))?;
    if max == 0 {
        return Err(MonitorError::InvalidData {
            context: "invalid backlight range",
            details: "max_brightness is 0".into(),
        });
    }
    let current = read_u32(&path.join("actual_brightness"))
        .or_else(|_| read_u32(&path.join("brightness")))?;
    let kind = fs::read_to_string(path.join("type")).unwrap_or_default();

    Ok(BacklightValues {
        priority: type_priority(kind.trim()),
        current,
        max,
    })
}

fn type_priority(kind: &str) -> u8 {
    match kind {
        "firmware" => 0,
        "platform" => 1,
        "raw" => 2,
        _ => 3,
    }
}

fn read_u32(path: &Path) -> Result<u32, MonitorError> {
    let value = fs::read_to_string(path)
        .map_err(|error| MonitorError::io(format!("failed to read {}", path.display()), error))?;
    value
        .trim()
        .parse()
        .map_err(|error| MonitorError::InvalidData {
            context: "invalid backlight value",
            details: format!("{}: {error}", path.display()),
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{i2c_bus_of, type_priority};

    #[test]
    fn firmware_interfaces_are_preferred() {
        assert!(type_priority("firmware") < type_priority("platform"));
        assert!(type_priority("platform") < type_priority("raw"));
        assert!(type_priority("raw") < type_priority(""));
    }

    #[test]
    fn only_backlights_under_an_i2c_adapter_are_external() {
        assert_eq!(
            i2c_bus_of(Path::new(
                "/sys/devices/pci0000:00/0000:00:02.0/i2c-14/14-0037/ddcci14/backlight/ddcci14"
            )),
            Some(14)
        );
        assert_eq!(
            i2c_bus_of(Path::new(
                "/sys/devices/pci0000:00/0000:00:02.0/drm/card1/card1-eDP-1/intel_backlight"
            )),
            None
        );
        assert_eq!(
            i2c_bus_of(Path::new("/sys/devices/platform/i2c-designware.0/foo")),
            None
        );
    }
}
