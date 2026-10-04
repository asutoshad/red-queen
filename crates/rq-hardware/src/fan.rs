//! Fan control through hwmon `pwmN` / `pwmN_enable`.
//!
//! The backend defines the duty scale; callers speak in [`Percent`]. Every
//! write first re-verifies that the sysfs path still belongs to the chip
//! that was discovered, because hwmon numbering changes when a driver is
//! reloaded and a stale path must never be written.

use std::fmt::Debug;
use std::io;
use std::sync::Arc;

use rq_core::{DutyScale, FanRole, Percent};

use crate::capabilities::identify_fans;
use crate::root::SystemRoot;
use crate::snapshot::SystemSnapshot;

/// The only chip we drive fans on.
const ACER_CHIP: &str = "acer";

/// Fan mode as the hardware reports it (hwmon `pwmN_enable` semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwFanMode {
    /// Firmware automatic control (`pwmN_enable` = 2).
    Auto,
    /// Full speed (`pwmN_enable` = 0).
    Max,
    /// Speed set by software (`pwmN_enable` = 1).
    Manual,
}

impl HwFanMode {
    fn to_enable(self) -> &'static str {
        match self {
            Self::Max => "0",
            Self::Manual => "1",
            Self::Auto => "2",
        }
    }

    fn from_enable(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Max),
            1 => Some(Self::Manual),
            2..=5 => Some(Self::Auto), // hwmon: 2 and above are automatic modes
            _ => None,
        }
    }
}

/// A controllable fan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanChannelInfo {
    /// Telemetry id, `<chip>/<index>` (matches `FanReading::id`).
    pub key: String,
    /// Name clients use: `cpu` or `gpu` when the role is known and unique,
    /// otherwise the key.
    pub name: String,
    /// What it cools.
    pub role: FanRole,
    /// How the role was determined.
    pub role_source: rq_core::RoleSource,
}

/// Access to the fans of one device. A trait so the logic above it can be
/// tested with simulated firmware.
pub trait FanIo: Send + Sync + Debug {
    /// The controllable fans.
    fn channels(&self) -> Vec<FanChannelInfo>;
    /// Current speed in RPM.
    fn read_rpm(&self, key: &str) -> io::Result<u32>;
    /// Current mode.
    fn read_mode(&self, key: &str) -> io::Result<HwFanMode>;
    /// Changes the mode. Success means accepted, not applied: read back.
    fn write_mode(&self, key: &str, mode: HwFanMode) -> io::Result<()>;
    /// Current duty cycle.
    fn read_duty(&self, key: &str) -> io::Result<Percent>;
    /// Sets the duty cycle. Success means accepted, not applied: read back.
    fn write_duty(&self, key: &str, duty: Percent) -> io::Result<()>;
    /// Identifies the interface (for logs and change detection).
    fn describe(&self) -> String;
}

/// Fans on the acer hwmon device.
#[derive(Debug)]
pub struct HwmonFanIo {
    root: SystemRoot,
    chip_dir: String,
    chip_name: String,
    device_path: Option<String>,
    scale: DutyScale,
    channels: Vec<(FanChannelInfo, u32)>,
}

