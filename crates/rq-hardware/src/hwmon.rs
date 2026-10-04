//! Discovery of hwmon devices.
//!
//! hwmon numbering (`hwmon3`, `hwmon7`, ...) changes between boots, so
//! devices are identified by `name`, labels and device path, never by number.

use std::collections::BTreeSet;
use std::path::Path;

use rq_core::{MilliCelsius, Rpm};
use serde::Serialize;

use crate::root::SystemRoot;

const CLASS: &str = "/sys/class/hwmon";

/// One hwmon device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HwmonChip {
    /// Sysfs entry name (for example `hwmon7`). Not stable across boots;
    /// for diagnostics only.
    pub sysfs_name: String,
    /// Driver-provided `name`, for example `acer` or `coretemp`.
    pub name: Option<String>,
    /// Canonical device path, for example `/sys/devices/platform/acer-wmi`.
    pub device_path: Option<String>,
    /// Values were not read because the device is asleep.
    pub skipped_asleep: bool,
    /// Temperature channels.
    pub temps: Vec<TempChannel>,
    /// Fan speed channels.
    pub fans: Vec<FanChannel>,
    /// PWM control channels.
    pub pwms: Vec<PwmChannel>,
}

/// A `tempN_*` channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TempChannel {
    /// Channel number `N`.
    pub index: u32,
    /// `tempN_label`.
    pub label: Option<String>,
    /// `tempN_input`.
    pub input: Option<MilliCelsius>,
}

/// A `fanN_*` channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FanChannel {
    /// Channel number `N`.
    pub index: u32,
    /// `fanN_label`.
    pub label: Option<String>,
    /// `fanN_input`.
    pub input: Option<Rpm>,
}

/// A `pwmN` channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PwmChannel {
    /// Channel number `N`.
    pub index: u32,
    /// `pwmN` raw value.
    pub value: Option<u32>,
    /// `pwmN_enable` raw value.
    pub enable: Option<u32>,
    /// `pwmN_enable` exists.
    pub has_enable: bool,
    /// The owner (root) may write `pwmN`.
    pub owner_writable: bool,
}

impl HwmonChip {
    /// Whether this is the acer-wmi hwmon device.
    pub fn is_acer(&self) -> bool {
        self.name.as_deref() == Some("acer")
    }
}

/// Lists all hwmon devices. Devices under any path in `asleep` are listed
/// but their values are not read, so a runtime-suspended GPU isn't woken.
pub fn discover(root: &SystemRoot, asleep: &[String]) -> Vec<HwmonChip> {
    root.list_dir(CLASS)
        .into_iter()
        .map(|entry| read_chip(root, &entry, asleep))
        .collect()
}

fn read_chip(root: &SystemRoot, entry: &str, asleep: &[String]) -> HwmonChip {
    let dir = format!("{CLASS}/{entry}");
    let device_path = root
        .canonical(format!("{dir}/device"))
        .map(|p| p.display().to_string());
    let skipped_asleep = device_path
        .as_deref()
        .is_some_and(|d| asleep.iter().any(|a| Path::new(d).starts_with(a)));

    let files = root.list_dir(&dir);
    let indices = |prefix: &str| -> BTreeSet<u32> {
        files
            .iter()
            .filter_map(|f| channel_index(f, prefix))
            .collect()
    };
    let read = |file: String| {
        if skipped_asleep {
            None
        } else {
            root.read_string(file)
        }
    };

    let temps = indices("temp")
        .into_iter()
        .map(|i| TempChannel {
            index: i,
            label: root.read_string(format!("{dir}/temp{i}_label")),
            input: read(format!("{dir}/temp{i}_input"))
                .and_then(|s| s.trim().parse().ok())
                .map(MilliCelsius),
        })
        .collect();
    let fans = indices("fan")
        .into_iter()
        .map(|i| FanChannel {
            index: i,
            label: root.read_string(format!("{dir}/fan{i}_label")),
            input: read(format!("{dir}/fan{i}_input"))
                .and_then(|s| s.trim().parse().ok())
                .map(Rpm),
        })
        .collect();
    let pwms = indices("pwm")
        .into_iter()
        .filter(|i| root.exists(format!("{dir}/pwm{i}")))
        .map(|i| PwmChannel {
            index: i,
            value: read(format!("{dir}/pwm{i}")).and_then(|s| s.trim().parse().ok()),
            enable: read(format!("{dir}/pwm{i}_enable")).and_then(|s| s.trim().parse().ok()),
            has_enable: root.exists(format!("{dir}/pwm{i}_enable")),
            owner_writable: root
                .mode(format!("{dir}/pwm{i}"))
                .is_some_and(|m| m & 0o200 != 0),
        })
        .collect();

    HwmonChip {
        sysfs_name: entry.to_owned(),
        name: root.read_string(format!("{dir}/name")),
        device_path,
        skipped_asleep,
        temps,
        fans,
        pwms,
    }
}

/// Extracts `N` from `prefixN` or `prefixN_suffix` (for example `fan2_input`).
fn channel_index(file: &str, prefix: &str) -> Option<u32> {
    let rest = file.strip_prefix(prefix)?;
    let digits: &str = rest.split('_').next()?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_indices() {
        assert_eq!(channel_index("fan2_input", "fan"), Some(2));
        assert_eq!(channel_index("pwm1", "pwm"), Some(1));
        assert_eq!(channel_index("pwm1_enable", "pwm"), Some(1));
        assert_eq!(channel_index("temp10_label", "temp"), Some(10));
        assert_eq!(channel_index("temperature", "temp"), None);
        assert_eq!(channel_index("fan_x", "fan"), None);
        assert_eq!(channel_index("name", "fan"), None);
    }
}
