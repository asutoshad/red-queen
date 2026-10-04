//! Turns a [`SystemSnapshot`] into capability statuses.

use rq_core::{Backend, CapabilityStatus as Cap, FanRole, Feature, Reason, RoleSource};
use serde::Serialize;

use crate::gpu::GpuVendor;
use crate::models::{self, ModelQuirks};
use crate::power_supply::SupplyKind;
use crate::snapshot::SystemSnapshot;

const ACER_WMI: &str = "acer_wmi";

/// A fan as identified by discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FanIdentity {
    /// hwmon device name.
    pub chip: String,
    /// Channel number.
    pub index: u32,
    /// What it cools.
    pub role: FanRole,
    /// How the role was determined.
    pub role_source: RoleSource,
}

/// Identifies fans on the acer hwmon device.
///
/// Labels win when present. acer-wmi provides none, but its driver
/// defines channel 1 as the CPU fan and channel 2 as the GPU fan; that
/// mapping is reported as [`RoleSource::DriverChannelOrder`] and must be
/// confirmed by a behaviour test before it's trusted for control.
pub fn identify_fans(snap: &SystemSnapshot) -> Vec<FanIdentity> {
    let mut out = Vec::new();
    for chip in &snap.hwmon {
        let chip_name = chip.name.clone().unwrap_or_default();
        for fan in &chip.fans {
            let (role, role_source) = match fan.label.as_deref() {
                Some(label) => (FanRole::from_label(label), RoleSource::Label),
                None if chip.is_acer() => match fan.index {
                    1 => (FanRole::Cpu, RoleSource::DriverChannelOrder),
                    2 => (FanRole::Gpu, RoleSource::DriverChannelOrder),
                    _ => (FanRole::Unknown, RoleSource::Unknown),
                },
                None => (FanRole::Unknown, RoleSource::Unknown),
            };
            out.push(FanIdentity {
                chip: chip_name.clone(),
                index: fan.index,
                role,
                role_source,
            });
        }
    }
    out
}

/// Evaluates every known feature.
pub fn evaluate(snap: &SystemSnapshot) -> Vec<Cap> {
    let model = models::lookup(&snap.identity);
    let fans = identify_fans(snap);
    vec![
        thermal_profiles(snap, model),
        fan_telemetry(snap, model, &fans),
        fan_role(snap, model, &fans, FanRole::Cpu, Feature::CpuFan),
        fan_role(snap, model, &fans, FanRole::Gpu, Feature::GpuFan),
        fan_control(snap, model),
        cpu_package_power(snap),
        battery_telemetry(snap),
        battery_charge_limit(snap),
        companion(
            snap.acer.battery_health_interface,
            Feature::BatteryCalibration,
        ),
        companion(snap.acer.apge_interface, Feature::UsbPowerOffCharging),
        keyboard_backlight(snap),
        companion(snap.acer.apge_interface, Feature::KeyboardBacklightTimeout),
        Cap::unsupported(Feature::RgbKeyboard, Reason::NoInterface),
        companion(snap.acer.gaming_interface, Feature::LcdOverdrive),
        companion(snap.acer.gaming_interface, Feature::BootSound),
        Cap::unsupported(Feature::BootLogo, Reason::UnsupportedByDesign),
        nvidia_telemetry(snap),
        gpu_power_limit(snap),
        Cap::unknown(Feature::FirmwareUpdates, Reason::RuntimeCheckRequired),
        Cap::unsupported(Feature::AudioEnhancement, Reason::UnsupportedByDesign),
        nitrosense_key(snap),
    ]
}

/// The reason acer-wmi features are missing, if it's the known
/// `predator_v4` situation on a tested model.
fn acer_option_reason(snap: &SystemSnapshot, model: Option<&ModelQuirks>) -> Option<Reason> {
    let model = model?;
    (model.needs_predator_v4
        && snap.acer.module_loaded
        && snap.acer.gaming_interface
        && !snap.acer.predator_v4_enabled())
    .then(|| Reason::DriverOptionRequired {
        module: ACER_WMI.to_owned(),
        option: "predator_v4=1".to_owned(),
    })
}

fn thermal_profiles(snap: &SystemSnapshot, model: Option<&ModelQuirks>) -> Cap {
    if snap.platform_profile.available() {
        let acer = snap
            .platform_profile
            .handlers
            .iter()
            .any(|h| h.name.as_deref() == Some("acer-wmi"));
        let cap = Cap::detected(Feature::ThermalProfiles, Backend::PlatformProfile).writable();
        return if acer {
            cap.kernel_module(ACER_WMI)
        } else {
            cap
        };
    }
    let reason = acer_option_reason(snap, model).unwrap_or(Reason::NoInterface);
    Cap::unsupported(Feature::ThermalProfiles, reason)
}

fn fan_telemetry(snap: &SystemSnapshot, model: Option<&ModelQuirks>, fans: &[FanIdentity]) -> Cap {
    if snap.acer_hwmon().is_some_and(|c| !c.fans.is_empty()) {
        return Cap::detected(Feature::FanTelemetry, Backend::AcerWmiHwmon).kernel_module(ACER_WMI);
    }
    if !fans.is_empty() {
        return Cap::detected(Feature::FanTelemetry, Backend::Hwmon);
    }
    let reason = acer_option_reason(snap, model).unwrap_or(Reason::NoInterface);
    Cap::unsupported(Feature::FanTelemetry, reason)
}

