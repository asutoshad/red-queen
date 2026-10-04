//! Fan-control safety: limits and the monitor that decides when manual
//! control must be abandoned in favour of the firmware's automatic control.
//!
//! This is *supplemental* protection. The kernel's thermal handling and the
//! firmware remain authoritative; the monitor only ever hands control back
//! to them, it never tries to out-think them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{MilliCelsius, Percent, TelemetrySample};

/// Manual fan speed can never be set below this, whatever the
/// configuration says. A fan driven this low can stall.
pub const HARD_MIN_PERCENT: u8 = 20;

/// Validated safety limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafetyConfig {
    /// Lowest manual fan speed accepted.
    pub min_percent: Percent,
    /// CPU temperature at which manual control is abandoned.
    pub critical_cpu: MilliCelsius,
    /// GPU temperature at which manual control is abandoned.
    pub critical_gpu: MilliCelsius,
    /// Consecutive samples a commanded fan may read 0 / missing.
    pub fan_stall_samples: u32,
    /// Consecutive samples the CPU temperature may be unreadable while
    /// manual control is active.
    pub sensor_loss_samples: u32,
    /// How long manual control stays refused after a safety trip.
    pub lockout_secs: u64,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            min_percent: Percent::new(30).unwrap_or(Percent::MAX),
            critical_cpu: MilliCelsius(90_000),
            critical_gpu: MilliCelsius(85_000),
            fan_stall_samples: 5,
            sensor_loss_samples: 10,
            lockout_secs: 60,
        }
    }
}

/// The on-disk form (`/etc/red-queen/safety.toml`). Every field is optional;
/// unknown fields are rejected so a typo can't silently disable a limit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SafetyFile {
    /// Lowest manual fan speed, percent (20–100).
    pub min_fan_percent: Option<u8>,
    /// CPU critical temperature, °C (70–100).
    pub critical_cpu_celsius: Option<u32>,
    /// GPU critical temperature, °C (70–95).
    pub critical_gpu_celsius: Option<u32>,
    /// Stalled-fan samples before a trip (3–60).
    pub fan_stall_samples: Option<u32>,
    /// Lost-sensor samples before a trip (5–120).
    pub sensor_loss_samples: Option<u32>,
    /// Lockout after a trip, seconds (10–3600).
    pub lockout_secs: Option<u64>,
}

impl SafetyFile {
    /// Applies the file over the defaults. Out-of-range values are clamped
    /// to the nearest allowed value and reported in the returned warnings,
    /// so a bad config can make the limits *stricter than asked for or
    /// default*, never dangerously loose.
    pub fn into_config(self) -> (SafetyConfig, Vec<String>) {
        let d = SafetyConfig::default();
        let mut warnings = Vec::new();
        let mut clamp = |name: &str, value: Option<u64>, lo: u64, hi: u64, default: u64| -> u64 {
            match value {
                None => default,
                Some(v) if (lo..=hi).contains(&v) => v,
                Some(v) => {
                    let c = v.clamp(lo, hi);
                    warnings.push(format!("{name} = {v} is outside {lo}..={hi}; using {c}"));
                    c
                }
            }
        };
        let min = clamp(
            "min_fan_percent",
            self.min_fan_percent.map(u64::from),
            u64::from(HARD_MIN_PERCENT),
            100,
            u64::from(d.min_percent.get()),
        );
        let cpu = clamp(
            "critical_cpu_celsius",
            self.critical_cpu_celsius.map(u64::from),
            70,
            100,
            90,
        );
        let gpu = clamp(
            "critical_gpu_celsius",
            self.critical_gpu_celsius.map(u64::from),
            70,
            95,
            85,
        );
        let stall = clamp(
            "fan_stall_samples",
            self.fan_stall_samples.map(u64::from),
            3,
            60,
            5,
        );
        let lost = clamp(
            "sensor_loss_samples",
            self.sensor_loss_samples.map(u64::from),
            5,
            120,
            10,
        );
        let lockout = clamp("lockout_secs", self.lockout_secs, 10, 3600, d.lockout_secs);
        let config = SafetyConfig {
            min_percent: Percent::new(u8::try_from(min).unwrap_or(100)).unwrap_or(Percent::MAX),
            critical_cpu: MilliCelsius(i32::try_from(cpu * 1000).unwrap_or(90_000)),
            critical_gpu: MilliCelsius(i32::try_from(gpu * 1000).unwrap_or(85_000)),
            fan_stall_samples: u32::try_from(stall).unwrap_or(5),
            sensor_loss_samples: u32::try_from(lost).unwrap_or(10),
            lockout_secs: lockout,
        };
        (config, warnings)
    }
}

