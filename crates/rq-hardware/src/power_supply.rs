//! Linux `power_supply` discovery.
//!
//! Batteries are found by `type`, never by assuming `BAT0`. Peripheral
//! supplies (`scope=Device`, for example Bluetooth headphones) are skipped,
//! and `serial_number` is never read.

use rq_core::BatteryState;
use serde::Serialize;

use crate::root::SystemRoot;

const CLASS: &str = "/sys/class/power_supply";

/// Kind of power supply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SupplyKind {
    /// A battery.
    Battery,
    /// AC adapter.
    Mains,
    /// USB / USB-C power.
    Usb,
    /// Anything else (raw kernel type).
    Other(String),
}

/// A system power supply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PowerSupply {
    /// Sysfs name, for example `BAT1` or `ACAD`.
    pub name: String,
    /// Kind.
    pub kind: SupplyKind,
    /// `online` (adapters).
    pub online: Option<bool>,
    /// Battery details, for batteries.
    pub battery: Option<BatteryInfo>,
}

/// Battery details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatteryInfo {
    /// `present`.
    pub present: Option<bool>,
    /// Charging state.
    pub state: Option<BatteryState>,
    /// `capacity` in percent.
    pub capacity_percent: Option<u8>,
    /// `technology`.
    pub technology: Option<String>,
    /// `manufacturer`.
    pub manufacturer: Option<String>,
    /// `model_name`.
    pub model_name: Option<String>,
    /// `cycle_count`.
    pub cycle_count: Option<u32>,
    /// Full capacity as a percentage of design capacity.
    pub health_percent: Option<u8>,
    /// `voltage_now` in µV.
    pub voltage_now_uv: Option<i64>,
    /// `current_now` in µA.
    pub current_now_ua: Option<i64>,
    /// `power_now` in µW.
    pub power_now_uw: Option<i64>,
    /// Generic charge-limit attributes, if the kernel exposes them.
    pub charge_control: ChargeControl,
}

/// Generic kernel charge-control attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ChargeControl {
    /// `charge_control_end_threshold`.
    pub end_threshold: Option<u8>,
    /// `charge_control_start_threshold`.
    pub start_threshold: Option<u8>,
    /// `charge_types`.
    pub charge_types: Option<String>,
    /// `charge_behaviour`.
    pub charge_behaviour: Option<String>,
}

impl ChargeControl {
    /// Whether any charge-limit attribute exists.
    pub fn available(&self) -> bool {
        self.end_threshold.is_some() || self.charge_types.is_some()
    }
}

/// Lists system power supplies.
pub fn discover(root: &SystemRoot) -> Vec<PowerSupply> {
    root.list_dir(CLASS)
        .into_iter()
        .filter_map(|name| {
            let dir = format!("{CLASS}/{name}");
            if root.read_string(format!("{dir}/scope")).as_deref() == Some("Device") {
                return None;
            }
            let kind = match root.read_string(format!("{dir}/type"))?.as_str() {
                "Battery" => SupplyKind::Battery,
                "Mains" => SupplyKind::Mains,
                "USB" => SupplyKind::Usb,
                other => SupplyKind::Other(other.to_owned()),
            };
            let battery = (kind == SupplyKind::Battery).then(|| read_battery(root, &dir));
            Some(PowerSupply {
                online: root
                    .read_parse::<u8>(format!("{dir}/online"))
                    .map(|v| v != 0),
                battery,
                kind,
                name,
            })
        })
        .collect()
}

fn read_battery(root: &SystemRoot, dir: &str) -> BatteryInfo {
    let s = |f: &str| {
        root.read_string(format!("{dir}/{f}"))
            .filter(|v| !v.trim().is_empty())
    };
    let n = |f: &str| root.read_parse::<i64>(format!("{dir}/{f}"));
    let health = health_percent(n("charge_full"), n("charge_full_design"))
        .or_else(|| health_percent(n("energy_full"), n("energy_full_design")));
    BatteryInfo {
        present: root
            .read_parse::<u8>(format!("{dir}/present"))
            .map(|v| v != 0),
        state: s("status").map(|v| BatteryState::from_kernel(&v)),
        capacity_percent: root
            .read_parse::<u8>(format!("{dir}/capacity"))
            .filter(|v| *v <= 100),
        technology: s("technology"),
        manufacturer: s("manufacturer"),
        model_name: s("model_name"),
        cycle_count: root.read_parse(format!("{dir}/cycle_count")),
        health_percent: health,
        voltage_now_uv: n("voltage_now"),
        current_now_ua: n("current_now"),
        power_now_uw: n("power_now"),
        charge_control: ChargeControl {
            end_threshold: root.read_parse(format!("{dir}/charge_control_end_threshold")),
            start_threshold: root.read_parse(format!("{dir}/charge_control_start_threshold")),
            charge_types: s("charge_types"),
            charge_behaviour: s("charge_behaviour"),
        },
    }
}

fn health_percent(full: Option<i64>, design: Option<i64>) -> Option<u8> {
    let (full, design) = (full?, design?);
    if full <= 0 || design <= 0 {
        return None;
    }
    u8::try_from((full * 100 + design / 2) / design)
        .ok()
        .map(|p| p.min(100))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health() {
        assert_eq!(health_percent(Some(3_800_000), Some(4_000_000)), Some(95));
        assert_eq!(health_percent(Some(4_100_000), Some(4_000_000)), Some(100));
        assert_eq!(health_percent(Some(0), Some(4_000_000)), None);
        assert_eq!(health_percent(None, Some(1)), None);
    }
}
