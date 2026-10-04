//! Fan control: the state machine that keeps manual control safe.
//!
//! Rules (each is enforced here and tested):
//!
//! * Firmware automatic control is the default and the destination of every
//!   error path.
//! * Before any fan leaves automatic control, a durable marker is written;
//!   if that fails, manual control is refused.
//! * A manual speed below the configured minimum is refused, never
//!   clamped silently and never written. There is no "fan off".
//! * The speed is written *before* the mode switches to manual, so a fan
//!   never runs in manual mode on a stale or zero duty.
//! * Every change is read back; anything unconfirmed is rolled back to
//!   automatic and locks manual control out for a while.
//! * A safety monitor watches temperatures and fan speeds and hands control
//!   back to the firmware by itself.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rq_core::{
    FanMode, Percent, SafetyConfig, SafetyMonitor, SupervisedFan, TelemetrySample, TripReason,
};
use rq_hardware::fan::{FanChannelInfo, FanIo, HwFanMode};
use rq_ipc::{FanInfo, FansInfo, SafetyStatus};
use tracing::{error, info, warn};

use crate::persist::ManualFlag;

/// Why a fan request didn't happen. Maps onto D-Bus errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FanError {
    /// No controllable fans, or manual control can't be made safe.
    Unavailable(String),
    /// A malformed or out-of-range argument.
    InvalidArgument(String),
    /// Manual control is refused for a while after a safety trip.
    LockedOut(String),
    /// A change was accepted but the hardware didn't confirm it.
    NotConfirmed(String),
    /// An unexpected hardware error.
    Failed(String),
}

impl std::fmt::Display for FanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Unavailable(m)
        | Self::InvalidArgument(m)
        | Self::LockedOut(m)
        | Self::NotConfirmed(m)
        | Self::Failed(m)) = self;
        f.write_str(m)
    }
}

impl std::error::Error for FanError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FanState {
    mode: FanMode,
    requested: Option<Percent>,
}

#[derive(Debug, Clone)]
struct Trip {
    reason: TripReason,
    at: Instant,
}

/// Owns the fan interface and everything learned about it.
#[derive(Debug)]
pub struct FanController {
    io: Option<Arc<dyn FanIo>>,
    config: SafetyConfig,
    flag: Arc<dyn ManualFlag>,
    /// Fans that are *not* in automatic control, by channel key.
    state: BTreeMap<String, FanState>,
    monitor: SafetyMonitor,
    trip: Option<Trip>,
}

impl FanController {
    /// A controller over `io` (or none, when the hardware has no
    /// controllable fans).
    pub fn new(
        io: Option<Arc<dyn FanIo>>,
        config: SafetyConfig,
        flag: Arc<dyn ManualFlag>,
    ) -> Self {
        Self {
            io,
            config,
            flag,
            state: BTreeMap::new(),
            monitor: SafetyMonitor::new(config),
            trip: None,
        }
    }

    /// The active limits.
    pub fn config(&self) -> SafetyConfig {
        self.config
    }

    /// Whether any fan is under manual control.
    pub fn manual_active(&self) -> bool {
        !self.state.is_empty()
    }

