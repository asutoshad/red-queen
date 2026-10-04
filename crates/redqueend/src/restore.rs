//! Standalone "give the fans back to the firmware" operation.
//!
//! Run by the service's stop hook (`redqueend --restore-auto`), so even a
//! daemon killed by SIGKILL or a crash leaves the fans under firmware
//! control. It talks to the hardware directly: no D-Bus, no polkit.

use std::sync::Arc;

use rq_core::SafetyConfig;
use rq_hardware::fan::HwmonFanIo;
use rq_hardware::{SystemRoot, SystemSnapshot};

use crate::fans::{FanController, FanError};
use crate::persist::ManualFlag;

/// What the restore did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restored {
    /// The marker was not set: nothing to undo, nothing was written.
    NothingToDo,
    /// The marker was set but there is no controllable fan interface right
    /// now. The marker stays so a later start can finish the job.
    NoInterface,
    /// Every fan was returned to firmware control and confirmed.
    Done,
}

/// Restores automatic fan control if the marker says it may be needed.
pub fn restore_fans_standalone(
    root: &SystemRoot,
    flag: Arc<dyn ManualFlag>,
    safety: SafetyConfig,
) -> Result<Restored, FanError> {
    if !flag.is_set() {
        return Ok(Restored::NothingToDo);
    }
    let snap = SystemSnapshot::discover(root);
    let Some(io) = HwmonFanIo::select(root, &snap) else {
        return Ok(Restored::NoInterface);
    };
    FanController::new(Some(io), safety, flag).set_auto()?;
    Ok(Restored::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::MemoryFlag;
    use rq_testkit::presets;

    type R = Result<(), Box<dyn std::error::Error>>;

    fn mode(fs: &rq_testkit::FakeSystem, n: u8) -> Result<String, std::io::Error> {
        let raw = std::fs::read_to_string(
            fs.path()
                .join(format!("sys/class/hwmon/hwmon7/pwm{n}_enable")),
        )?;
        Ok(raw.trim().to_owned())
    }

    #[test]
    fn restores_fans_left_in_manual_mode_by_a_dead_daemon() -> R {
        let fs = presets::anv15_51(true)?;
        presets::add_acer_pwm(&fs)?;
        for n in [1, 2] {
            fs.file_mode(
                &format!("/sys/devices/platform/acer-wmi/hwmon/hwmon7/pwm{n}_enable"),
                "1",
                0o644,
            )?;
        }
        let flag = Arc::new(MemoryFlag::new());
        flag.set()?;
        let out = restore_fans_standalone(
            &SystemRoot::at(fs.path()),
            flag.clone(),
            SafetyConfig::default(),
        )?;
        assert_eq!(out, Restored::Done);
        assert_eq!(
            (mode(&fs, 1)?, mode(&fs, 2)?),
            ("2".into(), "2".into()),
            "both fans automatic"
        );
        assert!(!flag.is_set());
        Ok(())
    }

    #[test]
    fn writes_nothing_when_the_marker_is_clear() -> R {
        let fs = presets::anv15_51(true)?;
        presets::add_acer_pwm(&fs)?;
        fs.file_mode(
            "/sys/devices/platform/acer-wmi/hwmon/hwmon7/pwm1_enable",
            "1",
            0o644,
        )?;
        let out = restore_fans_standalone(
            &SystemRoot::at(fs.path()),
            Arc::new(MemoryFlag::new()),
            SafetyConfig::default(),
        )?;
        assert_eq!(out, Restored::NothingToDo);
        assert_eq!(
            mode(&fs, 1)?,
            "1",
            "left alone: this run isn't ours to undo"
        );
        Ok(())
    }

    #[test]
    fn without_an_interface_the_marker_is_kept() -> R {
        let fs = presets::anv15_51(true)?; // fan RPM only, no pwm files
        let flag = Arc::new(MemoryFlag::new());
        flag.set()?;
        let out = restore_fans_standalone(
            &SystemRoot::at(fs.path()),
            flag.clone(),
            SafetyConfig::default(),
        )?;
        assert_eq!(out, Restored::NoInterface);
        assert!(flag.is_set());
        Ok(())
    }
}
