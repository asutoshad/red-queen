//! Linux `platform_profile` discovery (legacy file and class API).

use rq_core::ThermalProfileId;
use serde::Serialize;

use crate::root::SystemRoot;

const LEGACY: &str = "/sys/firmware/acpi/platform_profile";
const LEGACY_CHOICES: &str = "/sys/firmware/acpi/platform_profile_choices";
const CLASS: &str = "/sys/class/platform-profile";

/// What the kernel exposes for platform profiles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PlatformProfileInfo {
    /// The legacy `/sys/firmware/acpi/platform_profile` interface.
    pub legacy: Option<ProfileState>,
    /// Handlers registered under `/sys/class/platform-profile`.
    pub handlers: Vec<ProfileHandler>,
}

/// Active profile and advertised choices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileState {
    /// Currently active profile.
    pub active: Option<ThermalProfileId>,
    /// Profiles the driver advertises. Advertised is not the same as
    /// accepted: firmware may still reject some on write.
    pub choices: Vec<ThermalProfileId>,
}

/// One registered profile handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileHandler {
    /// Class entry, for example `platform-profile-0`.
    pub id: String,
    /// Driver name, for example `acer-wmi`.
    pub name: Option<String>,
    /// State of this handler.
    pub state: ProfileState,
}

impl PlatformProfileInfo {
    /// Whether any profile interface exists.
    pub fn available(&self) -> bool {
        self.legacy.is_some() || !self.handlers.is_empty()
    }
}

/// Reads the platform profile interfaces.
pub fn discover(root: &SystemRoot) -> PlatformProfileInfo {
    let legacy = root.exists(LEGACY).then(|| ProfileState {
        active: root
            .read_string(LEGACY)
            .map(|s| ThermalProfileId::from_kernel_name(&s)),
        choices: root
            .read_string(LEGACY_CHOICES)
            .map(|s| ThermalProfileId::parse_choices(&s))
            .unwrap_or_default(),
    });
    let handlers = root
        .list_dir(CLASS)
        .into_iter()
        .map(|id| {
            let dir = format!("{CLASS}/{id}");
            ProfileHandler {
                name: root.read_string(format!("{dir}/name")),
                state: ProfileState {
                    active: root
                        .read_string(format!("{dir}/profile"))
                        .map(|s| ThermalProfileId::from_kernel_name(&s)),
                    choices: root
                        .read_string(format!("{dir}/choices"))
                        .map(|s| ThermalProfileId::parse_choices(&s))
                        .unwrap_or_default(),
                },
                id,
            }
        })
        .collect();
    PlatformProfileInfo { legacy, handlers }
}