    /// `auto`, `max` or `custom`, summarising all fans.
    pub fn summary(&self) -> &'static str {
        if self.state.is_empty() {
            "auto"
        } else if self.state.values().all(|s| s.mode == FanMode::Max) {
            "max"
        } else {
            "custom"
        }
    }

    fn channels(&self) -> Vec<FanChannelInfo> {
        self.io.as_ref().map(|io| io.channels()).unwrap_or_default()
    }

    fn lockout_remaining(&self, now: Instant) -> Option<(Duration, &TripReason)> {
        let trip = self.trip.as_ref()?;
        let total = Duration::from_secs(self.config.lockout_secs);
        let left = total.checked_sub(now.saturating_duration_since(trip.at))?;
        (!left.is_zero()).then_some((left, &trip.reason))
    }

    /// Current state, read live from the hardware. Blocking.
    pub fn info(&self, now: Instant) -> FansInfo {
        let fans = match &self.io {
            None => Vec::new(),
            Some(io) => io
                .channels()
                .into_iter()
                .map(|ch| FanInfo {
                    rpm: io.read_rpm(&ch.key).ok().map(rq_core::Rpm),
                    mode: io.read_mode(&ch.key).ok().map(|m| match m {
                        HwFanMode::Auto => FanMode::Auto,
                        HwFanMode::Max => FanMode::Max,
                        HwFanMode::Manual => FanMode::Custom,
                    }),
                    duty_percent: io.read_duty(&ch.key).ok().map(Percent::get),
                    requested_percent: self
                        .state
                        .get(&ch.key)
                        .and_then(|s| s.requested)
                        .map(Percent::get),
                    role_verified: ch.role_source == rq_core::RoleSource::Label,
                    role: ch.role,
                    id: ch.name,
                })
                .collect(),
        };
        let lock = self.lockout_remaining(now);
        FansInfo {
            available: self.io.is_some(),
            controllable: self.io.is_some(),
            fans,
            safety: SafetyStatus {
                tripped: lock.is_some(),
                reason: lock.map(|(_, r)| r.to_string()),
                lockout_remaining_s: lock.map_or(0, |(d, _)| d.as_secs() + 1),
                min_percent: self.config.min_percent.get(),
                critical_cpu_celsius: self.config.critical_cpu.celsius() as u32,
                critical_gpu_celsius: self.config.critical_gpu.celsius() as u32,
            },
        }
    }

    /// Refuses manual control while locked out after a safety trip.
    pub fn check_manual_allowed(&self, now: Instant) -> Result<(), FanError> {
        match self.lockout_remaining(now) {
            Some((left, reason)) => Err(FanError::LockedOut(format!(
                "manual fan control is locked out for {} more seconds because {reason}",
                left.as_secs() + 1
            ))),
            None if self.io.is_none() => Err(FanError::Unavailable(
                "this machine has no controllable fans".into(),
            )),
            None => Ok(()),
        }
    }

    /// Validates a manual speed request without touching hardware. Used
    /// before authorization so bad input never causes a password prompt.
    pub fn validate_custom(
        &self,
        name: &str,
        percent: u32,
    ) -> Result<(FanChannelInfo, Percent), FanError> {
        self.io()?;
        let channel = self
            .channels()
            .into_iter()
            .find(|c| c.name == name)
            .ok_or_else(|| {
                let known: Vec<String> = self.channels().into_iter().map(|c| c.name).collect();
                FanError::InvalidArgument(format!(
                    "unknown fan '{name}' (available: {})",
                    known.join(", ")
                ))
            })?;
        let pct = Percent::try_from(i64::from(percent))
            .map_err(|_| FanError::InvalidArgument(format!("{percent}% is outside 0-100")))?;
        if pct < self.config.min_percent {
            return Err(FanError::InvalidArgument(format!(
                "{percent}% is below the safe minimum of {}%; use `auto` to hand control back to the firmware",
                self.config.min_percent.get()
            )));
        }
        Ok((channel, pct))
    }

    fn io(&self) -> Result<Arc<dyn FanIo>, FanError> {
        self.io
            .clone()
            .ok_or_else(|| FanError::Unavailable("this machine has no controllable fans".into()))
    }

    fn record_manual(&self) -> Result<(), FanError> {
        self.flag.set().map_err(|e| {
            FanError::Unavailable(format!(
                "cannot record the manual-control marker ({e}); refusing manual control because automatic control could not be restored after a crash"
            ))
        })
    }

    /// Clears the marker once every fan is confirmed automatic.
    fn clear_marker_if_safe(&self) {
        if self.state.is_empty()
            && let Err(e) = self.flag.clear()
        {
            warn!(error = %e, "could not clear the manual-control marker (a harmless extra restore will run at next start)");
        }
    }

    /// Returns every fan to firmware control and verifies it. Tries every
    /// fan even if one fails. Fans that could not be confirmed stay tracked
    /// (and supervised) so the restore is retried.
    pub fn set_auto(&mut self) -> Result<(), FanError> {
        let Some(io) = self.io.clone() else {
            // Nothing to write to. The marker stays: a later start with the
            // interface present will restore.
            self.state.clear();
            return Ok(());
        };
        let mut failures = Vec::new();
        for ch in io.channels() {
            let wrote = io.write_mode(&ch.key, HwFanMode::Auto);
            match (wrote, io.read_mode(&ch.key)) {
                (Ok(()), Ok(HwFanMode::Auto)) => {
                    self.state.remove(&ch.key);
                }
                (w, r) => failures.push(format!("{}: write {w:?}, read-back {r:?}", ch.name)),
            }
        }
        if failures.is_empty() {
            self.monitor.reset();
            self.clear_marker_if_safe();
            Ok(())
        } else {
            error!(?failures, "could not confirm automatic fan control");
            Err(FanError::NotConfirmed(format!(
                "automatic fan control could not be confirmed ({})",
                failures.join("; ")
            )))
        }
    }

    /// Full speed on every fan.
    pub fn set_max(&mut self, now: Instant) -> Result<(), FanError> {
        self.check_manual_allowed(now)?;
        let io = self.io()?;
        self.record_manual()?;
        for ch in io.channels() {
            let wrote = io.write_mode(&ch.key, HwFanMode::Max);
            if let Err(e) = wrote {
                return Err(self.abort(now, format!("{}: {e}", ch.name)));
            }
            if io.read_mode(&ch.key).ok() != Some(HwFanMode::Max) {
                return Err(self.abort(now, format!("{}: full speed was not confirmed", ch.name)));
            }
            self.state.insert(
                ch.key,
                FanState {
                    mode: FanMode::Max,
                    requested: Some(Percent::MAX),
                },
            );
        }
        self.monitor.reset();
        info!("fans set to maximum speed");
        Ok(())
    }

    /// Manual speed for one fan, others unchanged.
    pub fn set_custom(&mut self, name: &str, percent: u32, now: Instant) -> Result<(), FanError> {
        self.check_manual_allowed(now)?;
        let (channel, pct) = self.validate_custom(name, percent)?;
        let io = self.io()?;
        self.record_manual()?;

        // Speed first, then the mode switch: the fan never runs in manual
        // mode on a stale or zero duty.
        if let Err(e) = io.write_duty(&channel.key, pct) {
            return Err(self.abort(
                now,
                format!("{}: setting the speed failed: {e}", channel.name),
            ));
        }
        if let Err(e) = io.write_mode(&channel.key, HwFanMode::Manual) {
            return Err(self.abort(
                now,
                format!("{}: switching to manual failed: {e}", channel.name),
            ));
        }
        let mode_ok = io.read_mode(&channel.key).ok() == Some(HwFanMode::Manual);
        let duty = io.read_duty(&channel.key).ok();
        let duty_ok = duty.is_some_and(|d| d.get().abs_diff(pct.get()) <= 1);
        if !(mode_ok && duty_ok) {
            return Err(self.abort(
                now,
                format!(
                    "{}: asked for manual at {}%, hardware reports mode ok = {mode_ok}, duty {:?}",
                    channel.name,
                    pct.get(),
                    duty.map(Percent::get)
                ),
            ));
        }
        self.state.insert(
            channel.key,
            FanState {
                mode: FanMode::Custom,
                requested: Some(pct),
            },
        );
        self.monitor.reset();
        info!(
            fan = channel.name,
            percent = pct.get(),
            "manual fan speed set"
        );
        Ok(())
    }

    /// Something went wrong mid-change: go back to automatic and lock
    /// manual control out. Returns the error to report.
    fn abort(&mut self, now: Instant, detail: String) -> FanError {
        error!(%detail, "fan control change failed; returning to automatic control");
        self.trip(
            TripReason::ControlFailed {
                detail: detail.clone(),
            },
            now,
        );
        FanError::NotConfirmed(format!("{detail}; automatic control was restored"))
    }

    fn trip(&mut self, reason: TripReason, now: Instant) {
        let restored = self.set_auto();
        self.trip = Some(Trip { reason, at: now });
        if restored.is_err() {
            error!("automatic control could not be confirmed after a safety trip; will retry");
        }
    }

    /// Looks at a fresh sample. If manual control must be abandoned, does so
    /// and returns why. Cheap when nothing is under manual control.
    pub fn supervise(&mut self, sample: &TelemetrySample, now: Instant) -> Option<TripReason> {
        if self.state.is_empty() {
            return None;
        }
        if self.trip.is_some() {
            // A previous restore didn't fully succeed: keep trying, quietly.
            let _ = self.set_auto();
            return None;
        }
        let supervised: Vec<SupervisedFan> = self
            .state
            .keys()
            .map(|id| SupervisedFan { id: id.clone() })
            .collect();
        let reason = self.monitor.observe(sample, &supervised)?;
        warn!(%reason, "safety trip: returning fans to automatic control");
        self.trip(reason.clone(), now);
        Some(reason)
    }

    /// Adopts a new fan interface after a rescan. If the interface changed
    /// while fans were under manual control, control is handed back through
    /// the new interface and the change is reported.
    pub fn replace_io(&mut self, io: Option<Arc<dyn FanIo>>, now: Instant) -> Option<TripReason> {
        let same = match (&self.io, &io) {
            (Some(a), Some(b)) => a.describe() == b.describe(),
            (None, None) => true,
            _ => false,
        };
        if same {
            return None;
        }
        let was_manual = self.manual_active();
        self.io = io;
        self.state.clear();
        self.monitor.reset();
        if self.io.is_some() && self.flag.is_set() {
            // The old interface is gone but the firmware may still hold a
            // manual setting: make sure it doesn't.
            let _ = self.set_auto();
        }
        if was_manual {
            warn!("the fan interface changed while fans were under manual control");
            self.trip = Some(Trip {
                reason: TripReason::InterfaceLost,
                at: now,
            });
            return Some(TripReason::InterfaceLost);
        }
        None
    }

    /// Start-up recovery: if the previous run ended with the marker set,
    /// hand the fans back to the firmware.
    pub fn recover_on_start(&mut self) {
        if !self.flag.is_set() {
            return;
        }
        warn!(
            "the previous run ended with fans possibly under manual control; restoring automatic control"
        );
        let _ = self.set_auto();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::MemoryFlag;
    use rq_core::{CpuStatus, FanReading, FanRole, MilliCelsius, RoleSource, Rpm};
    use std::sync::Mutex;

    /// One simulated fan.
    #[derive(Debug, Clone)]
    struct SimFan {
        mode: HwFanMode,
        duty: Percent,
        rpm: u32,
    }

    /// Simulated fan hardware with injectable faults.
    #[derive(Debug)]
    struct Sim {
        fans: Mutex<BTreeMap<String, SimFan>>,
        log: Mutex<Vec<String>>,
        ignore_mode_writes: Mutex<bool>,
        fail_duty_writes: Mutex<bool>,
        stuck_in_manual: Mutex<Option<String>>,
        name: &'static str,
    }

    impl Sim {
        fn new() -> Arc<Self> {
            let fan = |rpm| SimFan {
                mode: HwFanMode::Auto,
                duty: Percent::new(40).expect("pct"),
                rpm,
            };
            Arc::new(Self {
                fans: Mutex::new(BTreeMap::from([
                    ("acer/1".into(), fan(2500)),
                    ("acer/2".into(), fan(2300)),
                ])),
                log: Mutex::new(Vec::new()),
                ignore_mode_writes: Mutex::new(false),
                fail_duty_writes: Mutex::new(false),
                stuck_in_manual: Mutex::new(None),
                name: "sim",
            })
        }
        fn mode(&self, key: &str) -> HwFanMode {
            self.fans.lock().expect("lock")[key].mode
        }
        fn duty(&self, key: &str) -> u8 {
            self.fans.lock().expect("lock")[key].duty.get()
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().expect("lock").clone()
        }
        fn set_rpm(&self, key: &str, rpm: u32) {
            self.fans
                .lock()
                .expect("lock")
                .get_mut(key)
                .expect("fan")
                .rpm = rpm;
        }
    }

    impl FanIo for Sim {
        fn channels(&self) -> Vec<FanChannelInfo> {
            vec![
                FanChannelInfo {
                    key: "acer/1".into(),
                    name: "cpu".into(),
                    role: FanRole::Cpu,
                    role_source: RoleSource::DriverChannelOrder,
                },
                FanChannelInfo {
                    key: "acer/2".into(),
                    name: "gpu".into(),
                    role: FanRole::Gpu,
                    role_source: RoleSource::DriverChannelOrder,
                },
            ]
        }
        fn read_rpm(&self, key: &str) -> std::io::Result<u32> {
            Ok(self.fans.lock().expect("lock")[key].rpm)
        }
        fn read_mode(&self, key: &str) -> std::io::Result<HwFanMode> {
            Ok(self.mode(key))
        }
        fn write_mode(&self, key: &str, mode: HwFanMode) -> std::io::Result<()> {
            self.log
                .lock()
                .expect("lock")
                .push(format!("mode {key} {mode:?}"));
            if *self.ignore_mode_writes.lock().expect("lock") && mode != HwFanMode::Auto {
                return Ok(()); // accepted but not applied
            }
            if mode == HwFanMode::Auto
                && self.stuck_in_manual.lock().expect("lock").as_deref() == Some(key)
            {
                return Err(std::io::Error::from_raw_os_error(5));
            }
            self.fans
                .lock()
                .expect("lock")
                .get_mut(key)
                .expect("fan")
                .mode = mode;
            Ok(())
        }
        fn read_duty(&self, key: &str) -> std::io::Result<Percent> {
            Ok(self.fans.lock().expect("lock")[key].duty)
        }
        fn write_duty(&self, key: &str, duty: Percent) -> std::io::Result<()> {
            self.log
                .lock()
                .expect("lock")
                .push(format!("duty {key} {}", duty.get()));
            if *self.fail_duty_writes.lock().expect("lock") {
                return Err(std::io::Error::from_raw_os_error(5));
            }
            self.fans
                .lock()
                .expect("lock")
                .get_mut(key)
                .expect("fan")
                .duty = duty;
            Ok(())
        }
        fn describe(&self) -> String {
            self.name.to_owned()
        }
    }

    struct Rig {
        sim: Arc<Sim>,
        flag: Arc<MemoryFlag>,
        ctl: FanController,
        t0: Instant,
    }

    fn rig() -> Rig {
        let sim = Sim::new();
        let flag = Arc::new(MemoryFlag::new());
        let ctl = FanController::new(Some(sim.clone()), SafetyConfig::default(), flag.clone());
        Rig {
            sim,
            flag,
            ctl,
            t0: Instant::now(),
        }
    }

    fn sample(cpu_c: Option<i32>, fans: &[(&str, u32)]) -> TelemetrySample {
        TelemetrySample {
            cpu: CpuStatus {
                temperature: cpu_c.map(|c| MilliCelsius(c * 1000)),
                ..Default::default()
            },
            fans: fans
                .iter()
                .map(|(id, rpm)| FanReading {
                    id: (*id).into(),
                    role: FanRole::Cpu,
                    rpm: Some(Rpm(*rpm)),
                })
                .collect(),
            ..Default::default()
        }
    }

    const HEALTHY: [(&str, u32); 2] = [("acer/1", 3000), ("acer/2", 3000)];

    #[test]
    fn custom_speed_is_written_before_the_mode_and_verified() {
        let mut r = rig();
        r.ctl.set_custom("cpu", 60, r.t0).expect("set");
        assert_eq!(
            r.sim.log(),
            ["duty acer/1 60", "mode acer/1 Manual"],
            "speed first, then manual"
        );
        assert_eq!(r.sim.mode("acer/1"), HwFanMode::Manual);
        assert_eq!(r.sim.duty("acer/1"), 60);
        assert_eq!(
            r.sim.mode("acer/2"),
            HwFanMode::Auto,
            "the other fan is untouched"
        );
        assert!(r.flag.is_set(), "marker recorded");
        assert_eq!(r.ctl.summary(), "custom");
        let info = r.ctl.info(r.t0);
        let cpu = info.fans.iter().find(|f| f.id == "cpu").expect("cpu");
        assert_eq!(
            (cpu.mode, cpu.duty_percent, cpu.requested_percent),
            (Some(FanMode::Custom), Some(60), Some(60))
        );
        assert!(
            !cpu.role_verified,
            "role comes from channel order, not a label"
        );
    }

    #[test]
    fn fans_are_independent() {
        let mut r = rig();
        r.ctl.set_custom("cpu", 50, r.t0).expect("cpu");
        r.ctl.set_custom("gpu", 80, r.t0).expect("gpu");
        assert_eq!((r.sim.duty("acer/1"), r.sim.duty("acer/2")), (50, 80));
        r.ctl.set_auto().expect("auto");
        assert_eq!(
            (r.sim.mode("acer/1"), r.sim.mode("acer/2")),
            (HwFanMode::Auto, HwFanMode::Auto)
        );
        assert!(
            !r.flag.is_set(),
            "marker cleared once everything is automatic"
        );
        assert_eq!(r.ctl.summary(), "auto");
    }

    #[test]
    fn nothing_below_the_minimum_is_ever_written() {
        let mut r = rig();
        let min = r.ctl.config().min_percent.get();
        for pct in 0..min {
            let err = r
                .ctl
                .set_custom("cpu", u32::from(pct), r.t0)
                .expect_err("below minimum");
            assert!(
                matches!(err, FanError::InvalidArgument(_)),
                "{pct}: {err:?}"
            );
        }
        assert!(r.sim.log().is_empty(), "the hardware was never touched");
        assert!(!r.flag.is_set());
        for pct in min..=100 {
            assert!(
                r.ctl.validate_custom("cpu", u32::from(pct)).is_ok(),
                "{pct}"
            );
        }
    }

    #[test]
    fn bad_arguments_are_refused_before_anything_happens() {
        let mut r = rig();
        for (fan, pct) in [
            ("nope", 50),
            ("", 50),
            ("acer/1", 50),
            ("cpu", 101),
            ("cpu", u32::MAX),
        ] {
            let err = r.ctl.set_custom(fan, pct, r.t0).expect_err("invalid");
            assert!(
                matches!(err, FanError::InvalidArgument(_)),
                "{fan} {pct}: {err:?}"
            );
        }
        assert!(r.sim.log().is_empty());
    }

    #[test]
    fn max_then_auto() {
        let mut r = rig();
        r.ctl.set_max(r.t0).expect("max");
        assert_eq!(
            (r.sim.mode("acer/1"), r.sim.mode("acer/2")),
            (HwFanMode::Max, HwFanMode::Max)
        );
        assert_eq!(r.ctl.summary(), "max");
        assert!(r.flag.is_set());
        r.ctl.set_auto().expect("auto");
        assert!(!r.flag.is_set());
    }

    #[test]
    fn a_change_the_hardware_ignores_is_rolled_back_and_locks_out_manual_control() {
        let mut r = rig();
        *r.sim.ignore_mode_writes.lock().expect("lock") = true;
        let err = r.ctl.set_custom("cpu", 60, r.t0).expect_err("unconfirmed");
        assert!(matches!(err, FanError::NotConfirmed(_)), "{err:?}");
        assert_eq!(r.sim.mode("acer/1"), HwFanMode::Auto);
        assert!(!r.flag.is_set(), "back to automatic, marker cleared");
        assert!(!r.ctl.manual_active());

        // Locked out for the configured time, but `auto` always works.
        *r.sim.ignore_mode_writes.lock().expect("lock") = false;
        let err = r
            .ctl
            .set_custom("cpu", 60, r.t0 + Duration::from_secs(5))
            .expect_err("locked");
        assert!(matches!(err, FanError::LockedOut(_)), "{err:?}");
        assert!(r.ctl.info(r.t0 + Duration::from_secs(5)).safety.tripped);
        assert!(r.ctl.set_auto().is_ok());
        r.ctl
            .set_custom("cpu", 60, r.t0 + Duration::from_secs(61))
            .expect("allowed again");
    }

    #[test]
    fn a_refused_speed_write_returns_to_automatic() {
        let mut r = rig();
        *r.sim.fail_duty_writes.lock().expect("lock") = true;
        let err = r.ctl.set_custom("gpu", 70, r.t0).expect_err("refused");
        assert!(matches!(err, FanError::NotConfirmed(_)), "{err:?}");
        assert_eq!(r.sim.mode("acer/2"), HwFanMode::Auto);
        assert!(
            !r.sim.log().iter().any(|l| l.contains("Manual")),
            "never switched to manual"
        );
        assert!(!r.flag.is_set());
    }

    #[test]
    fn if_the_marker_cannot_be_written_manual_control_is_refused() {
        let mut r = rig();
        r.flag.fail_on_set(true);
        let err = r.ctl.set_custom("cpu", 60, r.t0).expect_err("refused");
        assert!(matches!(err, FanError::Unavailable(_)), "{err:?}");
        assert!(r.sim.log().is_empty(), "no fan was touched");
        let err = r.ctl.set_max(r.t0).expect_err("refused");
        assert!(matches!(err, FanError::Unavailable(_)));
        assert!(r.sim.log().is_empty());
    }

    #[test]
    fn overheating_hands_control_back_by_itself() {
        let mut r = rig();
        r.ctl.set_custom("cpu", 40, r.t0).expect("manual");
        assert_eq!(r.ctl.supervise(&sample(Some(80), &HEALTHY), r.t0), None);
        let reason = r
            .ctl
            .supervise(&sample(Some(91), &HEALTHY), r.t0)
            .expect("trip");
        assert!(
            matches!(reason, TripReason::CpuCritical { .. }),
            "{reason:?}"
        );
        assert_eq!(r.sim.mode("acer/1"), HwFanMode::Auto);
        assert!(!r.flag.is_set());
        assert!(matches!(
            r.ctl.set_custom("cpu", 40, r.t0),
            Err(FanError::LockedOut(_))
        ));
        // Nothing to supervise any more.
        assert_eq!(r.ctl.supervise(&sample(Some(99), &HEALTHY), r.t0), None);
    }

    #[test]
    fn a_stalled_fan_under_manual_control_trips() {
        let mut r = rig();
        r.ctl.set_custom("gpu", 50, r.t0).expect("manual");
        r.sim.set_rpm("acer/2", 0);
        let stalled = [("acer/1", 3000), ("acer/2", 0)];
        for _ in 0..4 {
            assert_eq!(r.ctl.supervise(&sample(Some(60), &stalled), r.t0), None);
        }
        let reason = r
            .ctl
            .supervise(&sample(Some(60), &stalled), r.t0)
            .expect("trip");
        assert_eq!(
            reason,
            TripReason::FanStalled {
                fan: "acer/2".into()
            }
        );
        assert_eq!(r.sim.mode("acer/2"), HwFanMode::Auto);
    }

    #[test]
    fn a_fan_that_will_not_return_to_automatic_keeps_the_marker_and_is_retried() {
        let mut r = rig();
        r.ctl.set_custom("cpu", 50, r.t0).expect("cpu");
        r.ctl.set_custom("gpu", 50, r.t0).expect("gpu");
        *r.sim.stuck_in_manual.lock().expect("lock") = Some("acer/2".into());
        let err = r.ctl.set_auto().expect_err("one fan is stuck");
        assert!(matches!(err, FanError::NotConfirmed(_)), "{err:?}");
        assert_eq!(
            r.sim.mode("acer/1"),
            HwFanMode::Auto,
            "the others were still restored"
        );
        assert!(
            r.flag.is_set(),
            "the marker stays while any fan is unconfirmed"
        );
        assert!(r.ctl.manual_active(), "and it is still supervised");

        *r.sim.stuck_in_manual.lock().expect("lock") = None;
        r.ctl.set_auto().expect("second attempt works");
        assert!(!r.flag.is_set());
        assert!(!r.ctl.manual_active());
    }

    #[test]
    fn a_trip_whose_restore_fails_retries_every_tick_without_new_events() {
        let mut r = rig();
        r.ctl.set_custom("cpu", 50, r.t0).expect("manual");
        *r.sim.stuck_in_manual.lock().expect("lock") = Some("acer/1".into());
        assert!(
            r.ctl.supervise(&sample(Some(95), &HEALTHY), r.t0).is_some(),
            "first trip is reported"
        );
        assert!(r.flag.is_set());
        assert_eq!(
            r.ctl.supervise(&sample(Some(95), &HEALTHY), r.t0),
            None,
            "not reported again"
        );
        *r.sim.stuck_in_manual.lock().expect("lock") = None;
        assert_eq!(r.ctl.supervise(&sample(Some(95), &HEALTHY), r.t0), None);
        assert_eq!(
            r.sim.mode("acer/1"),
            HwFanMode::Auto,
            "the retry finally worked"
        );
        assert!(!r.flag.is_set());
    }

    #[test]
    fn crash_recovery_restores_automatic_control_at_start() {
        let sim = Sim::new();
        sim.fans
            .lock()
            .expect("lock")
            .get_mut("acer/1")
            .expect("fan")
            .mode = HwFanMode::Manual;
        let flag = Arc::new(MemoryFlag::new());
        flag.set().expect("marker left by the crashed run");
        let mut ctl = FanController::new(Some(sim.clone()), SafetyConfig::default(), flag.clone());
        ctl.recover_on_start();
        assert_eq!(sim.mode("acer/1"), HwFanMode::Auto);
        assert!(!flag.is_set());
    }

    #[test]
    fn no_marker_means_no_writes_at_start() {
        let mut r = rig();
        r.ctl.recover_on_start();
        assert!(r.sim.log().is_empty());
    }

    #[test]
    fn recovery_without_an_interface_keeps_the_marker_for_later() {
        let flag = Arc::new(MemoryFlag::new());
        flag.set().expect("marker");
        let mut ctl = FanController::new(None, SafetyConfig::default(), flag.clone());
        ctl.recover_on_start();
        assert!(
            flag.is_set(),
            "can't restore now; try again when the driver is back"
        );
    }

    #[test]
    fn losing_the_interface_during_manual_control_is_reported_and_restored_via_the_new_one() {
        let mut r = rig();
        r.ctl.set_custom("cpu", 50, r.t0).expect("manual");

        // The driver was reloaded: a new interface, whose firmware still
        // holds the old manual setting.
        let reborn = Sim::new();
        reborn
            .fans
            .lock()
            .expect("lock")
            .get_mut("acer/1")
            .expect("fan")
            .mode = HwFanMode::Manual;
        let reborn = Arc::new(Sim {
            name: "reborn",
            ..Arc::try_unwrap(reborn).expect("sole owner")
        });

        let trip = r.ctl.replace_io(Some(reborn.clone()), r.t0);
        assert_eq!(trip, Some(TripReason::InterfaceLost));
        assert_eq!(
            reborn.mode("acer/1"),
            HwFanMode::Auto,
            "handed back through the new interface"
        );
        assert!(!r.ctl.manual_active());
        assert!(!r.flag.is_set());
    }

    #[test]
    fn an_unchanged_interface_keeps_its_state() {
        let mut r = rig();
        r.ctl.set_custom("cpu", 50, r.t0).expect("manual");
        assert_eq!(r.ctl.replace_io(Some(r.sim.clone()), r.t0), None);
        assert!(r.ctl.manual_active());
    }

    #[test]
    fn no_fans_means_unavailable() {
        let mut ctl =
            FanController::new(None, SafetyConfig::default(), Arc::new(MemoryFlag::new()));
        let t = Instant::now();
        assert!(!ctl.info(t).controllable);
        assert!(matches!(ctl.set_max(t), Err(FanError::Unavailable(_))));
        assert!(matches!(
            ctl.set_custom("cpu", 50, t),
            Err(FanError::Unavailable(_) | FanError::InvalidArgument(_))
        ));
        assert!(
            ctl.set_auto().is_ok(),
            "restoring automatic control never fails for lack of fans"
        );
    }
}
