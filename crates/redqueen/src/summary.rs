//! Human-readable probe summary.

use std::io::{self, Write};

use rq_core::{CapabilityStatus, FanRole, Feature, Maturity, Reason, RoleSource};
use rq_hardware::ProbeReport;

/// Writes the summary.
pub fn write(out: &mut impl Write, r: &ProbeReport) -> io::Result<()> {
    let id = &r.identity;
    let na = "unknown";
    writeln!(out, "The Red Queen hardware probe\n")?;
    writeln!(out, "Machine")?;
    row(
        out,
        "Model",
        &format!(
            "{} {}",
            id.vendor.as_deref().unwrap_or(na),
            id.product_name.as_deref().unwrap_or(na)
        ),
    )?;
    row(
        out,
        "BIOS",
        &format!(
            "{} ({})",
            id.bios_version.as_deref().unwrap_or(na),
            id.bios_date.as_deref().unwrap_or(na)
        ),
    )?;
    row(out, "Kernel", r.kernel.release.as_deref().unwrap_or(na))?;
    row(out, "OS", r.os.pretty_name.as_deref().unwrap_or(na))?;
    row(
        out,
        "Desktop",
        &format!(
            "{} ({})",
            r.summary.desktop.as_deref().unwrap_or(na),
            r.session_type.as_deref().unwrap_or(na)
        ),
    )?;

    let s = &r.snapshot;
    writeln!(out, "\nDrivers")?;
    let acer = if s.acer.module_loaded {
        format!(
            "loaded, predator_v4={}",
            if s.acer.predator_v4_enabled() {
                "on"
            } else {
                "off"
            }
        )
    } else {
        "not loaded".to_owned()
    };
    row(out, "acer_wmi", &acer)?;
    for g in &s.gpus {
        row(
            out,
            &format!("GPU {}", g.address),
            &format!(
                "{:04x}:{:04x}  driver {}  ({})",
                g.vendor_id,
                g.device_id,
                g.driver.as_deref().unwrap_or("none"),
                g.runtime_status.as_deref().unwrap_or("?")
            ),
        )?;
    }
    if let Some(v) = &s.nvidia.driver_version {
        row(
            out,
            "NVIDIA driver",
            &format!(
                "{v}, NVML library {}",
                if s.nvidia.nvml_library {
                    "found"
                } else {
                    "missing"
                }
            ),
        )?;
    }

    if !r.fans.is_empty() {
        writeln!(out, "\nFans")?;
        for f in &r.fans {
            let rpm = s
                .hwmon
                .iter()
                .find(|c| c.name.as_deref() == Some(f.chip.as_str()))
                .and_then(|c| c.fans.iter().find(|x| x.index == f.index))
                .and_then(|x| x.input)
                .map_or_else(|| "no reading".to_owned(), |v| format!("{} RPM", v.0));
            let role = match f.role {
                FanRole::Cpu => "CPU",
                FanRole::Gpu => "GPU",
                FanRole::Unknown => "unknown",
            };
            let how = match f.role_source {
                RoleSource::Label => "from label",
                RoleSource::DriverChannelOrder => "from driver channel order, unverified",
                RoleSource::Unknown => "unidentified",
            };
            row(
                out,
                &format!("{} fan{}", f.chip, f.index),
                &format!("{rpm}  {role} ({how})"),
            )?;
        }
    }

    if let Some(state) = s
        .platform_profile
        .handlers
        .first()
        .map(|h| &h.state)
        .or(s.platform_profile.legacy.as_ref())
    {
        writeln!(out, "\nThermal profiles")?;
        row(
            out,
            "Active",
            state.active.as_ref().map_or(na, |p| p.kernel_name()),
        )?;
        let choices: Vec<&str> = state.choices.iter().map(|c| c.kernel_name()).collect();
        row(out, "Advertised", &choices.join(", "))?;
        writeln!(
            out,
            "  (advertised choices may still be rejected by firmware)"
        )?;
    }

    writeln!(out, "\nCapabilities")?;
    for c in &r.capabilities {
        row(out, feature_name(c.feature), &status_line(c))?;
    }
    writeln!(
        out,
        "\nFor an issue report, attach the output of: redqueen probe --json"
    )?;
    Ok(())
}

fn row(out: &mut impl Write, key: &str, value: &str) -> io::Result<()> {
    writeln!(out, "  {key:<24} {value}")
}

fn status_line(c: &CapabilityStatus) -> String {
    let status = match c.maturity {
        Maturity::Supported => "supported",
        Maturity::Detected => "detected",
        Maturity::Experimental => "experimental",
        Maturity::Unsupported => "not available",
        Maturity::Unknown => "unknown",
    };
    match &c.reason {
        Some(reason) => format!("{status}: {}", reason_text(reason)),
        None if c.requires_privilege && !c.writable => format!("{status} (readable by the daemon)"),
        None => status.to_owned(),
    }
}

fn reason_text(r: &Reason) -> String {
    match r {
        Reason::NoInterface => "no kernel interface on this machine".into(),
        Reason::DriverOptionRequired { module, option } => {
            format!("needs the {module} driver option {option}")
        }
        Reason::PendingKernelSupport => {
            "not yet supported by the kernel driver for this model".into()
        }
        Reason::CompanionModuleRequired => {
            "firmware supports it; needs the optional companion module".into()
        }
        Reason::KernelModuleNotLoaded { module } => format!("kernel module {module} not loaded"),
        Reason::ProprietaryDriverRequired { current_driver } => format!(
            "needs the proprietary driver (current: {})",
            current_driver.as_deref().unwrap_or("none")
        ),
        Reason::RuntimeCheckRequired => "checked by the daemon at run time".into(),
        Reason::UnsupportedByDesign => "not offered (no safe interface)".into(),
    }
}

fn feature_name(f: Feature) -> &'static str {
    match f {
        Feature::ThermalProfiles => "Thermal profiles",
        Feature::FanTelemetry => "Fan speeds",
        Feature::CpuFan => "CPU fan",
        Feature::GpuFan => "GPU fan",
        Feature::FanControl => "Fan control",
        Feature::CpuPackagePower => "CPU package power",
        Feature::BatteryTelemetry => "Battery status",
        Feature::BatteryChargeLimit => "Battery charge limit",
        Feature::BatteryCalibration => "Battery calibration",
        Feature::UsbPowerOffCharging => "USB charging when off",
        Feature::KeyboardBacklight => "Keyboard backlight",
        Feature::KeyboardBacklightTimeout => "Backlight timeout",
        Feature::RgbKeyboard => "RGB keyboard",
        Feature::LcdOverdrive => "LCD overdrive",
        Feature::BootSound => "Boot sound",
        Feature::BootLogo => "Boot logo",
        Feature::NvidiaTelemetry => "NVIDIA telemetry",
        Feature::GpuPowerLimit => "GPU power limit",
        Feature::FirmwareUpdates => "Firmware updates",
        Feature::AudioEnhancement => "TrueHarmony audio",
        Feature::NitroSenseKey => "NitroSense key",
    }
}