fn fan_role(
    snap: &SystemSnapshot,
    model: Option<&ModelQuirks>,
    fans: &[FanIdentity],
    role: FanRole,
    feature: Feature,
) -> Cap {
    match fans.iter().find(|f| f.role == role) {
        Some(f) => {
            let backend = if f.chip == "acer" {
                Backend::AcerWmiHwmon
            } else {
                Backend::Hwmon
            };
            Cap::detected(feature, backend)
        }
        None => {
            let reason = acer_option_reason(snap, model).unwrap_or(Reason::NoInterface);
            Cap::unsupported(feature, reason)
        }
    }
}

fn fan_control(snap: &SystemSnapshot, model: Option<&ModelQuirks>) -> Cap {
    let controllable = snap
        .acer_hwmon()
        .is_some_and(|c| c.pwms.iter().any(|p| p.has_enable && p.owner_writable));
    if controllable {
        return Cap::detected(Feature::FanControl, Backend::AcerWmiHwmon)
            .writable()
            .kernel_module(ACER_WMI);
    }
    if let Some(reason) = acer_option_reason(snap, model) {
        return Cap::unsupported(Feature::FanControl, reason);
    }
    let reason = if model.is_some_and(|m| m.pwm_pending_upstream) {
        Reason::PendingKernelSupport
    } else {
        Reason::NoInterface
    };
    Cap::unsupported(Feature::FanControl, reason)
}

fn cpu_package_power(snap: &SystemSnapshot) -> Cap {
    if snap.rapl.present {
        Cap::detected(Feature::CpuPackagePower, Backend::Rapl).privileged()
    } else {
        Cap::unsupported(Feature::CpuPackagePower, Reason::NoInterface)
    }
}

fn batteries(snap: &SystemSnapshot) -> impl Iterator<Item = &crate::power_supply::BatteryInfo> {
    snap.power_supplies
        .iter()
        .filter(|p| p.kind == SupplyKind::Battery)
        .filter_map(|p| p.battery.as_ref())
}

fn battery_telemetry(snap: &SystemSnapshot) -> Cap {
    if batteries(snap).next().is_some() {
        Cap::detected(Feature::BatteryTelemetry, Backend::PowerSupply)
    } else {
        Cap::unsupported(Feature::BatteryTelemetry, Reason::NoInterface)
    }
}

fn battery_charge_limit(snap: &SystemSnapshot) -> Cap {
    if batteries(snap).any(|b| b.charge_control.available()) {
        return Cap::detected(Feature::BatteryChargeLimit, Backend::PowerSupply).writable();
    }
    companion(
        snap.acer.battery_health_interface,
        Feature::BatteryChargeLimit,
    )
}

/// A feature the firmware has but only the companion module would expose.
fn companion(firmware_interface: bool, feature: Feature) -> Cap {
    let reason = if firmware_interface {
        Reason::CompanionModuleRequired
    } else {
        Reason::NoInterface
    };
    Cap::unsupported(feature, reason)
}

fn keyboard_backlight(snap: &SystemSnapshot) -> Cap {
    if snap.keyboard_backlights().next().is_some() {
        Cap::detected(Feature::KeyboardBacklight, Backend::LedClass).writable()
    } else {
        Cap::unsupported(Feature::KeyboardBacklight, Reason::NoInterface)
    }
}

fn nvidia_gpu(snap: &SystemSnapshot) -> Option<&crate::gpu::GpuInfo> {
    snap.gpus.iter().find(|g| g.vendor == GpuVendor::Nvidia)
}

fn nvidia_telemetry(snap: &SystemSnapshot) -> Cap {
    let Some(gpu) = nvidia_gpu(snap) else {
        return Cap::unsupported(Feature::NvidiaTelemetry, Reason::NoInterface);
    };
    if gpu.driver.as_deref() == Some("nvidia") && snap.nvidia.nvml_library {
        Cap::detected(Feature::NvidiaTelemetry, Backend::Nvml).kernel_module("nvidia")
    } else {
        Cap::unsupported(
            Feature::NvidiaTelemetry,
            Reason::ProprietaryDriverRequired {
                current_driver: gpu.driver.clone(),
            },
        )
    }
}

fn gpu_power_limit(snap: &SystemSnapshot) -> Cap {
    if nvidia_telemetry(snap).supported {
        Cap::unknown(Feature::GpuPowerLimit, Reason::RuntimeCheckRequired)
    } else {
        Cap::unsupported(Feature::GpuPowerLimit, Reason::NoInterface)
    }
}

fn nitrosense_key(snap: &SystemSnapshot) -> Cap {
    if snap.acer.hotkeys_input {
        Cap::detected(Feature::NitroSenseKey, Backend::AcerWmiHotkeys).kernel_module(ACER_WMI)
    } else {
        Cap::unsupported(Feature::NitroSenseKey, Reason::NoInterface)
    }
}