/// Why manual control was abandoned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum TripReason {
    /// The CPU reached its critical temperature.
    CpuCritical {
        /// Temperature when it tripped.
        temperature: MilliCelsius,
    },
    /// The GPU reached its critical temperature.
    GpuCritical {
        /// Temperature when it tripped.
        temperature: MilliCelsius,
    },
    /// A fan being driven manually isn't spinning.
    FanStalled {
        /// Which fan.
        fan: String,
    },
    /// The CPU temperature can't be read, so nothing can be supervised.
    SensorLost,
    /// A hardware write was refused or didn't take effect.
    ControlFailed {
        /// What went wrong.
        detail: String,
    },
    /// The fan interface disappeared (driver unloaded).
    InterfaceLost,
}

impl std::fmt::Display for TripReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CpuCritical { temperature } => {
                write!(f, "the CPU reached {:.0} °C", temperature.celsius())
            }
            Self::GpuCritical { temperature } => {
                write!(f, "the GPU reached {:.0} °C", temperature.celsius())
            }
            Self::FanStalled { fan } => write!(f, "the {fan} fan stopped responding"),
            Self::SensorLost => write!(f, "the CPU temperature became unreadable"),
            Self::ControlFailed { detail } => write!(f, "a fan control write failed: {detail}"),
            Self::InterfaceLost => write!(f, "the fan interface disappeared"),
        }
    }
}

/// A fan currently under manual control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisedFan {
    /// Fan identifier, matching [`crate::FanReading::id`].
    pub id: String,
}

/// Watches telemetry while fans are under manual control.
#[derive(Debug, Clone)]
pub struct SafetyMonitor {
    config: SafetyConfig,
    stalled: BTreeMap<String, u32>,
    cpu_missing: u32,
}

impl SafetyMonitor {
    /// A monitor with the given limits.
    pub fn new(config: SafetyConfig) -> Self {
        Self {
            config,
            stalled: BTreeMap::new(),
            cpu_missing: 0,
        }
    }

    /// Forgets all counters (call when manual control starts or ends).
    pub fn reset(&mut self) {
        self.stalled.clear();
        self.cpu_missing = 0;
    }