impl HwmonFanIo {
    /// Builds the interface for the acer hwmon chip if it has at least one
    /// fan with both `pwmN` and `pwmN_enable` writable by the owner.
    pub fn select(root: &SystemRoot, snap: &SystemSnapshot) -> Option<Arc<dyn FanIo>> {
        let chip = snap.acer_hwmon()?;
        let fans = identify_fans(snap);
        let role_counts = |role: FanRole| {
            fans.iter()
                .filter(|f| f.chip == ACER_CHIP && f.role == role)
                .count()
        };
        let channels: Vec<(FanChannelInfo, u32)> = chip
            .pwms
            .iter()
            .filter(|p| p.has_enable && p.owner_writable)
            .filter_map(|p| {
                let fan = fans
                    .iter()
                    .find(|f| f.chip == ACER_CHIP && f.index == p.index)?;
                let key = format!("{ACER_CHIP}/{}", p.index);
                let unique =
                    matches!(fan.role, FanRole::Cpu | FanRole::Gpu) && role_counts(fan.role) == 1;
                let name = match (unique, fan.role) {
                    (true, FanRole::Cpu) => "cpu".to_owned(),
                    (true, FanRole::Gpu) => "gpu".to_owned(),
                    _ => key.clone(),
                };
                Some((
                    FanChannelInfo {
                        key,
                        name,
                        role: fan.role,
                        role_source: fan.role_source,
                    },
                    p.index,
                ))
            })
            .collect();
        if channels.is_empty() {
            return None;
        }
        Some(Arc::new(Self {
            root: root.clone(),
            chip_dir: format!("/sys/class/hwmon/{}", chip.sysfs_name),
            chip_name: ACER_CHIP.to_owned(),
            device_path: chip.device_path.clone(),
            scale: DutyScale::Hwmon255,
            channels,
        }))
    }

    fn index(&self, key: &str) -> io::Result<u32> {
        self.channels
            .iter()
            .find(|(c, _)| c.key == key)
            .map(|(_, i)| *i)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown fan"))
    }

    /// Fails unless the path still belongs to the chip we discovered.
    fn verify_chip(&self) -> io::Result<()> {
        let name_ok = self
            .root
            .read_string(format!("{}/name", self.chip_dir))
            .as_deref()
            == Some(self.chip_name.as_str());
        let device_ok = self
            .root
            .canonical(format!("{}/device", self.chip_dir))
            .map(|p| p.display().to_string())
            == self.device_path;
        if name_ok && device_ok {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the fan device changed since discovery; refusing to write",
            ))
        }
    }

    fn read_u32(&self, file: String) -> io::Result<u32> {
        self.root.read_parse(file).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "unreadable or malformed value")
        })
    }
}

impl FanIo for HwmonFanIo {
    fn channels(&self) -> Vec<FanChannelInfo> {
        self.channels.iter().map(|(c, _)| c.clone()).collect()
    }

    fn read_rpm(&self, key: &str) -> io::Result<u32> {
        let i = self.index(key)?;
        self.verify_chip()?;
        self.read_u32(format!("{}/fan{i}_input", self.chip_dir))
    }

    fn read_mode(&self, key: &str) -> io::Result<HwFanMode> {
        let i = self.index(key)?;
        self.verify_chip()?;
        let v = self.read_u32(format!("{}/pwm{i}_enable", self.chip_dir))?;
        HwFanMode::from_enable(v)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown fan mode value"))
    }

    fn write_mode(&self, key: &str, mode: HwFanMode) -> io::Result<()> {
        let i = self.index(key)?;
        self.verify_chip()?;
        self.root
            .write_attr(format!("{}/pwm{i}_enable", self.chip_dir), mode.to_enable())
    }

    fn read_duty(&self, key: &str) -> io::Result<Percent> {
        let i = self.index(key)?;
        self.verify_chip()?;
        Ok(self
            .scale
            .from_raw(self.read_u32(format!("{}/pwm{i}", self.chip_dir))?))
    }

    fn write_duty(&self, key: &str, duty: Percent) -> io::Result<()> {
        let i = self.index(key)?;
        self.verify_chip()?;
        self.root.write_attr(
            format!("{}/pwm{i}", self.chip_dir),
            &self.scale.to_raw(duty).to_string(),
        )
    }

