use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::BackendError;
use super::drm::{ConnectorMatch, Connectors};
use super::edid::{self, Edid};
use super::i2c::{self, I2cDevice};
use crate::MonitorId;
use crate::controller::AdjustableMonitor;
use crate::parallel::map_parallel;
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
    /// The EDID read from each bus, so other backends needn't read it again.
    pub(super) edids: HashMap<u32, Vec<u8>>,
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
    channel: DdcChannel,
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
        self.channel
            .write(&set_vcp_request(VCP_BRIGHTNESS, raw))
            .map_err(|error| BackendError::Device {
                context: "failed to set DDC brightness",
                details: format!("{} on i2c-{}: {error}", self.name, self.channel.bus),
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
    let mut access_errors = Vec::new();
    let mut claimed = Vec::new();
    let mut edids = HashMap::new();
    let mut groups = Vec::<MonitorGroup>::new();
    let mut group_indices = HashMap::new();

    let scans = map_parallel(buses, |bus| {
        let scan = scan_bus(bus.number, connectors);
        (bus, scan)
    });
    for (bus, scan) in scans {
        let BusScan { edid_bytes, found } = match scan {
            Ok(scan) => scan,
            Err(error) => {
                if is_access_error(&error) {
                    access_errors.push((bus.name(), error));
                }
                continue;
            }
        };
        edids.insert(bus.number, edid_bytes.clone());
        let Some((edid, placement)) = found else {
            continue;
        };
        // The same monitor can answer on more than one bus, for example on
        // both the DP AUX channel and an MST branch. Identical monitors share
        // their EDID too, so they are told apart by connector where possible.
        let key = (edid_bytes, placement.twin_connector.map(str::to_string));
        let index = *group_indices.entry(key).or_insert_with(|| {
            groups.push(MonitorGroup {
                edid,
                buses: Vec::new(),
            });
            groups.len() - 1
        });
        groups[index].buses.push((bus.number, placement));
    }

    // Each monitor is probed on its own thread; the buses that reach the same
    // monitor are tried one after another.
    for outcome in map_parallel(groups, probe_group) {
        monitors.extend(outcome.monitor);
        claimed.extend(outcome.claimed);
        warnings.extend(outcome.warning);
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
        edids,
    })
}

/// The EDID read from a bus, along with the monitor it identifies when the
/// EDID is valid and the monitor's connector isn't inactive.
struct BusScan<'a> {
    edid_bytes: Vec<u8>,
    found: Option<(Edid, Placement<'a>)>,
}

fn scan_bus(bus: u32, connectors: &Connectors) -> io::Result<BusScan<'_>> {
    let edid_bytes = read_edid(bus)?;
    let found = edid::parse(&edid_bytes).and_then(|edid| {
        let placement = place(&edid_bytes, bus, connectors)?;
        Some((edid, placement))
    });
    Ok(BusScan { edid_bytes, found })
}

/// One monitor and every bus that reaches it, in bus order.
struct MonitorGroup<'a> {
    edid: Edid,
    buses: Vec<(u32, Placement<'a>)>,
}

struct GroupOutcome<M = DdcMonitor> {
    monitor: Option<M>,
    claimed: Vec<ClaimedMonitor>,
    warning: Option<String>,
}

fn probe_group(group: MonitorGroup) -> GroupOutcome {
    probe_group_with(group, probe_monitor)
}

