//! Machine, kernel and OS identity. Contains no personal identifiers.

use serde::{Deserialize, Serialize};

/// Hardware identity from DMI. Serial numbers and UUIDs are never included.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareIdentity {
    /// System vendor, for example `Acer`.
    pub vendor: Option<String>,
    /// Product name, for example `Nitro ANV15-51`.
    pub product_name: Option<String>,
    /// Product family, for example `Acer Nitro V 15`.
    pub product_family: Option<String>,
    /// Product version.
    pub product_version: Option<String>,
    /// Board name.
    pub board_name: Option<String>,
    /// BIOS vendor.
    pub bios_vendor: Option<String>,
    /// BIOS version.
    pub bios_version: Option<String>,
    /// BIOS release date as reported (`MM/DD/YYYY`).
    pub bios_date: Option<String>,
}

impl HardwareIdentity {
    /// Whether this is an Acer machine with the given DMI product name.
    pub fn is_acer_product(&self, product: &str) -> bool {
        self.vendor.as_deref() == Some("Acer") && self.product_name.as_deref() == Some(product)
    }
}

/// The running kernel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelInfo {
    /// Release string, as `uname -r`.
    pub release: Option<String>,
}

/// The operating system, from `os-release`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsInfo {
    /// `ID`, for example `kali`.
    pub id: Option<String>,
    /// `ID_LIKE`, for example `debian`.
    pub id_like: Option<String>,
    /// `VERSION_ID`.
    pub version_id: Option<String>,
    /// `PRETTY_NAME`.
    pub pretty_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_match_requires_vendor() {
        let mut id = HardwareIdentity {
            vendor: Some("Acer".into()),
            product_name: Some("Nitro ANV15-51".into()),
            ..Default::default()
        };
        assert!(id.is_acer_product("Nitro ANV15-51"));
        id.vendor = Some("Other".into());
        assert!(!id.is_acer_product("Nitro ANV15-51"));
    }
}