    fn describe(&self) -> String {
        format!(
            "{}#{}",
            self.chip_dir,
            self.device_path.as_deref().unwrap_or("?")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rq_testkit::presets;

    type R = Result<(), Box<dyn std::error::Error>>;
    type Setup = (rq_testkit::FakeSystem, SystemRoot, Arc<dyn FanIo>);

    fn setup() -> Result<Setup, Box<dyn std::error::Error>> {
        let fs = presets::anv15_51(true)?;
        presets::add_acer_pwm(&fs)?;
        let root = SystemRoot::at(fs.path());
        let snap = SystemSnapshot::discover(&root);
        let io = HwmonFanIo::select(&root, &snap).ok_or("no fan io")?;
        Ok((fs, root, io))
    }

    #[test]
    fn finds_both_fans_with_role_names() -> R {
        let (_, _, io) = setup()?;
        let ch = io.channels();
        assert_eq!(ch.len(), 2);
        assert_eq!(
            (ch[0].key.as_str(), ch[0].name.as_str(), ch[0].role),
            ("acer/1", "cpu", FanRole::Cpu)
        );
        assert_eq!(
            (ch[1].key.as_str(), ch[1].name.as_str(), ch[1].role),
            ("acer/2", "gpu", FanRole::Gpu)
        );
        Ok(())
    }

    #[test]
    fn not_selected_without_pwm_files() -> R {
        let fs = presets::anv15_51(true)?; // fan RPM only, as on the stock kernel
        let root = SystemRoot::at(fs.path());
        let snap = SystemSnapshot::discover(&root);
        assert!(HwmonFanIo::select(&root, &snap).is_none());
        Ok(())
    }

    #[test]
    fn modes_and_duty_round_trip() -> R {
        let (fs, _, io) = setup()?;
        assert_eq!(io.read_mode("acer/1")?, HwFanMode::Auto);
        io.write_mode("acer/1", HwFanMode::Manual)?;
        assert_eq!(io.read_mode("acer/1")?, HwFanMode::Manual);
        io.write_mode("acer/1", HwFanMode::Max)?;
        assert_eq!(io.read_mode("acer/1")?, HwFanMode::Max);
        io.write_mode("acer/1", HwFanMode::Auto)?;
        assert_eq!(io.read_mode("acer/1")?, HwFanMode::Auto);

        io.write_duty("acer/2", Percent::new(50)?)?;
        let raw = std::fs::read_to_string(fs.path().join("sys/class/hwmon/hwmon7/pwm2"))?;
        assert_eq!(raw.trim(), "128", "50 % on the 0-255 scale");
        assert_eq!(io.read_duty("acer/2")?, Percent::new(50)?);
        assert_eq!(io.read_rpm("acer/1")?, 2331);
        Ok(())
    }

    #[test]
    fn unknown_fans_are_refused() -> R {
        let (_, _, io) = setup()?;
        assert_eq!(
            io.write_duty("acer/9", Percent::MAX).map_err(|e| e.kind()),
            Err(io::ErrorKind::NotFound)
        );
        assert!(io.write_mode("../../x", HwFanMode::Auto).is_err());
        Ok(())
    }

    #[test]
    fn a_renumbered_or_replaced_device_is_never_written() -> R {
        let (fs, _, io) = setup()?;
        // The driver was reloaded: this hwmon number now belongs to a
        // different chip.
        fs.file(
            "/sys/devices/platform/acer-wmi/hwmon/hwmon7/name",
            "something-else",
        )?;
        let err = io
            .write_duty("acer/1", Percent::MAX)
            .expect_err("must refuse");
        assert!(err.to_string().contains("changed since discovery"), "{err}");
        assert!(io.write_mode("acer/1", HwFanMode::Max).is_err());
        let pwm = std::fs::read_to_string(fs.path().join("sys/class/hwmon/hwmon7/pwm1"))?;
        assert_eq!(pwm.trim(), "128", "nothing was written");
        Ok(())
    }

    #[test]
    fn enable_values_follow_the_hwmon_convention() {
        assert_eq!(HwFanMode::from_enable(0), Some(HwFanMode::Max));
        assert_eq!(HwFanMode::from_enable(1), Some(HwFanMode::Manual));
        assert_eq!(HwFanMode::from_enable(2), Some(HwFanMode::Auto));
        assert_eq!(HwFanMode::from_enable(5), Some(HwFanMode::Auto));
        assert_eq!(HwFanMode::from_enable(9), None);
    }
}