/// Probes a monitor through each of its buses until one either works or
/// shows that the monitor has no brightness control. Only if none does is
/// the first failure reported.
fn probe_group_with<M>(
    group: MonitorGroup,
    probe: impl Fn(u32, &Edid, &Placement) -> Result<M, DdcError>,
) -> GroupOutcome<M> {
    let mut claimed = Vec::new();
    let mut failure = None;

    for (bus, placement) in &group.buses {
        match probe(*bus, &group.edid, placement) {
            Ok(monitor) => {
                return GroupOutcome {
                    monitor: Some(monitor),
                    claimed,
                    warning: None,
                };
            }
            Err(DdcError::Unsupported) => {
                return GroupOutcome {
                    monitor: None,
                    claimed,
                    warning: None,
                };
            }
            // Keep trying the other buses, which may reach the monitor
            // without going through the kernel driver.
            Err(DdcError::Claimed) => claimed.push(ClaimedMonitor {
                id: placement.monitor_id(&group.edid, *bus),
                name: group.edid.display_name(),
                bus: *bus,
            }),
            Err(error) => {
                failure.get_or_insert_with(|| {
                    format!(
                        "failed to inspect {} on i2c-{bus}: {error}",
                        group.edid.display_name()
                    )
                });
            }
        }
    }

    GroupOutcome {
        monitor: None,
        claimed,
        warning: failure,
    }
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

fn probe_monitor(bus: u32, edid: &Edid, placement: &Placement) -> Result<DdcMonitor, DdcError> {
    let device = I2cDevice::open(bus, DDC_ADDRESS).map_err(|error| {
        if i2c::is_busy(&error) {
            DdcError::Claimed
        } else {
            DdcError::Io(error)
        }
    })?;
    let mut channel = DdcChannel { device, bus };
    let value = get_vcp(&mut channel, VCP_BRIGHTNESS)?;
    if value.max == 0 {
        return Err(DdcError::Protocol(
            "monitor reported a zero brightness range".into(),
        ));
    }

    Ok(DdcMonitor {
        id: placement.monitor_id(edid, bus),
        name: edid.display_name(),
        brightness: raw_to_percent(
            u32::from(value.current.min(value.max)),
            0,
            u32::from(value.max),
        ),
        channel,
        max: value.max,
    })
}

/// Where an external monitor reached over an I2C bus is connected.
pub(super) struct Placement<'a> {
    connector: Option<&'a str>,
    /// The bus's own connector, set only when identical monitors share the
    /// EDID byte for byte and sysfs links this bus to its connector.
    twin_connector: Option<&'a str>,
}

/// Finds the connector of the monitor on `bus`, or `None` if that connector
/// is inactive.
pub(super) fn place<'a>(
    edid_bytes: &[u8],
    bus: u32,
    connectors: &'a Connectors,
) -> Option<Placement<'a>> {
    let connector = match connectors.match_edid(edid_bytes) {
        ConnectorMatch::Active(connector) => Some(connector.name.as_str()),
        ConnectorMatch::Inactive => return None,
        ConnectorMatch::Unknown => None,
    };
    // The EDID can't tell identical monitors apart, but the bus's link to its
    // connector can. DP MST buses have no link and stay matched by EDID.
    let twin_connector = if connectors.active_edid_count(edid_bytes) > 1 {
        match connectors.match_bus(bus) {
            ConnectorMatch::Active(connector) => Some(connector.name.as_str()),
            ConnectorMatch::Inactive => return None,
            ConnectorMatch::Unknown => None,
        }
    } else {
        None
    };

    Some(Placement {
        connector,
        twin_connector,
    })
}

impl Placement<'_> {
    /// Builds the ID shared by every backend that reaches an external
    /// monitor, so the same monitor keeps its ID whether it is driven over
    /// i2c-dev or through ddcci-backlight.
    pub(super) fn monitor_id(&self, edid: &Edid, bus: u32) -> MonitorId {
        let serial = self
            .twin_connector
            .map(str::to_string)
            .or_else(|| edid.serial())
            .or_else(|| self.connector.map(str::to_string))
            .unwrap_or_else(|| format!("i2c-{bus}"));
        MonitorId::new(format!(
            "ddc:{}{:04X}:{serial}",
            edid.manufacturer, edid.product_code
        ))
    }
}

/// When each bus may next carry a DDC/CI command. Kept per bus rather than
/// per handle so the pause also holds across refreshes, which reopen the bus.
static NEXT_COMMAND: Mutex<BTreeMap<u32, Instant>> = Mutex::new(BTreeMap::new());

/// A DDC/CI channel that waits out the pause a monitor needs between
/// commands only when the next command comes, instead of after every one.
struct DdcChannel {
    device: I2cDevice,
    bus: u32,
}

impl DdcChannel {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.wait_until_ready();
        let result = self.device.write(data);
        self.pause_next_command();
        result
    }

    fn read(&mut self, buffer: &mut [u8]) -> io::Result<()> {
        let result = self.device.read(buffer);
        self.pause_next_command();
        result
    }

    fn wait_until_ready(&self) {
        let ready_at = next_commands().get(&self.bus).copied();
        if let Some(ready_at) = ready_at {
            let wait = ready_at.saturating_duration_since(Instant::now());
            if !wait.is_zero() {
                thread::sleep(wait);
            }
        }
    }

    fn pause_next_command(&self) {
        next_commands().insert(self.bus, Instant::now() + COMMAND_DELAY);
    }
}

fn next_commands() -> MutexGuard<'static, BTreeMap<u32, Instant>> {
    NEXT_COMMAND.lock().unwrap_or_else(PoisonError::into_inner)
}

