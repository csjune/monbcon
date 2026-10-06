use std::collections::HashSet;
use std::fmt;
use std::io;
use std::thread;
use std::time::Duration;

use super::BackendError;
use super::drm::{ConnectorMatch, Connectors};
use super::edid::{self, Edid};
use super::i2c::{self, I2cDevice};
use crate::MonitorId;
use crate::controller::AdjustableMonitor;
use crate::scale::{percent_to_raw, raw_to_percent};

const EDID_ADDRESS: u16 = 0x50;
const DDC_ADDRESS: u16 = 0x37;
const DISPLAY_WRITE_ADDRESS: u8 = 0x6E;
const HOST_SOURCE_ADDRESS: u8 = 0x51;
const HOST_READ_ADDRESS: u8 = 0x50;
const GET_VCP_OPCODE: u8 = 0x01;
const GET_VCP_REPLY_OPCODE: u8 = 0x02;
const SET_VCP_OPCODE: u8 = 0x03;
const VCP_BRIGHTNESS: u8 = 0x10;
const GET_VCP_REPLY_LEN: usize = 11;
const LENGTH_FLAG: u8 = 0x80;

// DDC/CI requires the host to wait before reading a reply and between
// consecutive commands.
const REPLY_DELAY: Duration = Duration::from_millis(50);
const COMMAND_DELAY: Duration = Duration::from_millis(50);
const RETRY_DELAY: Duration = Duration::from_millis(100);
const GET_VCP_ATTEMPTS: usize = 3;

pub(super) struct DdcDiscovery {
    pub(super) monitors: Vec<DdcMonitor>,
    pub(super) warnings: Vec<String>,
    /// Monitors whose DDC/CI address is owned by a kernel driver, normally
    /// ddcci-backlight. The backlight backend controls those instead.
    pub(super) claimed: Vec<ClaimedMonitor>,
}

pub(super) struct ClaimedMonitor {
    pub(super) id: MonitorId,
    pub(super) name: String,
    pub(super) bus: u32,
}

pub(super) struct DdcMonitor {
    id: MonitorId,
    name: String,
    brightness: i32,
    bus: u32,
    device: I2cDevice,
    max: u16,
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
        let raw = percent_to_raw(percent, 0, u32::from(self.max)) as u16;
        let result = self.device.write(&set_vcp_request(VCP_BRIGHTNESS, raw));
        thread::sleep(COMMAND_DELAY);
        result.map_err(|error| BackendError::Device {
            context: "failed to set DDC brightness",
            details: format!("{} on i2c-{}: {error}", self.name, self.bus),
        })?;

        self.brightness = percent;
        Ok(())
    }
}

pub(super) fn discover(connectors: &Connectors) -> Result<DdcDiscovery, BackendError> {
    let buses = i2c::display_buses()
        .map_err(|error| BackendError::io("failed to enumerate I2C buses", error))?;
    let mut monitors = Vec::new();
    let mut warnings = Vec::new();
    let mut handled_edids = HashSet::new();
    let mut failures = Vec::new();
    let mut access_errors = Vec::new();
    let mut claimed = Vec::new();

    for bus in buses {
        let edid_bytes = match read_edid(bus.number) {
            Ok(bytes) => bytes,
            Err(error) => {
                if is_access_error(&error) {
                    access_errors.push((bus.name(), error));
                }
                continue;
            }
        };
        let Some(edid) = edid::parse(&edid_bytes) else {
            continue;
        };
        let connector_name = match connectors.match_edid(&edid_bytes) {
            ConnectorMatch::Active(connector) => Some(connector.name.as_str()),
            ConnectorMatch::Inactive => continue,
            ConnectorMatch::Unknown => None,
        };
        // The same monitor can answer on more than one bus, for example on
        // both the DP AUX channel and an MST branch.
        if handled_edids.contains(&edid_bytes) {
            continue;
        }

        match probe_monitor(bus.number, &edid, connector_name) {
            Ok(monitor) => {
                handled_edids.insert(edid_bytes);
                monitors.push(monitor);
            }
            Err(DdcError::Unsupported) => {
                handled_edids.insert(edid_bytes);
            }
            // Leave the EDID unhandled so another bus that reaches the same
            // monitor can still be used for DDC/CI.
            Err(DdcError::Claimed) => claimed.push(ClaimedMonitor {
                id: monitor_id(&edid, bus.number, connector_name),
                name: edid.display_name(),
                bus: bus.number,
            }),
            Err(error) => failures.push((edid_bytes, edid.display_name(), bus.name(), error)),
        }
    }

    for (edid_bytes, display_name, bus_name, error) in failures {
        if handled_edids.insert(edid_bytes) {
            warnings.push(format!(
                "failed to inspect {display_name} on {bus_name}: {error}"
            ));
        }
    }

    if monitors.is_empty()
        && let Some((bus_name, error)) = access_errors.first()
    {
        warnings.push(format!(
            "couldn't open /dev/{bus_name} ({error}); make sure the i2c-dev module is \
             loaded and the user can access /dev/i2c-* (for example via the i2c group)"
        ));
    }

    claimed.retain(|claimed| {
        !monitors
            .iter()
            .any(|monitor: &DdcMonitor| monitor.id == claimed.id)
    });

    Ok(DdcDiscovery {
        monitors,
        warnings,
        claimed,
    })
}

