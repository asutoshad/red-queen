//! Per-model knowledge that can't be discovered from the kernel.
//!
//! Kept deliberately small: everything that *can* be detected is detected.
//! An entry exists only for models that have been tested, and each claim is
//! tied to the BIOS versions it was tested on, because firmware updates can
//! change behaviour.

use rq_core::{Feature, HardwareIdentity};

/// Facts about a tested model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelQuirks {
    /// DMI product name.
    pub product: &'static str,
    /// acer-wmi needs `predator_v4=1` for profiles and fan monitoring
    /// because the kernel lacks a DMI entry for this model.
    pub needs_predator_v4: bool,
    /// The kernel's acer-wmi has no PWM support for this model yet.
    pub pwm_pending_upstream: bool,
    /// BIOS versions the claims below were verified on.
    pub verified_bios: &'static [&'static str],
    /// Features tested on real hardware (on a verified BIOS).
    pub verified_features: &'static [Feature],
    /// Thermal profiles the firmware advertises but rejects (on a verified
    /// BIOS), as kernel `platform_profile` names.
    pub rejected_profiles: &'static [&'static str],
}

impl ModelQuirks {
    /// Whether the claims apply to this exact machine: the product matches
    /// and the BIOS is one the model was verified on.
    pub fn verified_on(&self, identity: &HardwareIdentity) -> bool {
        identity.is_acer_product(self.product)
            && identity
                .bios_version
                .as_deref()
                .is_some_and(|v| self.verified_bios.contains(&v))
    }
}

const MODELS: &[ModelQuirks] = &[ModelQuirks {
    product: "Nitro ANV15-51",
    needs_predator_v4: true,
    pwm_pending_upstream: true,
    // Tested 2026-10-04, kernel 7.1.5, `predator_v4=1`: quiet, low-power,
    // balanced and balanced-performance switch and read back correctly;
    // `performance` is rejected by the firmware (EIO); fan RPM readings
    // follow the profile.
    verified_bios: &["V1.60"],
    verified_features: &[Feature::ThermalProfiles, Feature::FanTelemetry],
    rejected_profiles: &["performance"],
}];

/// Looks up a tested model by product name. Use
/// [`ModelQuirks::verified_on`] before trusting its test results.
pub fn lookup(identity: &HardwareIdentity) -> Option<&'static ModelQuirks> {
    MODELS.iter().find(|m| identity.is_acer_product(m.product))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(bios: &str) -> HardwareIdentity {
        HardwareIdentity {
            vendor: Some("Acer".into()),
            product_name: Some("Nitro ANV15-51".into()),
            bios_version: Some(bios.into()),
            ..Default::default()
        }
    }

    #[test]
    fn claims_are_tied_to_the_tested_bios() {
        let m = lookup(&identity("V1.60")).expect("model known");
        assert!(m.verified_on(&identity("V1.60")));
        assert!(
            !m.verified_on(&identity("V1.61")),
            "a newer BIOS may behave differently"
        );
        assert!(!m.verified_on(&HardwareIdentity::default()));
    }

    #[test]
    fn unknown_models_have_no_entry() {
        let other = HardwareIdentity {
            vendor: Some("Acer".into()),
            product_name: Some("Nitro AN515-58".into()),
            ..Default::default()
        };
        assert!(lookup(&other).is_none());
    }
}
