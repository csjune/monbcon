//! Conversions between hardware brightness values and percentages.

pub(crate) fn raw_to_percent(value: u32, min: u32, max: u32) -> i32 {
    if max <= min {
        return 100;
    }
    (((value.saturating_sub(min)) as f64 / (max - min) as f64) * 100.0)
        .round()
        .clamp(0.0, 100.0) as i32
}

pub(crate) fn percent_to_raw(percent: i32, min: u32, max: u32) -> u32 {
    let range = u64::from(max.saturating_sub(min));
    min + ((percent.clamp(0, 100) as u64 * range + 50) / 100) as u32
}

#[cfg(test)]
mod tests {
    use super::{percent_to_raw, raw_to_percent};

    #[test]
    fn conversion_respects_hardware_ranges() {
        assert_eq!(raw_to_percent(10, 10, 90), 0);
        assert_eq!(raw_to_percent(50, 10, 90), 50);
        assert_eq!(raw_to_percent(90, 10, 90), 100);
        assert_eq!(raw_to_percent(37, 0, 100), 37);
        assert_eq!(raw_to_percent(1, 1, 19200), 0);
        assert_eq!(raw_to_percent(19200, 1, 19200), 100);
        assert_eq!(percent_to_raw(0, 10, 90), 10);
        assert_eq!(percent_to_raw(50, 10, 90), 50);
        assert_eq!(percent_to_raw(100, 10, 90), 90);
        assert_eq!(percent_to_raw(0, 1, 19200), 1);
        assert_eq!(percent_to_raw(100, 1, 19200), 19200);
    }

    #[test]
    fn degenerate_and_huge_ranges_do_not_overflow() {
        assert_eq!(raw_to_percent(5, 5, 5), 100);
        assert_eq!(raw_to_percent(200, 0, 100), 100);
        assert_eq!(percent_to_raw(100, 0, u32::MAX), u32::MAX);
        assert_eq!(percent_to_raw(50, 7, 7), 7);
    }
}