fn is_access_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
    )
}

pub(super) fn read_edid(bus: u32) -> io::Result<Vec<u8>> {
    let mut device = I2cDevice::open_shared(bus, EDID_ADDRESS)?;
    device.write(&[0])?;
    let mut edid = vec![0; edid::BLOCK_LEN];
    device.read(&mut edid)?;
    Ok(edid)
}

fn probe_monitor(
    bus: u32,
    edid: &Edid,
    connector_name: Option<&str>,
) -> Result<DdcMonitor, DdcError> {
    let mut device = I2cDevice::open(bus, DDC_ADDRESS).map_err(|error| {
        if i2c::is_busy(&error) {
            DdcError::Claimed
        } else {
            DdcError::Io(error)
        }
    })?;
    let value = get_vcp(&mut device, VCP_BRIGHTNESS)?;
    if value.max == 0 {
        return Err(DdcError::Protocol(
            "monitor reported a zero brightness range".into(),
        ));
    }

    Ok(DdcMonitor {
        id: monitor_id(edid, bus, connector_name),
        name: edid.display_name(),
        brightness: raw_to_percent(
            u32::from(value.current.min(value.max)),
            0,
            u32::from(value.max),
        ),
        bus,
        device,
        max: value.max,
    })
}

/// Builds the ID shared by every backend that reaches an external monitor,
/// so the same monitor keeps its ID whether it is driven over i2c-dev or
/// through ddcci-backlight.
pub(super) fn monitor_id(edid: &Edid, bus: u32, connector_name: Option<&str>) -> MonitorId {
    let serial = edid.serial().unwrap_or_else(|| {
        connector_name
            .map(str::to_string)
            .unwrap_or_else(|| format!("i2c-{bus}"))
    });
    MonitorId::new(format!(
        "ddc:{}{:04X}:{serial}",
        edid.manufacturer, edid.product_code
    ))
}

fn get_vcp(device: &mut I2cDevice, code: u8) -> Result<VcpValue, DdcError> {
    let request = get_vcp_request(code);
    let mut last_error = DdcError::Protocol("no reply".into());

    for attempt in 0..GET_VCP_ATTEMPTS {
        if attempt > 0 {
            thread::sleep(RETRY_DELAY);
        }
        if let Err(error) = device.write(&request) {
            last_error = DdcError::Io(error);
            continue;
        }
        thread::sleep(REPLY_DELAY);
        let mut reply = [0; GET_VCP_REPLY_LEN];
        if let Err(error) = device.read(&mut reply) {
            last_error = DdcError::Io(error);
            continue;
        }
        match parse_get_vcp_reply(code, &reply) {
            Ok(value) => {
                thread::sleep(COMMAND_DELAY);
                return Ok(value);
            }
            Err(DdcError::Unsupported) => return Err(DdcError::Unsupported),
            Err(error) => last_error = error,
        }
    }

    Err(last_error)
}

#[derive(Debug)]
enum DdcError {
    Claimed,
    Io(io::Error),
    Protocol(String),
    Unsupported,
}

