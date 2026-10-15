use std::fs;
use std::path::{Path, PathBuf};

use super::BackendError;
use super::i2c;

const DRM_CLASS_PATH: &str = "/sys/class/drm";
const EDID_BLOCK_LEN: usize = 128;

pub(super) struct Connector {
    pub(super) name: String,
    pub(super) active: bool,
    device_path: PathBuf,
    /// The I2C adapter a non-DP connector links as its DDC channel.
    ddc_path: Option<PathBuf>,
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
        let mut matched = false;
        for connector in self.with_edid(edid) {
            if connector.active {
                return ConnectorMatch::Active(connector);
            }
            matched = true;
        }
        if matched {
            ConnectorMatch::Inactive
        } else {
            ConnectorMatch::Unknown
        }
    }

    /// Counts the active connectors showing the same base EDID block. More
    /// than one means identical monitors that the EDID can't tell apart.
    pub(super) fn active_edid_count(&self, edid: &[u8]) -> usize {
        self.with_edid(edid)
            .filter(|connector| connector.active)
            .count()
    }

    /// Finds the connector that owns a sysfs device, such as a backlight
    /// device registered under `card1-eDP-1`.
    pub(super) fn match_device(&self, device_path: &Path) -> ConnectorMatch<'_> {
        self.first_match(|connector| device_path.starts_with(&connector.device_path))
    }

    /// Finds the connector an I2C bus belongs to. DP AUX adapters are
    /// registered under their connector, and other connectors link their
    /// adapter as `ddc`. DP MST buses have no such link.
    pub(super) fn match_bus(&self, bus: u32) -> ConnectorMatch<'_> {
        match fs::canonicalize(i2c::bus_device_path(bus)) {
            Ok(bus_path) => self.match_bus_path(&bus_path),
            Err(_) => ConnectorMatch::Unknown,
        }
    }

    fn match_bus_path(&self, bus_path: &Path) -> ConnectorMatch<'_> {
        self.first_match(|connector| {
            bus_path.starts_with(&connector.device_path)
                || connector.ddc_path.as_deref() == Some(bus_path)
        })
    }

    fn with_edid<'a>(&'a self, edid: &[u8]) -> impl Iterator<Item = &'a Connector> {
        let base = edid.get(..EDID_BLOCK_LEN);
        self.connectors
            .iter()
            .filter(move |connector| base.is_some() && connector.edid.get(..EDID_BLOCK_LEN) == base)
    }

    fn first_match(&self, predicate: impl Fn(&Connector) -> bool) -> ConnectorMatch<'_> {
        match self
            .connectors
            .iter()
            .find(|connector| predicate(connector))
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
            ddc_path: fs::canonicalize(path.join("ddc")).ok(),
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
            ddc_path: None,
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
    fn identical_edids_are_counted_per_active_connector() {
        let connectors = Connectors {
            connectors: vec![
                connector("DP-1", true, 1),
                connector("HDMI-A-1", true, 1),
                connector("DP-2", false, 1),
                connector("DP-3", true, 2),
            ],
        };
        assert_eq!(connectors.active_edid_count(&[1; 128]), 2);
        assert_eq!(connectors.active_edid_count(&[2; 128]), 1);
        assert_eq!(connectors.active_edid_count(&[1; 16]), 0);
    }

    #[test]
    fn buses_match_aux_children_and_ddc_links() {
        let mut hdmi = connector("HDMI-A-1", true, 1);
        hdmi.ddc_path = Some(PathBuf::from("/sys/devices/pci0000:00/i2c-3"));
        let connectors = Connectors {
            connectors: vec![connector("DP-1", true, 1), hdmi],
        };
        assert!(matches!(
            connectors.match_bus_path(Path::new("/sys/devices/drm/card1/card1-DP-1/i2c-5")),
            ConnectorMatch::Active(connector) if connector.name == "DP-1"
        ));
        assert!(matches!(
            connectors.match_bus_path(Path::new("/sys/devices/pci0000:00/i2c-3")),
            ConnectorMatch::Active(connector) if connector.name == "HDMI-A-1"
        ));
        assert!(matches!(
            connectors.match_bus_path(Path::new("/sys/devices/pci0000:00/i2c-9")),
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
