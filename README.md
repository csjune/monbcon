# monbcon

[![crates.io](https://img.shields.io/crates/v/monbcon.svg)](https://crates.io/crates/monbcon)
[![docs.rs](https://img.shields.io/docsrs/monbcon)](https://docs.rs/monbcon)
[![CI](https://github.com/csjune/monbcon/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/csjune/monbcon/actions/workflows/ci.yml)

Monitor discovery and brightness control for Windows and Linux.

monbcon finds the displays attached to the system and reads or changes
their brightness, covering both external monitors and laptop panels:

| Platform | External monitors | Laptop panels |
|---|---|---|
| Windows | DDC/CI through the Monitor Configuration API | WMI (`WmiMonitorBrightness`) |
| Linux | DDC/CI over `i2c-dev`, or the `ddcci-backlight` driver when it owns the bus | `/sys/class/backlight` through logind |

It started as the backend of [MMB](https://github.com/csjune/MMB), a tray app
for adjusting brightness across multiple monitors.

## Usage

```rust
use monbcon::{BrightnessUpdate, MonitorController};

fn main() -> Result<(), monbcon::MonitorError> {
    let mut controller = MonitorController::new();
    let result = controller.refresh()?;

    for monitor in &result.snapshots {
        println!("{} ({}): {}%", monitor.name, monitor.id, monitor.brightness);
    }

    let updates = result
        .snapshots
        .iter()
        .map(|monitor| BrightnessUpdate {
            generation: result.generation,
            id: monitor.id.clone(),
            value: 50,
        })
        .collect();
    for outcome in controller.apply(updates).outcomes {
        if let Some(error) = outcome.error {
            eprintln!("{}: {error}", outcome.id);
        }
    }
    Ok(())
}
```

Every `refresh` starts a new generation. Updates carry the generation they
were made against, and `apply` rejects updates from an older generation, so a
change queued before the monitor list changed never reaches the wrong display.

DDC/CI calls block for tens to hundreds of milliseconds, so applications with
a UI should drive `MonitorController` from a worker thread.

## Linux requirements

- The `i2c-dev` kernel module must be loaded, and the user needs read/write
  access to `/dev/i2c-*` (usually via the `i2c` group or a udev rule).
- DDC/CI must be enabled in the monitor's on-screen menu.
- Laptop backlights are set through logind's `SetBrightness`, which works for
  the active session without root.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