    /// Looks at one sample. Returns a reason if manual control must be
    /// abandoned. Does nothing while no fan is under manual control: the
    /// firmware is in charge then.
    pub fn observe(
        &mut self,
        sample: &TelemetrySample,
        supervised: &[SupervisedFan],
    ) -> Option<TripReason> {
        if supervised.is_empty() {
            self.reset();
            return None;
        }

        match sample.cpu.temperature {
            Some(t) if t >= self.config.critical_cpu => {
                return Some(TripReason::CpuCritical { temperature: t });
            }
            Some(_) => self.cpu_missing = 0,
            None => {
                self.cpu_missing += 1;
                if self.cpu_missing >= self.config.sensor_loss_samples {
                    return Some(TripReason::SensorLost);
                }
            }
        }

        // An asleep GPU makes no heat and has no reading; only an awake
        // GPU with a real reading is checked.
        if let Some(gpu) = &sample.gpu
            && !gpu.asleep
            && let Some(t) = gpu.temperature
            && t >= self.config.critical_gpu
        {
            return Some(TripReason::GpuCritical { temperature: t });
        }

        for fan in supervised {
            let spinning = sample
                .fans
                .iter()
                .find(|r| r.id == fan.id)
                .and_then(|r| r.rpm)
                .is_some_and(|rpm| rpm.0 > 0);
            let count = self.stalled.entry(fan.id.clone()).or_insert(0);
            if spinning {
                *count = 0;
            } else {
                *count += 1;
                if *count >= self.config.fan_stall_samples {
                    return Some(TripReason::FanStalled {
                        fan: fan.id.clone(),
                    });
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CpuStatus, FanReading, FanRole, GpuStatus, Rpm};

    fn sample(
        cpu: Option<i32>,
        gpu: Option<(bool, Option<i32>)>,
        fans: &[(&str, Option<u32>)],
    ) -> TelemetrySample {
        TelemetrySample {
            cpu: CpuStatus {
                temperature: cpu.map(MilliCelsius),
                ..Default::default()
            },
            gpu: gpu.map(|(asleep, t)| GpuStatus {
                asleep,
                temperature: t.map(MilliCelsius),
            }),
            fans: fans
                .iter()
                .map(|(id, rpm)| FanReading {
                    id: (*id).into(),
                    role: FanRole::Cpu,
                    rpm: rpm.map(Rpm),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn supervised(ids: &[&str]) -> Vec<SupervisedFan> {
        ids.iter()
            .map(|i| SupervisedFan { id: (*i).into() })
            .collect()
    }

    fn monitor() -> SafetyMonitor {
        SafetyMonitor::new(SafetyConfig::default())
    }

    #[test]
    fn nothing_is_supervised_without_manual_control() {
        let mut m = monitor();
        let hot = sample(Some(120_000), None, &[]);
        assert_eq!(m.observe(&hot, &[]), None, "firmware is in charge; no trip");
    }

    #[test]
    fn cpu_critical_trips_at_the_threshold() {
        let mut m = monitor();
        let s = |t| sample(Some(t), None, &[("cpu", Some(3000))]);
        assert_eq!(m.observe(&s(89_999), &supervised(&["cpu"])), None);
        assert_eq!(
            m.observe(&s(90_000), &supervised(&["cpu"])),
            Some(TripReason::CpuCritical {
                temperature: MilliCelsius(90_000)
            })
        );
    }

    #[test]
    fn gpu_critical_only_counts_when_awake_with_a_reading() {
        let mut m = monitor();
        let fans = [("cpu", Some(3000))];
        let sup = supervised(&["cpu"]);
        assert_eq!(
            m.observe(
                &sample(Some(50_000), Some((true, Some(99_000))), &fans),
                &sup
            ),
            None
        );
        assert_eq!(
            m.observe(&sample(Some(50_000), Some((false, None)), &fans), &sup),
            None
        );
        assert_eq!(
            m.observe(
                &sample(Some(50_000), Some((false, Some(84_999))), &fans),
                &sup
            ),
            None
        );
        assert_eq!(
            m.observe(
                &sample(Some(50_000), Some((false, Some(85_000))), &fans),
                &sup
            ),
            Some(TripReason::GpuCritical {
                temperature: MilliCelsius(85_000)
            })
        );
    }

    #[test]
    fn stalled_fan_trips_after_consecutive_bad_samples_only() {
        let mut m = monitor();
        let sup = supervised(&["cpu"]);
        let bad = sample(Some(50_000), None, &[("cpu", Some(0))]);
        let good = sample(Some(50_000), None, &[("cpu", Some(2500))]);
        for _ in 0..4 {
            assert_eq!(m.observe(&bad, &sup), None);
        }
        assert_eq!(
            m.observe(&good, &sup),
            None,
            "a good reading resets the count"
        );
        for _ in 0..4 {
            assert_eq!(m.observe(&bad, &sup), None);
        }
        assert_eq!(
            m.observe(&bad, &sup),
            Some(TripReason::FanStalled { fan: "cpu".into() })
        );
    }

    #[test]
    fn a_missing_or_unreadable_fan_counts_as_stalled() {
        let mut m = monitor();
        let sup = supervised(&["gpu"]);
        let missing = sample(Some(50_000), None, &[("cpu", Some(2500))]);
        let unreadable = sample(Some(50_000), None, &[("gpu", None)]);
        for _ in 0..2 {
            assert_eq!(m.observe(&missing, &sup), None);
            assert_eq!(m.observe(&unreadable, &sup), None);
        }
        assert_eq!(
            m.observe(&missing, &sup),
            Some(TripReason::FanStalled { fan: "gpu".into() })
        );
    }

    #[test]
    fn losing_the_cpu_sensor_trips() {
        let mut m = monitor();
        let sup = supervised(&["cpu"]);
        let blind = sample(None, None, &[("cpu", Some(2500))]);
        for _ in 0..9 {
            assert_eq!(m.observe(&blind, &sup), None);
        }
        assert_eq!(m.observe(&blind, &sup), Some(TripReason::SensorLost));
    }

    #[test]
    fn counters_reset_when_manual_control_ends() {
        let mut m = monitor();
        let bad = sample(Some(50_000), None, &[("cpu", Some(0))]);
        for _ in 0..4 {
            m.observe(&bad, &supervised(&["cpu"]));
        }
        m.observe(&bad, &[]); // control ended
        assert_eq!(
            m.observe(&bad, &supervised(&["cpu"])),
            None,
            "starts from zero again"
        );
    }

    #[test]
    fn config_file_is_clamped_never_loosened() {
        let (c, warnings) = SafetyFile {
            min_fan_percent: Some(0),
            critical_cpu_celsius: Some(200),
            critical_gpu_celsius: Some(10),
            fan_stall_samples: Some(1000),
            sensor_loss_samples: Some(0),
            lockout_secs: Some(0),
        }
        .into_config();
        assert_eq!(
            c.min_percent.get(),
            HARD_MIN_PERCENT,
            "the floor can't be lowered"
        );
        assert_eq!(c.critical_cpu, MilliCelsius(100_000));
        assert_eq!(c.critical_gpu, MilliCelsius(70_000));
        assert_eq!(c.fan_stall_samples, 60);
        assert_eq!(c.sensor_loss_samples, 5);
        assert_eq!(c.lockout_secs, 10);
        assert_eq!(warnings.len(), 6);
    }

    #[test]
    fn defaults_and_empty_file_agree() {
        let (c, warnings) = SafetyFile::default().into_config();
        assert_eq!(c, SafetyConfig::default());
        assert!(warnings.is_empty());
        assert!(c.min_percent.get() >= HARD_MIN_PERCENT);
    }

    #[test]
    fn trip_reasons_serialize_with_a_code() {
        let v = serde_json::to_value(TripReason::FanStalled { fan: "cpu".into() }).expect("json");
        assert_eq!(v["code"], "fan_stalled");
        assert_eq!(v["fan"], "cpu");
    }
}
