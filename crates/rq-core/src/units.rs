//! Physical units and validated values.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A temperature in millidegrees Celsius, the unit used by the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MilliCelsius(pub i32);

impl MilliCelsius {
    /// Degrees Celsius.
    pub fn celsius(self) -> f64 {
        f64::from(self.0) / 1000.0
    }

    /// Degrees Fahrenheit.
    pub fn fahrenheit(self) -> f64 {
        self.celsius() * 9.0 / 5.0 + 32.0
    }
}

/// The unit a temperature is shown in. Presentation only: hardware values
/// are always kept in [`MilliCelsius`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemperatureUnit {
    /// Degrees Celsius.
    #[default]
    Celsius,
    /// Degrees Fahrenheit.
    Fahrenheit,
}

impl TemperatureUnit {
    /// Converts a temperature to this unit.
    pub fn convert(self, t: MilliCelsius) -> f64 {
        match self {
            Self::Celsius => t.celsius(),
            Self::Fahrenheit => t.fahrenheit(),
        }
    }
}

/// Fan speed in revolutions per minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Rpm(pub u32);

/// A whole percentage, guaranteed to be within `0..=100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "u8")]
pub struct Percent(u8);

/// Returned when a value is outside `0..=100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("percentage {0} is outside 0..=100")]
pub struct PercentOutOfRange(pub i64);

impl Percent {
    /// 0 %.
    pub const ZERO: Self = Self(0);
    /// 100 %.
    pub const MAX: Self = Self(100);

    /// Creates a percentage, rejecting values above 100.
    pub fn new(value: u8) -> Result<Self, PercentOutOfRange> {
        Self::try_from(i64::from(value))
    }

    /// The value in `0..=100`.
    pub fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<i64> for Percent {
    type Error = PercentOutOfRange;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u8::try_from(value)
            .ok()
            .filter(|v| *v <= 100)
            .map(Self)
            .ok_or(PercentOutOfRange(value))
    }
}

impl From<Percent> for u8 {
    fn from(p: Percent) -> Self {
        p.0
    }
}

/// How a fan backend expresses duty cycle in its raw interface.
///
/// The mapping belongs to the backend: a percentage is never written to
/// hardware without going through the backend's scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DutyScale {
    /// Standard hwmon `pwmN`: 0 = 0 %, 255 = 100 %.
    Hwmon255,
}

impl DutyScale {
    /// Largest raw value of this scale.
    pub fn raw_max(self) -> u32 {
        match self {
            Self::Hwmon255 => 255,
        }
    }

    /// Converts a percentage to the raw value, rounding to nearest.
    pub fn to_raw(self, p: Percent) -> u32 {
        let max = self.raw_max();
        (u32::from(p.get()) * max + 50) / 100
    }

    /// Converts a raw value back to a percentage, rounding to nearest.
    /// Values above the scale maximum are clamped to 100 %.
    pub fn from_raw(self, raw: u32) -> Percent {
        let max = self.raw_max();
        let raw = raw.min(max);
        // In range by construction: raw <= max, so the result is <= 100.
        Percent(((raw * 100 + max / 2) / max) as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_rejects_out_of_range() {
        assert!(Percent::try_from(-1).is_err());
        assert!(Percent::try_from(101).is_err());
        assert!(Percent::try_from(i64::MAX).is_err());
        assert_eq!(Percent::try_from(0).map(Percent::get), Ok(0));
        assert_eq!(Percent::try_from(100).map(Percent::get), Ok(100));
    }

    #[test]
    fn percent_deserialize_validates() {
        assert!(serde_json::from_str::<Percent>("101").is_err());
        assert!(serde_json::from_str::<Percent>("-5").is_err());
        assert_eq!(
            serde_json::from_str::<Percent>("42").map(Percent::get).ok(),
            Some(42)
        );
    }

    #[test]
    fn hwmon_scale_endpoints() {
        let s = DutyScale::Hwmon255;
        assert_eq!(s.to_raw(Percent::ZERO), 0);
        assert_eq!(s.to_raw(Percent::MAX), 255);
        assert_eq!(s.from_raw(0), Percent::ZERO);
        assert_eq!(s.from_raw(255), Percent::MAX);
        assert_eq!(s.from_raw(1000), Percent::MAX);
    }

    #[test]
    fn hwmon_scale_round_trips_every_percentage() {
        let s = DutyScale::Hwmon255;
        for v in 0..=100u8 {
            let p = Percent::new(v).expect("in range");
            assert_eq!(s.from_raw(s.to_raw(p)), p, "round trip of {v}%");
        }
    }

    #[test]
    fn hwmon_scale_midpoint() {
        let s = DutyScale::Hwmon255;
        assert_eq!(s.to_raw(Percent::new(50).expect("in range")), 128);
    }

    #[test]
    fn temperature_conversion() {
        let t = MilliCelsius(43_000);
        assert!((t.celsius() - 43.0).abs() < f64::EPSILON);
        assert!((TemperatureUnit::Fahrenheit.convert(t) - 109.4).abs() < 1e-9);
    }
}
