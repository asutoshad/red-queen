//! Fake system trees (`/sys`, `/proc`, `/etc`) for tests.
//!
//! Tests point backends at [`FakeSystem::path`] instead of `/`, so they never
//! touch the real machine. Symlinks are created relative, like real sysfs,
//! so canonicalization stays inside the fake tree.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use tempfile::TempDir;

/// A temporary directory laid out like a Linux root filesystem.
pub struct FakeSystem {
    dir: TempDir,
}

impl FakeSystem {
    /// An empty tree.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            dir: tempfile::Builder::new().prefix("rq-fake-").tempdir()?,
        })
    }

    /// The directory to use as the system root.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    fn host_path(&self, abs: &str) -> PathBuf {
        self.dir.path().join(abs.trim_start_matches('/'))
    }

    /// Writes a file (creating parent directories). A trailing newline is
    /// added, as sysfs does.
    pub fn file(&self, abs: &str, contents: &str) -> io::Result<&Self> {
        let p = self.host_path(abs);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(p, format!("{contents}\n"))?;
        Ok(self)
    }

    /// Writes a file with the given Unix mode.
    pub fn file_mode(&self, abs: &str, contents: &str, mode: u32) -> io::Result<&Self> {
        use std::os::unix::fs::PermissionsExt;
        self.file(abs, contents)?;
        fs::set_permissions(self.host_path(abs), fs::Permissions::from_mode(mode))?;
        Ok(self)
    }

    /// Creates a directory.
    pub fn dir(&self, abs: &str) -> io::Result<&Self> {
        fs::create_dir_all(self.host_path(abs))?;
        Ok(self)
    }

    /// Creates a symlink at `link` pointing to `target` (both absolute
    /// within the fake tree). The link is stored relative.
    pub fn symlink(&self, link: &str, target: &str) -> io::Result<&Self> {
        let link_host = self.host_path(link);
        if let Some(parent) = link_host.parent() {
            fs::create_dir_all(parent)?;
        }
        let link_parent = Path::new(link).parent().unwrap_or(Path::new("/"));
        let rel = relative(link_parent, Path::new(target));
        std::os::unix::fs::symlink(rel, link_host)?;
        Ok(self)
    }
}

/// Path from directory `from` to `to`, both absolute.
fn relative(from: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component<'_>> = from.components().collect();
    let to: Vec<Component<'_>> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c);
    }
    out
}

/// Ready-made trees modelled on real machines.
pub mod presets {
    use super::FakeSystem;
    use std::io;

    pub use super::add_acer_pwm_files as add_acer_pwm;

    /// Serial strings planted in fixtures. Probe output must never
    /// contain them.
    pub const PLANTED_SECRETS: [&str; 4] = [
        "SECRET-BATTERY-SERIAL",
        "SECRET-PRODUCT-SERIAL",
        "SECRET-UUID",
        "example-host",
    ];

    /// The ANV15-51 as observed on BIOS V1.60 with kernel 7.1.5.
    ///
    /// With `predator_v4` false this is the stock kernel (no profile, no
    /// fan hwmon). With it true, acer-wmi was loaded with `predator_v4=1`.
    pub fn anv15_51(predator_v4: bool) -> io::Result<FakeSystem> {
        let fs = FakeSystem::new()?;
        dmi(&fs)?;
        os(&fs)?;
        acer_wmi(&fs, predator_v4)?;
        coretemp(&fs)?;
        battery(&fs)?;
        gpus(&fs)?;
        leds_and_input(&fs)?;
        if predator_v4 {
            platform_profile(&fs)?;
            acer_hwmon(&fs)?;
        } else {
            fs.dir("/sys/class/platform-profile")?;
        }
        Ok(fs)
    }

