//! Battery and power types.

use serde::{Deserialize, Serialize};

/// Battery charging state, from `power_supply` `status`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatteryState {
    /// Charging.
    Charging,
    /// Discharging.
    Discharging,
    /// On AC but not charging (for example held at a charge limit).
    NotCharging,
    /// Full.
    Full,
    /// The kernel reports `Unknown`, or a value this version doesn't know.
    Unknown,
}

impl BatteryState {
    /// Parses the kernel `status` attribute.
    pub fn from_kernel(status: &str) -> Self {
        match status.trim() {
            "Charging" => Self::Charging,
            "Discharging" => Self::Discharging,
            "Not charging" => Self::NotCharging,
            "Full" => Self::Full,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kernel_states() {
        assert_eq!(
            BatteryState::from_kernel("Charging\n"),
            BatteryState::Charging
        );
        assert_eq!(
            BatteryState::from_kernel("Not charging"),
            BatteryState::NotCharging
        );
        assert_eq!(BatteryState::from_kernel("Full"), BatteryState::Full);
        assert_eq!(
            BatteryState::from_kernel("Discharging"),
            BatteryState::Discharging
        );
        assert_eq!(BatteryState::from_kernel("weird"), BatteryState::Unknown);
    }
}
