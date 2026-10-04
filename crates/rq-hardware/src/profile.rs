//! Reading and changing the platform thermal profile.

use std::fmt::Debug;
use std::io;
use std::sync::Arc;

use rq_core::ThermalProfileId;
use rustix::io::Errno;

use crate::platform_profile::PlatformProfileInfo;
use crate::root::SystemRoot;

/// The acer-wmi platform profile handler name.
const ACER_HANDLER: &str = "acer-wmi";
const LEGACY: &str = "/sys/firmware/acpi/platform_profile";
const LEGACY_CHOICES: &str = "/sys/firmware/acpi/platform_profile_choices";

/// Access to one platform profile interface. A trait so the logic that
/// sits on top (read-back verification, rejection handling) can be tested
/// with simulated firmware.
pub trait ProfileIo: Send + Sync + Debug {
    /// Profiles the driver advertises right now.
    fn choices(&self) -> Vec<ThermalProfileId>;
    /// The active profile, read from the kernel.
    fn read_active(&self) -> io::Result<ThermalProfileId>;
    /// Asks the kernel to switch. Success means the write was accepted,
    /// not that the profile changed: callers must read back.
    fn write_active(&self, profile: &ThermalProfileId) -> io::Result<()>;
    /// Identifies the interface (for logs and change detection).
    fn describe(&self) -> String;
}

/// A profile interface backed by sysfs files.
#[derive(Debug)]
pub struct SysfsProfileIo {
    root: SystemRoot,
    profile_path: String,
    choices_path: String,
}

/// Where a profile interface lives in sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePaths {
    /// The active-profile attribute.
    pub profile: String,
    /// The advertised-choices attribute.
    pub choices: String,
}

/// Picks the interface to control: the acer-wmi handler if present, else
/// the only handler, else the legacy firmware file. With several unrelated
/// handlers and no acer one, nothing is selected, because guessing which to
/// write would be unsafe.
///
/// Telemetry reads the same interface the daemon writes, so what is shown
/// is always what is controlled.
pub fn select_paths(info: &PlatformProfileInfo) -> Option<ProfilePaths> {
    let handler = info
        .handlers
        .iter()
        .find(|h| h.name.as_deref() == Some(ACER_HANDLER))
        .or_else(|| (info.handlers.len() == 1).then(|| &info.handlers[0]));
    match handler {
        Some(h) => {
            let dir = format!("/sys/class/platform-profile/{}", h.id);
            Some(ProfilePaths {
                profile: format!("{dir}/profile"),
                choices: format!("{dir}/choices"),
            })
        }
        None if info.handlers.is_empty() && info.legacy.is_some() => Some(ProfilePaths {
            profile: LEGACY.to_owned(),
            choices: LEGACY_CHOICES.to_owned(),
        }),
        None => None,
    }
}

impl SysfsProfileIo {
    /// Builds the interface selected by [`select_paths`].
    pub fn select(root: &SystemRoot, info: &PlatformProfileInfo) -> Option<Arc<dyn ProfileIo>> {
        let paths = select_paths(info)?;
        Some(Arc::new(Self {
            root: root.clone(),
            profile_path: paths.profile,
            choices_path: paths.choices,
        }))
    }
}

impl ProfileIo for SysfsProfileIo {
    fn choices(&self) -> Vec<ThermalProfileId> {
        self.root
            .read_string(&self.choices_path)
            .map(|s| ThermalProfileId::parse_choices(&s))
            .unwrap_or_default()
    }

    fn read_active(&self) -> io::Result<ThermalProfileId> {
        self.root
            .read_string(&self.profile_path)
            .map(|s| ThermalProfileId::from_kernel_name(&s))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "profile attribute unreadable"))
    }

    fn write_active(&self, profile: &ThermalProfileId) -> io::Result<()> {
        // Defence in depth: the daemon already validated this against the
        // advertised choices, but never put anything unexpected in a path
        // write.
        let name = profile.kernel_name();
        if ThermalProfileId::parse_untrusted(name).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid profile name",
            ));
        }
        self.root.write_attr(&self.profile_path, name)
    }

    fn describe(&self) -> String {
        self.profile_path.clone()
    }
}

/// Whether a failed write means the firmware refused this profile (as
/// opposed to a transient problem worth retrying).
pub fn is_firmware_rejection(e: &io::Error) -> bool {
    matches!(
        Errno::from_io_error(e),
        Some(Errno::IO | Errno::INVAL | Errno::OPNOTSUPP | Errno::NOSYS)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform_profile;
    use rq_testkit::presets;

    type R = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn selects_the_acer_handler_and_round_trips() -> R {
        let fs = presets::anv15_51(true)?;
        let root = SystemRoot::at(fs.path());
        let io = SysfsProfileIo::select(&root, &platform_profile::discover(&root)).ok_or("none")?;
        assert!(io.describe().ends_with("platform-profile-0/profile"));
        assert_eq!(io.read_active()?, ThermalProfileId::Balanced);
        assert_eq!(io.choices().len(), 5);

        io.write_active(&ThermalProfileId::Quiet)?;
        assert_eq!(
            io.read_active()?,
            ThermalProfileId::Quiet,
            "no stale tail from the old value"
        );
        io.write_active(&ThermalProfileId::BalancedPerformance)?;
        assert_eq!(io.read_active()?, ThermalProfileId::BalancedPerformance);
        Ok(())
    }

    #[test]
    fn refuses_unsafe_names_and_missing_files() -> R {
        let fs = presets::anv15_51(true)?;
        let root = SystemRoot::at(fs.path());
        let io = SysfsProfileIo::select(&root, &platform_profile::discover(&root)).ok_or("none")?;
        let bad = ThermalProfileId::Other("../../x".into());
        assert_eq!(
            io.write_active(&bad).map_err(|e| e.kind()),
            Err(io::ErrorKind::InvalidInput)
        );
        // Writes never create files.
        std::fs::remove_file(fs.path().join(io.describe().trim_start_matches('/')))?;
        assert!(io.write_active(&ThermalProfileId::Quiet).is_err());
        assert!(!root.exists(io.describe()));
        Ok(())
    }

    #[test]
    fn nothing_is_selected_when_unsure() -> R {
        let fs = presets::anv15_51(false)?;
        let root = SystemRoot::at(fs.path());
        assert!(SysfsProfileIo::select(&root, &platform_profile::discover(&root)).is_none());
        Ok(())
    }

    #[test]
    fn rejection_classification() {
        for code in [5, 22, 95, 38] {
            assert!(
                is_firmware_rejection(&io::Error::from_raw_os_error(code)),
                "{code}"
            );
        }
        for code in [13, 16, 2, 4] {
            assert!(
                !is_firmware_rejection(&io::Error::from_raw_os_error(code)),
                "{code}"
            );
        }
    }
}