    fn dmi(fs: &FakeSystem) -> io::Result<()> {
        let d = "/sys/class/dmi/id";
        fs.file(&format!("{d}/sys_vendor"), "Acer")?
            .file(&format!("{d}/product_name"), "Nitro ANV15-51")?
            .file(&format!("{d}/product_family"), "Acer Nitro V 15")?
            .file(&format!("{d}/product_version"), "V1.60")?
            .file(&format!("{d}/board_name"), "Sportage_RTH")?
            .file(&format!("{d}/bios_vendor"), "Insyde Corp.")?
            .file(&format!("{d}/bios_version"), "V1.60")?
            .file(&format!("{d}/bios_date"), "04/08/2026")?
            .file_mode(
                &format!("{d}/product_serial"),
                "SECRET-PRODUCT-SERIAL",
                0o400,
            )?
            .file_mode(&format!("{d}/product_uuid"), "SECRET-UUID", 0o400)?;
        Ok(())
    }

    fn os(fs: &FakeSystem) -> io::Result<()> {
        fs.file(
            "/etc/os-release",
            "PRETTY_NAME=\"Kali GNU/Linux Rolling\"\nNAME=\"Kali GNU/Linux\"\nVERSION_ID=\"2026.3\"\nID=kali\nID_LIKE=debian",
        )?
        .file("/proc/sys/kernel/osrelease", "7.1.5+kali-amd64")?
        .file("/proc/sys/kernel/hostname", "example-host")?;
        Ok(())
    }

    fn acer_wmi(fs: &FakeSystem, predator_v4: bool) -> io::Result<()> {
        let p = "/sys/module/acer_wmi/parameters";
        fs.file(
            &format!("{p}/predator_v4"),
            if predator_v4 { "Y" } else { "N" },
        )?
        .file(&format!("{p}/cycle_gaming_thermal_profile"), "Y")?
        .file(&format!("{p}/ec_raw_mode"), "N")?
        .file(&format!("{p}/force_caps"), "-1")?
        .dir("/sys/devices/platform/acer-wmi")?;
        for guid in [
            "7A4DDFE7-5B5D-40B4-8595-4408E0CC7F56-6",
            "61EF69EA-865C-4BC3-A502-A0DEBA0CB531-2",
            "79772EC5-04B1-4BFD-843C-61E7F77B6CC9-4",
            "676AA15E-6A47-4D9F-A2CC-1E6D18D14026-0",
            "05901221-D566-11D1-B2F0-00A0C9062910-13",
            "05901221-D566-11D1-B2F0-00A0C9062910-16",
        ] {
            fs.dir(&format!("/sys/bus/wmi/devices/{guid}"))?;
        }
        Ok(())
    }

    fn coretemp(fs: &FakeSystem) -> io::Result<()> {
        let dev = "/sys/devices/platform/coretemp.0";
        let h = format!("{dev}/hwmon/hwmon5");
        fs.file(&format!("{h}/name"), "coretemp")?
            .file(&format!("{h}/temp1_input"), "48000")?
            .file(&format!("{h}/temp1_label"), "Package id 0")?
            .file(&format!("{h}/temp2_input"), "45000")?
            .file(&format!("{h}/temp2_label"), "Core 0")?
            .symlink(&format!("{h}/device"), dev)?
            .symlink("/sys/class/hwmon/hwmon5", &h)?;
        Ok(())
    }

