//! GPU discovery from PCI sysfs.
//!
//! Only cached PCI attributes and the runtime-PM status are read, so a
//! suspended GPU is never woken. No `lspci` is executed.

use serde::Serialize;

use crate::root::SystemRoot;

const PCI: &str = "/sys/bus/pci/devices";

/// Known GPU vendors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuVendor {
    /// NVIDIA (0x10de).
    Nvidia,
    /// Intel (0x8086).
    Intel,
    /// AMD (0x1002).
    Amd,
    /// Other vendor.
    Other,
}

/// A display-class PCI device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GpuInfo {
    /// PCI address, for example `0000:01:00.0`.
    pub address: String,
    /// Canonical device path.
    pub device_path: Option<String>,
    /// Vendor.
    pub vendor: GpuVendor,
    /// PCI vendor ID.
    pub vendor_id: u16,
    /// PCI device ID.
    pub device_id: u16,
    /// Bound kernel driver, for example `nvidia`, `nouveau`, `i915`.
    pub driver: Option<String>,
    /// Runtime-PM status: `active`, `suspended`, ...
    pub runtime_status: Option<String>,
    /// Firmware boot display.
    pub boot_vga: Option<bool>,
}

impl GpuInfo {
    /// The device is runtime-suspended and must not be queried.
    pub fn is_asleep(&self) -> bool {
        matches!(
            self.runtime_status.as_deref(),
            Some("suspended" | "suspending")
        )
    }
}

/// NVIDIA driver state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NvidiaInfo {
    /// Proprietary kernel module version from `/proc/driver/nvidia/version`.
    pub driver_version: Option<String>,
    /// `libnvidia-ml.so.1` was found in a standard library directory.
    pub nvml_library: bool,
}

/// Lists display-class PCI devices.
pub fn discover(root: &SystemRoot) -> Vec<GpuInfo> {
    root.list_dir(PCI)
        .into_iter()
        .filter_map(|addr| {
            let dir = format!("{PCI}/{addr}");
            let class = parse_hex(&root.read_string(format!("{dir}/class"))?)?;
            if class >> 16 != 0x03 {
                return None;
            }
            let vendor_id =
                u16::try_from(parse_hex(&root.read_string(format!("{dir}/vendor"))?)?).ok()?;
            let device_id = root
                .read_string(format!("{dir}/device"))
                .and_then(|s| parse_hex(&s))
                .and_then(|v| u16::try_from(v).ok())
                .unwrap_or(0);
            Some(GpuInfo {
                device_path: root.canonical(&dir).map(|p| p.display().to_string()),
                vendor: match vendor_id {
                    0x10de => GpuVendor::Nvidia,
                    0x8086 => GpuVendor::Intel,
                    0x1002 => GpuVendor::Amd,
                    _ => GpuVendor::Other,
                },
                vendor_id,
                device_id,
                driver: root.link_name(format!("{dir}/driver")),
                runtime_status: root.read_string(format!("{dir}/power/runtime_status")),
                boot_vga: root
                    .read_parse::<u8>(format!("{dir}/boot_vga"))
                    .map(|v| v != 0),
                address: addr,
            })
        })
        .collect()
}

/// Reads NVIDIA driver information without touching the GPU.
pub fn nvidia_info(root: &SystemRoot) -> NvidiaInfo {
    let driver_version = root
        .read_string("/proc/driver/nvidia/version")
        .and_then(|text| parse_nvidia_version(&text));
    let nvml_library = [
        "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1",
        "/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1",
        "/usr/lib64/libnvidia-ml.so.1",
        "/usr/lib/libnvidia-ml.so.1",
    ]
    .iter()
    .any(|p| root.exists(p));
    NvidiaInfo {
        driver_version,
        nvml_library,
    }
}

/// Extracts the version from the `NVRM version:` line.
fn parse_nvidia_version(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with("NVRM version:"))?;
    line.split_whitespace()
        .find(|w| {
            w.contains('.')
                && w.split('.')
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(str::to_owned)
}

fn parse_hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_version() {
        let t = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.163.01  Tue Jan 13 2026\nGCC version: 15.2.0";
        assert_eq!(parse_nvidia_version(t).as_deref(), Some("550.163.01"));
        assert_eq!(parse_nvidia_version("garbage"), None);
    }

    #[test]
    fn hex() {
        assert_eq!(parse_hex("0x10de\n"), Some(0x10de));
        assert_eq!(parse_hex("0x030000"), Some(0x030000));
        assert_eq!(parse_hex("zz"), None);
    }
}