fn get_vcp(channel: &mut DdcChannel, code: u8) -> Result<VcpValue, DdcError> {
    let request = get_vcp_request(code);
    let mut last_error = DdcError::Protocol("no reply".into());

    for attempt in 0..GET_VCP_ATTEMPTS {
        if attempt > 0 {
            thread::sleep(RETRY_DELAY);
        }
        if let Err(error) = channel.write(&request) {
            last_error = DdcError::Io(error);
            continue;
        }
        thread::sleep(REPLY_DELAY);
        let mut reply = [0; GET_VCP_REPLY_LEN];
        if let Err(error) = channel.read(&mut reply) {
            last_error = DdcError::Io(error);
            continue;
        }
        match parse_get_vcp_reply(code, &reply) {
            Ok(value) => return Ok(value),
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
    use std::cell::RefCell;
    use std::io;

    use super::{
        DdcError, Edid, GET_VCP_REPLY_LEN, HOST_READ_ADDRESS, MonitorGroup, Placement, VcpValue,
        checksum, get_vcp_request, parse_get_vcp_reply, probe_group_with, set_vcp_request,
    };

    fn group(buses: &[u32]) -> MonitorGroup<'static> {
        MonitorGroup {
            edid: Edid {
                manufacturer: "DEL".into(),
                product_code: 0xA0F1,
                serial_number: 7,
                name: Some("DELL U2520D".into()),
                serial_text: None,
            },
            buses: buses
                .iter()
                .map(|&bus| {
                    (
                        bus,
                        Placement {
                            connector: Some("DP-1"),
                            twin_connector: None,
                        },
                    )
                })
                .collect(),
        }
    }

    /// Probes with a scripted result per bus and records which buses were
    /// tried.
    fn probe_scripted(
        buses: &[u32],
        result: impl Fn(u32) -> Result<u32, DdcError>,
    ) -> (super::GroupOutcome<u32>, Vec<u32>) {
        let tried = RefCell::new(Vec::new());
        let outcome = probe_group_with(group(buses), |bus, _, _| {
            tried.borrow_mut().push(bus);
            result(bus)
        });
        (outcome, tried.into_inner())
    }

    #[test]
    fn a_failing_bus_falls_back_to_the_next_one() {
        let (outcome, tried) = probe_scripted(&[4, 5], |bus| match bus {
            4 => Err(DdcError::Io(io::Error::other("no ack"))),
            _ => Ok(bus),
        });
        assert_eq!(outcome.monitor, Some(5));
        assert!(outcome.warning.is_none());
        assert_eq!(tried, [4, 5]);
    }

    #[test]
    fn claimed_buses_are_reported_but_not_final() {
        let (outcome, _) = probe_scripted(&[4, 5], |bus| match bus {
            4 => Err(DdcError::Claimed),
            _ => Ok(bus),
        });
        assert_eq!(outcome.monitor, Some(5));
        let claimed = outcome
            .claimed
            .iter()
            .map(|claimed| (claimed.bus, claimed.id.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(claimed, [(4, "ddc:DELA0F1:7")]);
    }

    #[test]
    fn unsupported_brightness_stops_without_a_warning() {
        let (outcome, tried) = probe_scripted(&[4, 5], |bus| match bus {
            4 => Err(DdcError::Unsupported),
            _ => Ok(bus),
        });
        assert_eq!(outcome.monitor, None);
        assert!(outcome.warning.is_none());
        assert_eq!(tried, [4]);
    }

    #[test]
    fn only_the_first_failure_is_reported_when_every_bus_fails() {
        let (outcome, _) = probe_scripted(&[4, 5], |bus| match bus {
            4 => Err(DdcError::Protocol("checksum mismatch".into())),
            _ => Err(DdcError::Io(io::Error::other("no ack"))),
        });
        assert_eq!(outcome.monitor, None);
        assert_eq!(
            outcome.warning.as_deref(),
            Some("failed to inspect DELL U2520D on i2c-4: invalid DDC/CI reply: checksum mismatch")
        );
    }

    #[test]
    fn identical_monitors_are_identified_by_connector() {
        let edid = Edid {
            manufacturer: "DEL".into(),
            product_code: 0xA0F1,
            serial_number: 0,
            name: None,
            serial_text: Some("SAME".into()),
        };
        let single = Placement {
            connector: Some("DP-1"),
            twin_connector: None,
        };
        let twin = Placement {
            connector: Some("DP-1"),
            twin_connector: Some("HDMI-A-1"),
        };

        assert_eq!(single.monitor_id(&edid, 4).as_str(), "ddc:DELA0F1:SAME");
        assert_eq!(twin.monitor_id(&edid, 4).as_str(), "ddc:DELA0F1:HDMI-A-1");
    }

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