    fn battery(fs: &FakeSystem) -> io::Result<()> {
        let bat = "/sys/devices/LNXSYSTM:00/LNXSYBUS:00/PNP0C0A:00/power_supply/BAT1";
        fs.file(&format!("{bat}/type"), "Battery")?
            .file(&format!("{bat}/status"), "Discharging")?
            .file(&format!("{bat}/present"), "1")?
            .file(&format!("{bat}/capacity"), "87")?
            .file(&format!("{bat}/technology"), "Li-ion")?
            .file(&format!("{bat}/cycle_count"), "42")?
            .file(&format!("{bat}/charge_full"), "3800000")?
            .file(&format!("{bat}/charge_full_design"), "4000000")?
            .file(&format!("{bat}/voltage_now"), "15800000")?
            .file(&format!("{bat}/current_now"), "1200000")?
            .file(&format!("{bat}/manufacturer"), "SMP")?
            .file(&format!("{bat}/model_name"), "AP21D8M")?
            .file(&format!("{bat}/serial_number"), "SECRET-BATTERY-SERIAL")?
            .symlink("/sys/class/power_supply/BAT1", bat)?;
        let ac = "/sys/devices/platform/ACPI0003:00/power_supply/ACAD";
        fs.file(&format!("{ac}/type"), "Mains")?
            .file(&format!("{ac}/online"), "0")?
            .symlink("/sys/class/power_supply/ACAD", ac)?;
        // A Bluetooth peripheral battery: must be skipped and its MAC never shown.
        let bt = "/sys/devices/virtual/misc/uhid/power_supply/hid-aa:bb:cc:dd:ee:ff-battery";
        fs.file(&format!("{bt}/type"), "Battery")?
            .file(&format!("{bt}/scope"), "Device")?
            .file(&format!("{bt}/capacity"), "50")?
            .symlink("/sys/class/power_supply/hid-aa:bb:cc:dd:ee:ff-battery", bt)?;
        Ok(())
    }

    fn gpus(fs: &FakeSystem) -> io::Result<()> {
        let igpu = "/sys/devices/pci0000:00/0000:00:02.0";
        fs.file(&format!("{igpu}/vendor"), "0x8086")?
            .file(&format!("{igpu}/device"), "0xa7a8")?
            .file(&format!("{igpu}/class"), "0x030000")?
            .file(&format!("{igpu}/boot_vga"), "1")?
            .file(&format!("{igpu}/power/runtime_status"), "active")?
            .dir("/sys/bus/pci/drivers/i915")?
            .symlink(&format!("{igpu}/driver"), "/sys/bus/pci/drivers/i915")?
            .symlink("/sys/bus/pci/devices/0000:00:02.0", igpu)?;
        let dgpu = "/sys/devices/pci0000:00/0000:00:01.0/0000:01:00.0";
        fs.file(&format!("{dgpu}/vendor"), "0x10de")?
            .file(&format!("{dgpu}/device"), "0x28a1")?
            .file(&format!("{dgpu}/class"), "0x030000")?
            .file(&format!("{dgpu}/boot_vga"), "0")?
            .file(&format!("{dgpu}/power/runtime_status"), "suspended")?
            .dir("/sys/bus/pci/drivers/nvidia")?
            .symlink(&format!("{dgpu}/driver"), "/sys/bus/pci/drivers/nvidia")?
            .symlink("/sys/bus/pci/devices/0000:01:00.0", dgpu)?;
        // A non-display device that must be ignored.
        let nvme = "/sys/devices/pci0000:00/0000:00:0e.0";
        fs.file(&format!("{nvme}/vendor"), "0x8086")?
            .file(&format!("{nvme}/device"), "0xa77f")?
            .file(&format!("{nvme}/class"), "0x010400")?
            .symlink("/sys/bus/pci/devices/0000:00:0e.0", nvme)?;
        fs.file(
            "/proc/driver/nvidia/version",
            "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.163.01  Tue Jan 13 00:00:00 UTC 2026\nGCC version:  gcc version 15.2.0",
        )?
        .file("/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1", "")?;
        Ok(())
    }

