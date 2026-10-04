//! Live telemetry samples and the bounded history buffer.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::{BatteryState, FanRole, MilliCelsius, Rpm, ThermalProfileId};

/// CPU state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CpuStatus {
    /// Overall load, 0–100.
    pub usage_percent: Option<f32>,
    /// Load per logical CPU, 0–100.
    pub per_core_percent: Vec<f32>,
    /// Average current frequency in MHz.
    pub avg_freq_mhz: Option<u32>,
    /// Highest current frequency in MHz.
    pub max_freq_mhz: Option<u32>,
    /// Package temperature.
    pub temperature: Option<MilliCelsius>,
    /// Package power in milliwatts (RAPL; daemon only).
    pub package_power_mw: Option<u32>,
}

/// GPU state (extended with NVML data when available).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuStatus {
    /// The discrete GPU is runtime-suspended; nothing was queried.
    pub asleep: bool,
    /// Temperature.
    pub temperature: Option<MilliCelsius>,
}

/// Memory usage in KiB.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStatus {
    /// `MemTotal`.
    pub total_kib: u64,
    /// `MemAvailable`.
    pub available_kib: u64,
    /// `SwapTotal`.
    pub swap_total_kib: u64,
    /// `SwapFree`.
    pub swap_free_kib: u64,
}

impl MemoryStatus {
    /// Used memory (total minus available).
    pub fn used_kib(&self) -> u64 {
        self.total_kib.saturating_sub(self.available_kib)
    }

    /// Used memory as a percentage.
    pub fn used_percent(&self) -> Option<f32> {
        (self.total_kib > 0).then(|| self.used_kib() as f32 * 100.0 / self.total_kib as f32)
    }
}

/// One fan reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanReading {
    /// Stable identifier, `<hwmon name>/<channel>` (for example `acer/1`).
    pub id: String,
    /// What it cools.
    pub role: FanRole,
    /// Speed, if readable.
    pub rpm: Option<Rpm>,
}

/// Battery summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatterySummary {
    /// Charge level.
    pub percent: Option<u8>,
    /// Charging state.
    pub state: Option<BatteryState>,
    /// Charge or discharge power in milliwatts (always positive).
    pub power_mw: Option<u32>,
}

/// Everything sampled in one telemetry tick.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TelemetrySample {
    /// Unix time in milliseconds.
    pub timestamp_ms: u64,
    /// CPU.
    pub cpu: CpuStatus,
    /// Discrete GPU, if any.
    pub gpu: Option<GpuStatus>,
    /// Memory.
    pub memory: MemoryStatus,
    /// Fans.
    pub fans: Vec<FanReading>,
    /// Primary battery.
    pub battery: Option<BatterySummary>,
    /// AC adapter connected.
    pub ac_online: Option<bool>,
    /// Active thermal profile.
    pub thermal_profile: Option<ThermalProfileId>,
    /// Seconds since boot.
    pub uptime_s: Option<u64>,
}

/// A fixed-capacity ring buffer of samples. Memory use is bounded by
/// `capacity` regardless of how long the daemon runs.
#[derive(Debug, Clone)]
pub struct History {
    samples: VecDeque<TelemetrySample>,
    capacity: usize,
}

impl History {
    /// A buffer holding at most `capacity` samples (minimum 1).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Adds a sample, dropping the oldest when full.
    pub fn push(&mut self, sample: TelemetrySample) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// The newest sample.
    pub fn latest(&self) -> Option<&TelemetrySample> {
        self.samples.back()
    }

    /// Samples from the last `window_ms` milliseconds, oldest first.
    pub fn since(&self, window_ms: u64) -> Vec<TelemetrySample> {
        let Some(newest) = self.latest().map(|s| s.timestamp_ms) else {
            return Vec::new();
        };
        let cutoff = newest.saturating_sub(window_ms);
        self.samples
            .iter()
            .filter(|s| s.timestamp_ms >= cutoff)
            .cloned()
            .collect()
    }

    /// Number of stored samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether no samples are stored.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Maximum number of samples.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> TelemetrySample {
        TelemetrySample {
            timestamp_ms: ms,
            ..Default::default()
        }
    }

    #[test]
    fn history_is_bounded() {
        let mut h = History::new(3);
        for t in 0..10 {
            h.push(at(t));
        }
        assert_eq!(h.len(), 3);
        assert_eq!(h.latest().map(|s| s.timestamp_ms), Some(9));
        assert_eq!(h.since(u64::MAX).first().map(|s| s.timestamp_ms), Some(7));
    }

    #[test]
    fn history_window() {
        let mut h = History::new(100);
        for t in (0..=10_000).step_by(1000) {
            h.push(at(t));
        }
        let w = h.since(3000);
        assert_eq!(
            w.iter().map(|s| s.timestamp_ms).collect::<Vec<_>>(),
            [7000, 8000, 9000, 10000]
        );
        assert!(History::new(5).since(1000).is_empty());
    }

    #[test]
    fn zero_capacity_is_clamped() {
        let mut h = History::new(0);
        h.push(at(1));
        h.push(at(2));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn memory_math() {
        let m = MemoryStatus {
            total_kib: 1000,
            available_kib: 250,
            ..Default::default()
        };
        assert_eq!(m.used_kib(), 750);
        assert_eq!(m.used_percent(), Some(75.0));
        assert_eq!(MemoryStatus::default().used_percent(), None);
    }
}
