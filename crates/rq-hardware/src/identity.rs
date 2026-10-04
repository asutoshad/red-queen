//! DMI, kernel and OS identity.

use rq_core::{HardwareIdentity, KernelInfo, OsInfo};

use crate::root::SystemRoot;

const DMI: &str = "/sys/class/dmi/id";

/// Reads DMI identity. Only non-identifying fields are read: serial
/// numbers, UUIDs and asset tags are never opened.
pub fn hardware_identity(root: &SystemRoot) -> HardwareIdentity {
    let f = |name: &str| {
        root.read_string(format!("{DMI}/{name}"))
            .filter(|s| !s.trim().is_empty())
    };
    HardwareIdentity {
        vendor: f("sys_vendor"),
        product_name: f("product_name"),
        product_family: f("product_family"),
        product_version: f("product_version"),
        board_name: f("board_name"),
        bios_vendor: f("bios_vendor"),
        bios_version: f("bios_version"),
        bios_date: f("bios_date"),
    }
}

/// The running kernel release (equivalent to `uname -r`).
pub fn kernel_info(root: &SystemRoot) -> KernelInfo {
    KernelInfo {
        release: root.read_string("/proc/sys/kernel/osrelease"),
    }
}

/// Parses `os-release` (`/etc/os-release`, falling back to
/// `/usr/lib/os-release`).
pub fn os_info(root: &SystemRoot) -> OsInfo {
    let text = root
        .read_string("/etc/os-release")
        .or_else(|| root.read_string("/usr/lib/os-release"))
        .unwrap_or_default();
    parse_os_release(&text)
}

fn parse_os_release(text: &str) -> OsInfo {
    let mut info = OsInfo::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unquote(value.trim());
        let slot = match key.trim() {
            "ID" => &mut info.id,
            "ID_LIKE" => &mut info.id_like,
            "VERSION_ID" => &mut info.version_id,
            "PRETTY_NAME" => &mut info.pretty_name,
            _ => continue,
        };
        *slot = Some(value);
    }
    info
}

fn unquote(v: &str) -> String {
    let stripped = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
    stripped.unwrap_or(v).replace("\\\"", "\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_os_release() {
        let info = parse_os_release(
            "PRETTY_NAME=\"Kali GNU/Linux Rolling\"\nID=kali\nID_LIKE='debian'\n# comment\nVERSION_ID=\"2026.3\"\nGARBAGE",
        );
        assert_eq!(info.id.as_deref(), Some("kali"));
        assert_eq!(info.id_like.as_deref(), Some("debian"));
        assert_eq!(info.version_id.as_deref(), Some("2026.3"));
        assert_eq!(info.pretty_name.as_deref(), Some("Kali GNU/Linux Rolling"));
    }
}
