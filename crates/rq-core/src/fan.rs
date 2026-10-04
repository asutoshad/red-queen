//! Fan roles and modes.

use serde::{Deserialize, Serialize};

/// What a fan cools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanRole {
    /// CPU fan.
    Cpu,
    /// GPU fan.
    Gpu,
    /// Unknown purpose.
    Unknown,
}

/// How a fan's role was determined. Anything other than [`RoleSource::Label`]
/// must be confirmed by a behaviour test before it is trusted for control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleSource {
    /// The driver provides a label naming the fan.
    Label,
    /// The driver documents a fixed channel order but provides no label.
    DriverChannelOrder,
    /// No information.
    Unknown,
}

impl FanRole {
    /// Derives a role from a hwmon label such as `"CPU Fan"`.
    pub fn from_label(label: &str) -> Self {
        let l = label.to_ascii_lowercase();
        if l.contains("cpu") {
            Self::Cpu
        } else if l.contains("gpu") {
            Self::Gpu
        } else {
            Self::Unknown
        }
    }
}

/// Fan control mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanMode {
    /// Firmware automatic control.
    Auto,
    /// Full speed.
    Max,
    /// User-defined speed or curve.
    Custom,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_from_labels() {
        assert_eq!(FanRole::from_label("CPU Fan"), FanRole::Cpu);
        assert_eq!(FanRole::from_label("gpu"), FanRole::Gpu);
        assert_eq!(FanRole::from_label("fan1"), FanRole::Unknown);
    }
}
