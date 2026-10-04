//! Per-model knowledge that can't be discovered from the kernel.
//!
//! Kept deliberately small: everything that *can* be detected is detected.
//! Entries here only exist for models that have been tested.

use rq_core::HardwareIdentity;

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
}

const MODELS: &[ModelQuirks] = &[ModelQuirks {
    product: "Nitro ANV15-51",
    needs_predator_v4: true,
    pwm_pending_upstream: true,
}];

/// Looks up a tested model.
pub fn lookup(identity: &HardwareIdentity) -> Option<&'static ModelQuirks> {
    MODELS.iter().find(|m| identity.is_acer_product(m.product))
}
