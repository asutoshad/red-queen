//! Thermal / performance profiles.

use serde::{Deserialize, Serialize};

/// A profile name that doesn't pass [`ThermalProfileId::parse_untrusted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("profile names are 1-64 characters of a-z, 0-9 and '-'")]
pub struct InvalidProfileName;

/// A platform thermal profile, independent of how a backend names it.
///
/// Known Linux `platform_profile` names map to dedicated variants; anything
/// else is preserved in [`ThermalProfileId::Other`] so it can be reported,
/// but code must never branch on its string contents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum ThermalProfileId {
    /// `low-power`
    LowPower,
    /// `cool`
    Cool,
    /// `quiet`
    Quiet,
    /// `balanced`
    Balanced,
    /// `balanced-performance`
    BalancedPerformance,
    /// `performance`
    Performance,
    /// `max-power`
    MaxPower,
    /// `custom`
    Custom,
    /// A name this version doesn't know.
    Other(String),
}

impl ThermalProfileId {
    /// Parses a kernel `platform_profile` name.
    pub fn from_kernel_name(name: &str) -> Self {
        match name.trim() {
            "low-power" => Self::LowPower,
            "cool" => Self::Cool,
            "quiet" => Self::Quiet,
            "balanced" => Self::Balanced,
            "balanced-performance" => Self::BalancedPerformance,
            "performance" => Self::Performance,
            "max-power" => Self::MaxPower,
            "custom" => Self::Custom,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The kernel `platform_profile` name.
    pub fn kernel_name(&self) -> &str {
        match self {
            Self::LowPower => "low-power",
            Self::Cool => "cool",
            Self::Quiet => "quiet",
            Self::Balanced => "balanced",
            Self::BalancedPerformance => "balanced-performance",
            Self::Performance => "performance",
            Self::MaxPower => "max-power",
            Self::Custom => "custom",
            Self::Other(name) => name,
        }
    }

    /// Whether this is a profile this version understands.
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Other(_))
    }

    /// Parses a profile name received from a client.
    ///
    /// Stricter than [`Self::from_kernel_name`]: the name must be 1–64
    /// characters of `a-z`, `0-9` and `-`. Nothing else is accepted, so a
    /// client can never smuggle path separators or control characters
    /// towards a sysfs write.
    pub fn parse_untrusted(name: &str) -> Result<Self, InvalidProfileName> {
        let ok = (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if ok {
            Ok(Self::from_kernel_name(name))
        } else {
            Err(InvalidProfileName)
        }
    }

    /// Parses a whitespace-separated `platform_profile_choices` list.
    pub fn parse_choices(list: &str) -> Vec<Self> {
        list.split_whitespace()
            .map(Self::from_kernel_name)
            .collect()
    }
}

impl From<ThermalProfileId> for String {
    fn from(id: ThermalProfileId) -> Self {
        id.kernel_name().to_owned()
    }
}

impl From<String> for ThermalProfileId {
    fn from(name: String) -> Self {
        Self::from_kernel_name(&name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [&str; 8] = [
        "low-power",
        "cool",
        "quiet",
        "balanced",
        "balanced-performance",
        "performance",
        "max-power",
        "custom",
    ];

    #[test]
    fn known_names_round_trip() {
        for name in ALL {
            let id = ThermalProfileId::from_kernel_name(name);
            assert!(id.is_known(), "{name}");
            assert_eq!(id.kernel_name(), name);
        }
    }

    #[test]
    fn unknown_names_are_preserved() {
        let id = ThermalProfileId::from_kernel_name("turbo");
        assert_eq!(id, ThermalProfileId::Other("turbo".into()));
        assert!(!id.is_known());
        assert_eq!(id.kernel_name(), "turbo");
    }

    #[test]
    fn parses_anv15_51_choices() {
        let choices = ThermalProfileId::parse_choices(
            "low-power quiet balanced balanced-performance performance\n",
        );
        assert_eq!(
            choices,
            vec![
                ThermalProfileId::LowPower,
                ThermalProfileId::Quiet,
                ThermalProfileId::Balanced,
                ThermalProfileId::BalancedPerformance,
                ThermalProfileId::Performance,
            ]
        );
    }

    #[test]
    fn untrusted_names_are_strictly_validated() {
        for ok in ["quiet", "balanced-performance", "low-power", "mode2", "x"] {
            assert!(ThermalProfileId::parse_untrusted(ok).is_ok(), "{ok}");
        }
        let long = "a".repeat(65);
        for bad in [
            "",
            "Quiet",
            "qu iet",
            "quiet\n",
            "../x",
            "a/b",
            "quiet\0",
            "ünïcode",
            "a_b",
            long.as_str(),
        ] {
            assert!(ThermalProfileId::parse_untrusted(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            ThermalProfileId::parse_untrusted("turbo"),
            Ok(ThermalProfileId::Other("turbo".into()))
        );
    }

    #[test]
    fn serializes_as_kernel_name() {
        let json = serde_json::to_string(&ThermalProfileId::BalancedPerformance).ok();
        assert_eq!(json.as_deref(), Some("\"balanced-performance\""));
    }
}
