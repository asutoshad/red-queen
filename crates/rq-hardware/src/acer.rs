//! Acer-specific discovery: the acer-wmi module, WMI interfaces and the
//! hotkey input device.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::root::SystemRoot;

/// Acer gaming WMI interface (profiles, fans, sensors).
pub const GUID_GAMING: &str = "7A4DDFE7-5B5D-40B4-8595-4408E0CC7F56";
/// Acer "APGE action" WMI interface (USB charging, backlight timeout).
pub const GUID_APGE: &str = "61EF69EA-865C-4BC3-A502-A0DEBA0CB531";
/// Acer battery-health WMI interface (charge limit, calibration).
pub const GUID_BATTERY_HEALTH: &str = "79772EC5-04B1-4BFD-843C-61E7F77B6CC9";

/// Name of the hotkey input device registered by acer-wmi.
const HOTKEY_DEVICE: &str = "Acer WMI hotkeys";

/// State of the acer-wmi driver and related firmware interfaces.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AcerInfo {
    /// The `acer_wmi` module is loaded.
    pub module_loaded: bool,
    /// Module parameters (`/sys/module/acer_wmi/parameters`).
    pub parameters: BTreeMap<String, String>,
    /// The `acer-wmi` platform device exists.
    pub platform_device: bool,
    /// Gaming WMI interface present in firmware.
    pub gaming_interface: bool,
    /// APGE action WMI interface present in firmware.
    pub apge_interface: bool,
    /// Battery-health WMI interface present in firmware.
    pub battery_health_interface: bool,
    /// The "Acer WMI hotkeys" input device exists.
    pub hotkeys_input: bool,
}

impl AcerInfo {
    /// Whether `predator_v4` is enabled on the loaded driver.
    pub fn predator_v4_enabled(&self) -> bool {
        self.parameters
            .get("predator_v4")
            .is_some_and(|v| v == "Y" || v == "1")
    }
}

/// WMI GUIDs exposed by the firmware (instance suffixes removed).
pub fn wmi_guids(root: &SystemRoot) -> Vec<String> {
    let mut guids: Vec<String> = root
        .list_dir("/sys/bus/wmi/devices")
        .into_iter()
        .map(|entry| strip_instance(&entry).to_ascii_uppercase())
        .collect();
    guids.sort();
    guids.dedup();
    guids
}

/// WMI device entries are `GUID-N`; returns the GUID part.
fn strip_instance(entry: &str) -> &str {
    if entry.len() > 36
        && entry.as_bytes()[36] == b'-'
        && entry[37..].bytes().all(|b| b.is_ascii_digit())
    {
        &entry[..36]
    } else {
        entry
    }
}

/// Reads acer-wmi state.
pub fn discover(root: &SystemRoot, guids: &[String]) -> AcerInfo {
    let pdir = "/sys/module/acer_wmi/parameters";
    let parameters = root
        .list_dir(pdir)
        .into_iter()
        .filter_map(|p| root.read_string(format!("{pdir}/{p}")).map(|v| (p, v)))
        .collect();
    let has = |g: &str| guids.iter().any(|x| x.eq_ignore_ascii_case(g));
    AcerInfo {
        module_loaded: root.exists("/sys/module/acer_wmi"),
        parameters,
        platform_device: root.exists("/sys/devices/platform/acer-wmi"),
        gaming_interface: has(GUID_GAMING),
        apge_interface: has(GUID_APGE),
        battery_health_interface: has(GUID_BATTERY_HEALTH),
        hotkeys_input: input_device_present(root, HOTKEY_DEVICE),
    }
}

/// Whether an input device with exactly this name exists. Only a yes/no
/// answer is returned: other device names (which may be personal, such as
/// "Someone's Headphones") are never reported.
pub fn input_device_present(root: &SystemRoot, name: &str) -> bool {
    let want = format!("N: Name=\"{name}\"");
    root.read_string("/proc/bus/input/devices")
        .is_some_and(|text| text.lines().any(|l| l.trim() == want))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_instance_suffix() {
        assert_eq!(
            strip_instance("7A4DDFE7-5B5D-40B4-8595-4408E0CC7F56-6"),
            "7A4DDFE7-5B5D-40B4-8595-4408E0CC7F56"
        );
        assert_eq!(
            strip_instance("05901221-D566-11D1-B2F0-00A0C9062910-16"),
            "05901221-D566-11D1-B2F0-00A0C9062910"
        );
        assert_eq!(strip_instance("short"), "short");
    }
}
