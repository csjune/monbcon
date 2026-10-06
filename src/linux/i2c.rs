use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;

const I2C_DEVICES_PATH: &str = "/sys/bus/i2c/devices";
const I2C_SLAVE: libc::c_ulong = 0x0703;
const I2C_SLAVE_FORCE: libc::c_ulong = 0x0706;

/// Adapter names that never carry a display's DDC channel. Probing them can
/// touch unrelated hardware such as memory SPD EEPROMs.
const IGNORED_ADAPTER_PREFIXES: [&str; 7] = [
    "SMBus",
    "Synopsys DesignWare",
    "soc:i2cdsi",
    "smu",
    "mac-io",
    "u4",
    "AMDGPU SMU",
];

pub(super) struct I2cBus {
    pub(super) number: u32,
}

impl I2cBus {
    pub(super) fn name(&self) -> String {
        format!("i2c-{}", self.number)
    }
}

pub(super) fn display_buses() -> io::Result<Vec<I2cBus>> {
    let mut buses = Vec::new();
    for entry in fs::read_dir(I2C_DEVICES_PATH)?.flatten() {
        let file_name = entry.file_name();
        let Some(number) = file_name
            .to_str()
            .and_then(|name| name.strip_prefix("i2c-"))
            .and_then(|number| number.parse::<u32>().ok())
        else {
            continue;
        };
        let adapter_name = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if is_ignored_adapter(adapter_name.trim()) {
            continue;
        }
        buses.push(I2cBus { number });
    }
    buses.sort_by_key(|bus| bus.number);
    Ok(buses)
}

fn is_ignored_adapter(name: &str) -> bool {
    IGNORED_ADAPTER_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// An i2c-dev handle bound to one target address.
pub(super) struct I2cDevice {
    file: File,
}

impl I2cDevice {
    /// Binds to `address`, failing with `EBUSY` when a kernel driver (such
    /// as ddcci) already owns it.
    pub(super) fn open(bus: u32, address: u16) -> io::Result<Self> {
        let file = open_bus(bus)?;
        set_target(&file, I2C_SLAVE, address)?;
        Ok(Self { file })
    }

    /// Binds to `address` even when a kernel driver owns it. Only use this
    /// for read-only access such as fetching an EDID from its EEPROM.
    pub(super) fn open_shared(bus: u32, address: u16) -> io::Result<Self> {
        let file = open_bus(bus)?;
        if let Err(error) = set_target(&file, I2C_SLAVE, address) {
            if !is_busy(&error) {
                return Err(error);
            }
            set_target(&file, I2C_SLAVE_FORCE, address)?;
        }
        Ok(Self { file })
    }

    pub(super) fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.file.write_all(data)
    }

    pub(super) fn read(&mut self, buffer: &mut [u8]) -> io::Result<()> {
        self.file.read_exact(buffer)
    }
}

pub(super) fn is_busy(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::EBUSY)
}

fn open_bus(bus: u32) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/dev/i2c-{bus}"))
}

fn set_target(file: &File, request: libc::c_ulong, address: u16) -> io::Result<()> {
    let result =
        unsafe { libc::ioctl(file.as_raw_fd(), request as _, libc::c_ulong::from(address)) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::is_ignored_adapter;

    #[test]
    fn non_display_adapters_are_ignored() {
        assert!(is_ignored_adapter("SMBus I801 adapter at 0000:00:1f.4"));
        assert!(is_ignored_adapter("Synopsys DesignWare I2C adapter"));
        assert!(!is_ignored_adapter("DPMST"));
        assert!(!is_ignored_adapter("i915 gmbus tc1"));
        assert!(!is_ignored_adapter("NVIDIA i2c adapter 1 at 1:00.0"));
        assert!(!is_ignored_adapter("AMDGPU DM i2c hw bus 0"));
    }
}
