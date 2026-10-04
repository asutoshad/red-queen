//! A complete, read-only snapshot of what the machine exposes.

use rq_core::{HardwareIdentity, KernelInfo, OsInfo};
use serde::Serialize;

use crate::acer::{self, AcerInfo};
use crate::gpu::{self, GpuInfo, NvidiaInfo};
use crate::hwmon::{self, HwmonChip};
use crate::platform_profile::{self, PlatformProfileInfo};
use crate::power_supply::{self, PowerSupply};
use crate::root::SystemRoot;

/// Intel RAPL package-power interface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RaplInfo {
    /// `intel-rapl:0` exists.
    pub present: bool,
    /// `energy_uj` is readable by the current process (root only on
    /// current kernels).
    pub readable_here: bool,
}

/// Everything discovery found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SystemSnapshot {
    /// DMI identity.
    pub identity: HardwareIdentity,
    /// Running kernel.
    pub kernel: KernelInfo,
    /// Operating system.
    pub os: OsInfo,
    /// acer-wmi state.
    pub acer: AcerInfo,
    /// Firmware WMI GUIDs.
    pub wmi_guids: Vec<String>,
    /// Platform profile interfaces.
    pub platform_profile: PlatformProfileInfo,
    /// hwmon devices.
    pub hwmon: Vec<HwmonChip>,
    /// System power supplies.
    pub power_supplies: Vec<PowerSupply>,
    /// Display devices.
    pub gpus: Vec<GpuInfo>,
    /// NVIDIA driver state.
    pub nvidia: NvidiaInfo,
    /// LED class device names.
    pub leds: Vec<String>,
    /// RAPL.
    pub rapl: RaplInfo,
}

impl SystemSnapshot {
    /// Discovers everything under `root`. Never fails: anything missing
    /// or unreadable is simply absent from the snapshot.
    pub fn discover(root: &SystemRoot) -> Self {
        let gpus = gpu::discover(root);
        let asleep: Vec<String> = gpus
            .iter()
            .filter(|g| g.is_asleep())
            .filter_map(|g| g.device_path.clone())
            .collect();
        let wmi_guids = acer::wmi_guids(root);
        let rapl_energy = "/sys/class/powercap/intel-rapl:0/energy_uj";
        Self {
            identity: crate::identity::hardware_identity(root),
            kernel: crate::identity::kernel_info(root),
            os: crate::identity::os_info(root),
            acer: acer::discover(root, &wmi_guids),
            platform_profile: platform_profile::discover(root),
            hwmon: hwmon::discover(root, &asleep),
            power_supplies: power_supply::discover(root),
            nvidia: gpu::nvidia_info(root),
            leds: root.list_dir("/sys/class/leds"),
            rapl: RaplInfo {
                present: root.exists(rapl_energy),
                readable_here: root.is_readable(rapl_energy),
            },
            wmi_guids,
            gpus,
        }
    }

    /// The acer-wmi hwmon device, if present.
    pub fn acer_hwmon(&self) -> Option<&HwmonChip> {
        self.hwmon.iter().find(|c| c.is_acer())
    }

    /// LED names that are keyboard backlights.
    pub fn keyboard_backlights(&self) -> impl Iterator<Item = &str> {
        self.leds
            .iter()
            .map(String::as_str)
            .filter(|l| l.contains("kbd_backlight"))
    }
}
