use std::fs;
use std::path::{Path, PathBuf};

use super::BackendError;

const DRM_CLASS_PATH: &str = "/sys/class/drm";
const EDID_BLOCK_LEN: usize = 128;

pub(super) struct Connector {
    pub(super) name: String,
    pub(super) active: bool,
    device_path: PathBuf,
    edid: Vec<u8>,
}

#[derive(Default)]
pub(super) struct Connectors {
    connectors: Vec<Connector>,
}

pub(super) enum ConnectorMatch<'a> {
    Active(&'a Connector),
    Inactive,
    Unknown,
}

impl Connectors {
    /// Matches a monitor by the base EDID block read over I2C. DP MST buses
    /// are not linked to their connector in sysfs, so EDID is the only
    /// reliable key.
    pub(super) fn match_edid(&self, edid: &[u8]) -> ConnectorMatch<'_> {
        let Some(base) = edid.get(..EDID_BLOCK_LEN) else {
            return ConnectorMatch::Unknown;
        };
        let mut matched = false;
        for connector in &self.connectors {
            if connector.edid.get(..EDID_BLOCK_LEN) == Some(base) {
                if connector.active {
                    return ConnectorMatch::Active(connector);
                }
                matched = true;
            }
        }
        if matched {
            ConnectorMatch::Inactive
        } else {
            ConnectorMatch::Unknown
        }
    }

    /// Finds the connector that owns a sysfs device, such as a backlight
    /// device registered under `card1-eDP-1`.
    pub(super) fn match_device(&self, device_path: &Path) -> ConnectorMatch<'_> {
        match self
            .connectors
            .iter()
            .find(|connector| device_path.starts_with(&connector.device_path))
        {
            Some(connector) if connector.active => ConnectorMatch::Active(connector),
            Some(_) => ConnectorMatch::Inactive,
            None => ConnectorMatch::Unknown,
        }
    }
}

pub(super) fn connectors() -> Result<Connectors, BackendError> {
    let entries = fs::read_dir(DRM_CLASS_PATH)
        .map_err(|error| BackendError::io(format!("failed to read {DRM_CLASS_PATH}"), error))?;
    let mut connectors = Vec::new();

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str().and_then(connector_name) else {
            continue;
        };
        let path = entry.path();
        let Ok(device_path) = fs::canonicalize(&path) else {
            continue;
        };
        let connected = read_trimmed(&path.join("status")).as_deref() == Some("connected");
        let enabled = read_trimmed(&path.join("enabled")).as_deref() == Some("enabled");
        connectors.push(Connector {
            name: name.to_string(),
            active: connected && enabled,
            device_path,
            edid: fs::read(path.join("edid")).unwrap_or_default(),
        });
    }

    Ok(Connectors { connectors })
}

/// Returns the connector part of a DRM entry name: `card1-DP-2` -> `DP-2`.
fn connector_name(entry_name: &str) -> Option<&str> {
    let (card, connector) = entry_name.split_once('-')?;
    let index = card.strip_prefix("card")?;
    (!index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())).then_some(connector)
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Connector, ConnectorMatch, Connectors, connector_name};

    fn connector(name: &str, active: bool, edid_byte: u8) -> Connector {
        Connector {
            name: name.into(),
            active,
            device_path: PathBuf::from(format!("/sys/devices/drm/card1/card1-{name}")),
            edid: vec![edid_byte; 256],
        }
    }

    #[test]
    fn connector_names_skip_cards_and_render_nodes() {
        assert_eq!(connector_name("card1-DP-2"), Some("DP-2"));
        assert_eq!(connector_name("card12-HDMI-A-1"), Some("HDMI-A-1"));
        assert_eq!(connector_name("card1"), None);
        assert_eq!(connector_name("renderD128"), None);
        assert_eq!(connector_name("cardX-DP-1"), None);
    }

    #[test]
    fn edid_matches_prefer_active_connectors() {
        let connectors = Connectors {
            connectors: vec![connector("DP-1", false, 1), connector("DP-2", true, 1)],
        };
        assert!(matches!(
            connectors.match_edid(&[1; 128]),
            ConnectorMatch::Active(connector) if connector.name == "DP-2"
        ));

        let connectors = Connectors {
            connectors: vec![connector("DP-1", false, 1)],
        };
        assert!(matches!(
            connectors.match_edid(&[1; 128]),
            ConnectorMatch::Inactive
        ));
        assert!(matches!(
            connectors.match_edid(&[2; 128]),
            ConnectorMatch::Unknown
        ));
        assert!(matches!(
            connectors.match_edid(&[1; 16]),
            ConnectorMatch::Unknown
        ));
    }

    #[test]
    fn devices_match_their_parent_connector() {
        let connectors = Connectors {
            connectors: vec![connector("eDP-1", true, 1)],
        };
        assert!(matches!(
            connectors.match_device(Path::new(
                "/sys/devices/drm/card1/card1-eDP-1/intel_backlight"
            )),
            ConnectorMatch::Active(_)
        ));
        assert!(matches!(
            connectors.match_device(Path::new("/sys/devices/platform/acpi_video0")),
            ConnectorMatch::Unknown
        ));
    }
}
