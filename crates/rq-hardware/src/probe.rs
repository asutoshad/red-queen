//! The hardware probe report used for diagnostics and bug reports.

use rq_core::{CapabilityStatus, Feature, HardwareIdentity, KernelInfo, OsInfo};
use serde::Serialize;
use serde_json::Value;

use crate::capabilities::{self, FanIdentity};
use crate::redact;
use crate::root::SystemRoot;
use crate::snapshot::SystemSnapshot;

/// Report format version. Bump when fields are renamed or removed.
pub const SCHEMA_VERSION: u32 = 1;

/// Context the probe can't read from the filesystem.
#[derive(Debug, Clone, Default)]
pub struct ProbeContext {
    /// Version of the tool producing the report.
    pub tool_version: String,
    /// `XDG_CURRENT_DESKTOP`.
    pub desktop: Option<String>,
    /// `XDG_SESSION_TYPE`.
    pub session_type: Option<String>,
    /// Values that must never appear in the output (for example the user
    /// name). The hostname is added automatically.
    pub sensitive: Vec<String>,
}

/// One-line answers to the most common compatibility questions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeSummary {
    /// DMI product name.
    pub model: Option<String>,
    /// BIOS version.
    pub bios: Option<String>,
    /// Kernel release.
    pub kernel: Option<String>,
    /// Desktop environment.
    pub desktop: Option<String>,
    /// acer-wmi loaded.
    pub acer_wmi: bool,
    /// Platform profiles available.
    pub platform_profile: bool,
    /// Fan speeds readable.
    pub fan_telemetry: bool,
    /// Fan control available.
    pub fan_control: bool,
    /// CPU fan identified.
    pub cpu_fan: bool,
    /// GPU fan identified.
    pub gpu_fan: bool,
    /// NVIDIA telemetry available.
    pub nvidia: bool,
    /// RGB keyboard.
    pub rgb: bool,
    /// Battery charge limit.
    pub battery_limit: bool,
    /// USB charging while off.
    pub usb_charging: bool,
    /// LCD overdrive.
    pub lcd_overdrive: bool,
}

/// The full probe report.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeReport {
    /// Report format version.
    pub schema_version: u32,
    /// Producing tool and version.
    pub tool_version: String,
    /// Quick answers.
    pub summary: ProbeSummary,
    /// Hardware identity.
    pub identity: HardwareIdentity,
    /// Kernel.
    pub kernel: KernelInfo,
    /// Operating system.
    pub os: OsInfo,
    /// Session type (`x11`, `wayland`).
    pub session_type: Option<String>,
    /// Fans and how they were identified.
    pub fans: Vec<FanIdentity>,
    /// Capability statuses.
    pub capabilities: Vec<CapabilityStatus>,
    /// Raw discovery data.
    pub snapshot: SystemSnapshot,
}

impl ProbeReport {
    /// Builds a report from a snapshot.
    pub fn from_snapshot(snapshot: SystemSnapshot, ctx: &ProbeContext) -> Self {
        let capabilities = capabilities::evaluate(&snapshot);
        let has = |f: Feature| capabilities.iter().any(|c| c.feature == f && c.supported);
        let summary = ProbeSummary {
            model: snapshot.identity.product_name.clone(),
            bios: snapshot.identity.bios_version.clone(),
            kernel: snapshot.kernel.release.clone(),
            desktop: ctx.desktop.clone(),
            acer_wmi: snapshot.acer.module_loaded,
            platform_profile: has(Feature::ThermalProfiles),
            fan_telemetry: has(Feature::FanTelemetry),
            fan_control: has(Feature::FanControl),
            cpu_fan: has(Feature::CpuFan),
            gpu_fan: has(Feature::GpuFan),
            nvidia: has(Feature::NvidiaTelemetry),
            rgb: has(Feature::RgbKeyboard),
            battery_limit: has(Feature::BatteryChargeLimit),
            usb_charging: has(Feature::UsbPowerOffCharging),
            lcd_overdrive: has(Feature::LcdOverdrive),
        };
        Self {
            schema_version: SCHEMA_VERSION,
            tool_version: ctx.tool_version.clone(),
            fans: capabilities::identify_fans(&snapshot),
            identity: snapshot.identity.clone(),
            kernel: snapshot.kernel.clone(),
            os: snapshot.os.clone(),
            session_type: ctx.session_type.clone(),
            summary,
            capabilities,
            snapshot,
        }
    }

    /// Probes the system under `root`.
    pub fn collect(root: &SystemRoot, ctx: &ProbeContext) -> Self {
        Self::from_snapshot(SystemSnapshot::discover(root), ctx)
    }

    /// The report as redacted JSON, safe to attach to a public issue.
    pub fn to_redacted_json(&self, root: &SystemRoot, ctx: &ProbeContext) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        let mut sensitive = ctx.sensitive.clone();
        sensitive.extend(root.read_string("/proc/sys/kernel/hostname"));
        redact::redact(&mut value, &sensitive);
        value
    }
}
