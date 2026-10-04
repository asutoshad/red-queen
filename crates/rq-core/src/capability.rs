//! The capability model: what this machine can do, and why not.

use serde::{Deserialize, Serialize};

/// A feature The Red Queen knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    /// Switching platform thermal/performance profiles.
    ThermalProfiles,
    /// Reading fan speeds.
    FanTelemetry,
    /// A fan identified as the CPU fan.
    CpuFan,
    /// A fan identified as the GPU fan.
    GpuFan,
    /// Manual / max / automatic fan control.
    FanControl,
    /// CPU package power from RAPL.
    CpuPackagePower,
    /// Battery level, state and health.
    BatteryTelemetry,
    /// Charge limit (for example 80 %).
    BatteryChargeLimit,
    /// Battery calibration cycle.
    BatteryCalibration,
    /// USB charging while the laptop is off.
    UsbPowerOffCharging,
    /// Keyboard backlight brightness.
    KeyboardBacklight,
    /// Keyboard backlight auto-off timeout.
    KeyboardBacklightTimeout,
    /// RGB keyboard lighting.
    RgbKeyboard,
    /// Panel overdrive.
    LcdOverdrive,
    /// Boot animation and sound.
    BootSound,
    /// Custom boot logo.
    BootLogo,
    /// NVIDIA GPU telemetry.
    NvidiaTelemetry,
    /// Changing the GPU power limit.
    GpuPowerLimit,
    /// Firmware updates.
    FirmwareUpdates,
    /// Vendor audio enhancement (TrueHarmony).
    AudioEnhancement,
    /// The dedicated NitroSense key.
    NitroSenseKey,
}

/// How much is known about a capability on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Maturity {
    /// Verified on real hardware of this model.
    Supported,
    /// The interface exists here but hasn't been verified for this model.
    Detected,
    /// Available through reverse-engineered firmware calls; opt-in.
    Experimental,
    /// Not available on this machine.
    Unsupported,
    /// Can't be determined without a live check.
    Unknown,
}

impl Maturity {
    /// Whether the feature can be offered at all.
    pub fn is_available(self) -> bool {
        matches!(self, Self::Supported | Self::Detected | Self::Experimental)
    }
}

/// The interface a capability is provided through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// Linux `platform_profile`.
    PlatformProfile,
    /// The `acer` hwmon device from acer-wmi.
    AcerWmiHwmon,
    /// Any other hwmon device.
    Hwmon,
    /// Linux `power_supply`.
    PowerSupply,
    /// Intel RAPL powercap.
    Rapl,
    /// NVIDIA Management Library.
    Nvml,
    /// LED class device.
    LedClass,
    /// The "Acer WMI hotkeys" input device.
    AcerWmiHotkeys,
    /// The optional companion kernel module.
    CompanionModule,
    /// fwupd.
    Fwupd,
}

/// Why a capability is unavailable or uncertain. Codes only: user-facing
/// text is produced (and translated) by the interface.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Reason {
    /// The kernel exposes no interface for this feature.
    NoInterface,
    /// A driver supports it but needs a module option on this model.
    DriverOptionRequired {
        /// Kernel module name.
        module: String,
        /// Option to set, for example `predator_v4=1`.
        option: String,
    },
    /// The kernel driver doesn't support this yet on this model; a newer
    /// kernel or the companion module is needed.
    PendingKernelSupport,
    /// Firmware has the feature, but only the companion module exposes it.
    CompanionModuleRequired,
    /// A kernel module that provides it isn't loaded.
    KernelModuleNotLoaded {
        /// Kernel module name.
        module: String,
    },
    /// The vendor's proprietary driver is required.
    ProprietaryDriverRequired {
        /// The driver currently bound, if any.
        current_driver: Option<String>,
    },
    /// Needs a live query (for example NVML or fwupd).
    RuntimeCheckRequired,
    /// Deliberately not implemented, for safety or because no hardware
    /// setting exists.
    UnsupportedByDesign,
}

/// The state of one capability on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityStatus {
    /// Which feature.
    pub feature: Feature,
    /// Whether it can be offered.
    pub supported: bool,
    /// How much is known.
    pub maturity: Maturity,
    /// Values can be read.
    pub readable: bool,
    /// Values can be changed.
    pub writable: bool,
    /// Interface used.
    pub backend: Option<Backend>,
    /// Why it's unavailable or uncertain.
    pub reason: Option<Reason>,
    /// Reading or writing needs the privileged daemon.
    pub requires_privilege: bool,
    /// Kernel module that must be loaded.
    pub requires_kernel_module: Option<String>,
}

impl CapabilityStatus {
    /// An interface was found on this machine.
    pub fn detected(feature: Feature, backend: Backend) -> Self {
        Self {
            feature,
            supported: true,
            maturity: Maturity::Detected,
            readable: true,
            writable: false,
            backend: Some(backend),
            reason: None,
            requires_privilege: false,
            requires_kernel_module: None,
        }
    }

    /// Not available, with the reason.
    pub fn unsupported(feature: Feature, reason: Reason) -> Self {
        Self::unavailable(feature, Maturity::Unsupported, reason)
    }

    /// Can't be determined yet.
    pub fn unknown(feature: Feature, reason: Reason) -> Self {
        Self::unavailable(feature, Maturity::Unknown, reason)
    }

    fn unavailable(feature: Feature, maturity: Maturity, reason: Reason) -> Self {
        Self {
            feature,
            supported: false,
            maturity,
            readable: false,
            writable: false,
            backend: None,
            reason: Some(reason),
            requires_privilege: false,
            requires_kernel_module: None,
        }
    }

    /// Marks values as changeable (through the privileged daemon).
    #[must_use]
    pub fn writable(mut self) -> Self {
        self.writable = true;
        self.requires_privilege = true;
        self
    }

    /// Marks values as readable only by the privileged daemon.
    #[must_use]
    pub fn privileged(mut self) -> Self {
        self.requires_privilege = true;
        self
    }

    /// Records a kernel module this capability depends on.
    #[must_use]
    pub fn kernel_module(mut self, module: &str) -> Self {
        self.requires_kernel_module = Some(module.to_owned());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_matches_maturity() {
        let d = CapabilityStatus::detected(Feature::FanTelemetry, Backend::AcerWmiHwmon);
        assert!(d.supported && d.maturity.is_available());
        let u = CapabilityStatus::unsupported(Feature::BootLogo, Reason::UnsupportedByDesign);
        assert!(!u.supported && !u.maturity.is_available());
        let k = CapabilityStatus::unknown(Feature::FirmwareUpdates, Reason::RuntimeCheckRequired);
        assert!(!k.supported);
    }

    #[test]
    fn writable_implies_privilege() {
        let c = CapabilityStatus::detected(Feature::ThermalProfiles, Backend::PlatformProfile)
            .writable();
        assert!(c.writable && c.requires_privilege);
    }

    #[test]
    fn json_shape() {
        let c = CapabilityStatus::unsupported(
            Feature::ThermalProfiles,
            Reason::DriverOptionRequired {
                module: "acer_wmi".into(),
                option: "predator_v4=1".into(),
            },
        );
        let v = serde_json::to_value(&c).expect("serializable");
        assert_eq!(v["feature"], "thermal_profiles");
        assert_eq!(v["maturity"], "unsupported");
        assert_eq!(v["reason"]["code"], "driver_option_required");
        assert_eq!(v["reason"]["option"], "predator_v4=1");
    }
}