    fn leds_and_input(fs: &FakeSystem) -> io::Result<()> {
        for led in ["input0::capslock", "input0::numlock", "hda::micmute"] {
            fs.dir(&format!("/sys/class/leds/{led}"))?;
        }
        fs.file(
            "/proc/bus/input/devices",
            "I: Bus=0011 Vendor=0001 Product=0001 Version=ab83\nN: Name=\"AT Translated Set 2 keyboard\"\n\nI: Bus=0019 Vendor=0000 Product=0000 Version=0000\nN: Name=\"Acer WMI hotkeys\"\n\nI: Bus=0005 Vendor=004c Product=0000 Version=0000\nN: Name=\"Someone's Headphones\"\n",
        )?
        .file("/sys/class/powercap/intel-rapl:0/name", "package-0")?
        .file_mode("/sys/class/powercap/intel-rapl:0/energy_uj", "123456789", 0o400)?
        .file("/sys/class/powercap/intel-rapl:0/max_energy_range_uj", "262143328850")?
        .file(
            "/proc/stat",
            "cpu  1000 0 500 8000 100 0 0 0 0 0\ncpu0 500 0 250 4000 50 0 0 0 0 0\ncpu1 500 0 250 4000 50 0 0 0 0 0\nintr 0",
        )?
        .file(
            "/proc/meminfo",
            "MemTotal:       16000000 kB\nMemFree:  2000000 kB\nMemAvailable:    8000000 kB\nSwapTotal:       4000000 kB\nSwapFree:        4000000 kB",
        )?
        .file("/proc/uptime", "12345.67 40000.00")?
        .file("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq", "2400000")?
        .file("/sys/devices/system/cpu/cpu1/cpufreq/scaling_cur_freq", "3600000")?
        .dir("/sys/devices/system/cpu/cpufreq")?;
        Ok(())
    }

    fn platform_profile(fs: &FakeSystem) -> io::Result<()> {
        let choices = "low-power quiet balanced balanced-performance performance";
        fs.file("/sys/firmware/acpi/platform_profile", "balanced")?
            .file("/sys/firmware/acpi/platform_profile_choices", choices)?;
        let c = "/sys/devices/platform/acer-wmi/platform-profile/platform-profile-0";
        fs.file(&format!("{c}/name"), "acer-wmi")?
            .file(&format!("{c}/profile"), "balanced")?
            .file(&format!("{c}/choices"), choices)?
            .symlink("/sys/class/platform-profile/platform-profile-0", c)?;
        Ok(())
    }

    fn acer_hwmon(fs: &FakeSystem) -> io::Result<()> {
        let dev = "/sys/devices/platform/acer-wmi";
        let h = format!("{dev}/hwmon/hwmon7");
        fs.file(&format!("{h}/name"), "acer")?
            .file(&format!("{h}/fan1_input"), "2331")?
            .file(&format!("{h}/fan2_input"), "2071")?
            .file(&format!("{h}/temp1_input"), "47000")?
            .file(&format!("{h}/temp2_input"), "0")?
            .file(&format!("{h}/temp3_input"), "41000")?
            .symlink(&format!("{h}/device"), dev)?
            .symlink("/sys/class/hwmon/hwmon7", &h)?;
        Ok(())
    }
}

/// Adds `pwmN` / `pwmN_enable` files to the acer hwmon device of
/// [`presets::anv15_51`], as a kernel with fan-control support would
/// expose them (0644, 50 % duty, automatic mode).
pub fn add_acer_pwm_files(fs: &FakeSystem) -> io::Result<()> {
    let h = "/sys/devices/platform/acer-wmi/hwmon/hwmon7";
    for n in [1, 2] {
        fs.file_mode(&format!("{h}/pwm{n}"), "128", 0o644)?
            .file_mode(&format!("{h}/pwm{n}_enable"), "2", 0o644)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        assert_eq!(
            relative(
                Path::new("/sys/class/hwmon"),
                Path::new("/sys/devices/x/hwmon/hwmon1")
            ),
            PathBuf::from("../../devices/x/hwmon/hwmon1")
        );
        assert_eq!(
            relative(Path::new("/a"), Path::new("/a/b")),
            PathBuf::from("b")
        );
    }

    #[test]
    fn symlinks_resolve_inside_tree() -> io::Result<()> {
        let fs = presets::anv15_51(true)?;
        let link = fs.path().join("sys/class/hwmon/hwmon7/name");
        assert_eq!(std::fs::read_to_string(link)?.trim(), "acer");
        Ok(())
    }
}