impl fmt::Display for DdcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claimed => write!(formatter, "DDC/CI address is owned by a kernel driver"),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Protocol(details) => write!(formatter, "invalid DDC/CI reply: {details}"),
            Self::Unsupported => write!(formatter, "brightness is not supported"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct VcpValue {
    current: u16,
    max: u16,
}

fn checksum(seed: u8, bytes: &[u8]) -> u8 {
    bytes.iter().fold(seed, |sum, byte| sum ^ byte)
}

fn get_vcp_request(code: u8) -> [u8; 5] {
    let mut request = [
        HOST_SOURCE_ADDRESS,
        LENGTH_FLAG | 2,
        GET_VCP_OPCODE,
        code,
        0,
    ];
    request[4] = checksum(DISPLAY_WRITE_ADDRESS, &request[..4]);
    request
}

fn set_vcp_request(code: u8, value: u16) -> [u8; 7] {
    let [high, low] = value.to_be_bytes();
    let mut request = [
        HOST_SOURCE_ADDRESS,
        LENGTH_FLAG | 4,
        SET_VCP_OPCODE,
        code,
        high,
        low,
        0,
    ];
    request[6] = checksum(DISPLAY_WRITE_ADDRESS, &request[..6]);
    request
}

fn parse_get_vcp_reply(code: u8, reply: &[u8; GET_VCP_REPLY_LEN]) -> Result<VcpValue, DdcError> {
    if reply[0] != DISPLAY_WRITE_ADDRESS {
        return Err(DdcError::Protocol(format!(
            "unexpected source address 0x{:02X}",
            reply[0]
        )));
    }
    if reply[1] & LENGTH_FLAG == 0 {
        return Err(DdcError::Protocol("missing length flag".into()));
    }
    let length = reply[1] & !LENGTH_FLAG;
    if length == 0 {
        return Err(DdcError::Protocol("monitor sent a null message".into()));
    }
    if length != 8 {
        return Err(DdcError::Protocol(format!("unexpected length {length}")));
    }
    if checksum(HOST_READ_ADDRESS, &reply[..10]) != reply[10] {
        return Err(DdcError::Protocol("checksum mismatch".into()));
    }
    if reply[2] != GET_VCP_REPLY_OPCODE {
        return Err(DdcError::Protocol(format!(
            "unexpected opcode 0x{:02X}",
            reply[2]
        )));
    }
    match reply[3] {
        0 => {}
        1 => return Err(DdcError::Unsupported),
        result => {
            return Err(DdcError::Protocol(format!("result code 0x{result:02X}")));
        }
    }
    if reply[4] != code {
        return Err(DdcError::Protocol(format!(
            "reply is for VCP 0x{:02X}",
            reply[4]
        )));
    }

    Ok(VcpValue {
        max: u16::from_be_bytes([reply[6], reply[7]]),
        current: u16::from_be_bytes([reply[8], reply[9]]),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DdcError, GET_VCP_REPLY_LEN, HOST_READ_ADDRESS, VcpValue, checksum, get_vcp_request,
        parse_get_vcp_reply, set_vcp_request,
    };

    fn reply(result: u8, code: u8, max: u16, current: u16) -> [u8; GET_VCP_REPLY_LEN] {
        let [max_high, max_low] = max.to_be_bytes();
        let [current_high, current_low] = current.to_be_bytes();
        let mut reply = [
            0x6E,
            0x88,
            0x02,
            result,
            code,
            0x00,
            max_high,
            max_low,
            current_high,
            current_low,
            0,
        ];
        reply[10] = checksum(HOST_READ_ADDRESS, &reply[..10]);
        reply
    }

    #[test]
    fn requests_match_the_ddc_ci_wire_format() {
        assert_eq!(get_vcp_request(0x10), [0x51, 0x82, 0x01, 0x10, 0xAC]);
        assert_eq!(
            set_vcp_request(0x10, 70),
            [0x51, 0x84, 0x03, 0x10, 0x00, 0x46, 0xEE]
        );
    }

    #[test]
    fn parses_brightness_replies() {
        assert_eq!(
            parse_get_vcp_reply(0x10, &reply(0, 0x10, 100, 42)).unwrap(),
            VcpValue {
                current: 42,
                max: 100
            }
        );
        assert!(matches!(
            parse_get_vcp_reply(0x10, &reply(1, 0x10, 0, 0)),
            Err(DdcError::Unsupported)
        ));
    }

    #[test]
    fn rejects_corrupt_or_unexpected_replies() {
        let mut corrupt = reply(0, 0x10, 100, 42);
        corrupt[9] ^= 1;
        assert!(matches!(
            parse_get_vcp_reply(0x10, &corrupt),
            Err(DdcError::Protocol(_))
        ));

        let mut null_message = [0; GET_VCP_REPLY_LEN];
        null_message[..3].copy_from_slice(&[0x6E, 0x80, 0xBE]);
        assert!(matches!(
            parse_get_vcp_reply(0x10, &null_message),
            Err(DdcError::Protocol(_))
        ));

        assert!(matches!(
            parse_get_vcp_reply(0x10, &reply(0, 0x12, 100, 42)),
            Err(DdcError::Protocol(_))
        ));
    }
}
