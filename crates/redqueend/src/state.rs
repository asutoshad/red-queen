//! Shared daemon state: discovery results, the sampler and the history ring.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rq_core::{CapabilityStatus, HardwareIdentity, History, TelemetrySample, ThermalProfileId};
use rq_hardware::profile::{ProfileIo, SysfsProfileIo};
use rq_hardware::{Sampler, SystemRoot, SystemSnapshot, capabilities, models};
use rq_ipc::{DaemonStatus, MAX_SUBSCRIBERS, ThermalProfilesInfo};

use crate::limits::RateLimiter;
use crate::profile::{Change, ProfileController, SetError};

/// Smallest and largest allowed sampling interval.
pub const MIN_INTERVAL: Duration = Duration::from_millis(250);
/// See [`MIN_INTERVAL`].
pub const MAX_INTERVAL: Duration = Duration::from_secs(60);
/// How much history is kept.
pub const HISTORY_WINDOW: Duration = Duration::from_secs(3600);

/// Daemon configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Telemetry sampling interval.
    pub interval: Duration,
}

impl Config {
    /// Builds a config, clamping the interval to the allowed range.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval: interval.clamp(MIN_INTERVAL, MAX_INTERVAL),
        }
    }

    /// Samples needed to cover [`HISTORY_WINDOW`].
    pub fn history_capacity(&self) -> usize {
        usize::try_from(HISTORY_WINDOW.as_millis() / self.interval.as_millis().max(1))
            .unwrap_or(usize::MAX)
            .max(1)
    }
}

/// Chooses the profile interface to control for a discovered system.
pub type ProfileIoFactory =
    Arc<dyn Fn(&SystemRoot, &SystemSnapshot) -> Option<Arc<dyn ProfileIo>> + Send + Sync>;

/// Profiles this exact model and BIOS are known (from hardware tests) to
/// reject. Empty on anything untested, so the firmware decides.
fn known_rejected_profiles(snap: &SystemSnapshot) -> Vec<ThermalProfileId> {
    models::lookup(&snap.identity)
        .filter(|m| m.verified_on(&snap.identity))
        .map(|m| {
            m.rejected_profiles
                .iter()
                .map(|n| ThermalProfileId::from_kernel_name(n))
                .collect()
        })
        .unwrap_or_default()
}

fn default_profile_factory() -> ProfileIoFactory {
    Arc::new(|root, snap| SysfsProfileIo::select(root, &snap.platform_profile))
}

/// Returned by [`Shared::subscribe`] when the subscriber limit is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManySubscribers;

struct Inner {
    identity: HardwareIdentity,
    capabilities: Vec<CapabilityStatus>,
    sampler: Sampler,
    history: History,
    subscribers: BTreeSet<String>,
    discoveries: u64,
    last_discovery_ms: u64,
}

/// State shared between the D-Bus service and the background tasks.
pub struct Shared {
    root: SystemRoot,
    config: Config,
    started: Instant,
    profile_factory: ProfileIoFactory,
    inner: Mutex<Inner>,
    profile: Mutex<ProfileController>,
    limiter: Mutex<RateLimiter>,
    announced_profile: Mutex<Option<ThermalProfileId>>,
}

/// Unix time in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Shared {
    /// Discovers hardware and builds the initial state.
    pub fn new(root: SystemRoot, config: Config) -> Self {
        Self::with_profile_factory(root, config, default_profile_factory())
    }

    /// Like [`Self::new`] with a custom profile interface (tests inject
    /// simulated firmware here).
    pub fn with_profile_factory(
        root: SystemRoot,
        config: Config,
        profile_factory: ProfileIoFactory,
    ) -> Self {
        let snap = SystemSnapshot::discover(&root);
        let io = profile_factory(&root, &snap);
        let initial_profile = io.as_ref().and_then(|i| i.read_active().ok());
        let inner = Inner {
            identity: snap.identity.clone(),
            capabilities: capabilities::evaluate(&snap),
            sampler: Sampler::new(root.clone(), &snap),
            history: History::new(config.history_capacity()),
            subscribers: BTreeSet::new(),
            discoveries: 1,
            last_discovery_ms: now_ms(),
        };
        Self {
            root,
            config,
            started: Instant::now(),
            profile_factory,
            inner: Mutex::new(inner),
            profile: Mutex::new({
                let mut controller = ProfileController::new(io);
                controller.set_known_rejected(known_rejected_profiles(&snap));
                controller
            }),
            limiter: Mutex::new(RateLimiter::new()),
            announced_profile: Mutex::new(initial_profile),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The configured sampling interval.
    pub fn interval(&self) -> Duration {
        self.config.interval
    }

    /// Re-reads the hardware. Returns whether capabilities changed.
    ///
    /// Done outside the lock so slow sysfs reads never block D-Bus calls.
    pub fn rediscover(&self) -> bool {
        let snap = SystemSnapshot::discover(&self.root);
        let caps = capabilities::evaluate(&snap);
        let sampler = Sampler::new(self.root.clone(), &snap);
        let io = (self.profile_factory)(&self.root, &snap);
        {
            let mut controller = self.profile.lock().unwrap_or_else(PoisonError::into_inner);
            controller.replace_io(io);
            controller.set_known_rejected(known_rejected_profiles(&snap));
        }
        let mut inner = self.lock();
        let changed = inner.capabilities != caps;
        inner.identity = snap.identity;
        inner.capabilities = caps;
        inner.sampler = sampler;
        inner.discoveries += 1;
        inner.last_discovery_ms = now_ms();
        changed
    }

    /// Takes a sample, stores it and returns a copy.
    pub fn sample(&self) -> TelemetrySample {
        let mut inner = self.lock();
        let sample = inner.sampler.sample(now_ms());
        inner.history.push(sample.clone());
        sample
    }

    /// The newest stored sample.
    pub fn latest(&self) -> Option<TelemetrySample> {
        self.lock().history.latest().cloned()
    }

    /// Samples from the last `seconds` seconds.
    pub fn history(&self, seconds: u32) -> Vec<TelemetrySample> {
        self.lock().history.since(u64::from(seconds) * 1000)
    }

    /// Current capability statuses.
    pub fn capabilities(&self) -> Vec<CapabilityStatus> {
        self.lock().capabilities.clone()
    }

    /// Current hardware identity.
    pub fn identity(&self) -> HardwareIdentity {
        self.lock().identity.clone()
    }

    /// Daemon status.
    pub fn status(&self) -> DaemonStatus {
        let inner = self.lock();
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            uptime_s: self.started.elapsed().as_secs(),
            sample_interval_ms: u64::try_from(self.config.interval.as_millis()).unwrap_or(u64::MAX),
            history_capacity: inner.history.capacity(),
            history_len: inner.history.len(),
            subscribers: inner.subscribers.len(),
            discoveries: inner.discoveries,
            last_discovery_ms: inner.last_discovery_ms,
        }
    }

    /// Registers a client (by its unique bus name) for live telemetry.
    pub fn subscribe(&self, client: &str) -> Result<(), TooManySubscribers> {
        let mut inner = self.lock();
        if !inner.subscribers.contains(client) && inner.subscribers.len() >= MAX_SUBSCRIBERS {
            return Err(TooManySubscribers);
        }
        inner.subscribers.insert(client.to_owned());
        Ok(())
    }

    /// Removes a client.
    pub fn unsubscribe(&self, client: &str) {
        self.lock().subscribers.remove(client);
    }

    /// Forgets everything about a client that disconnected.
    pub fn client_gone(&self, client: &str) {
        self.unsubscribe(client);
        self.limiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .forget(client);
    }

    /// Takes a rate-limit token for a client's hardware-changing request.
    pub fn allow_request(&self, client: &str) -> bool {
        self.limiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .allow(client, Instant::now())
    }

    /// Thermal profile choices and state, read live. Blocking.
    pub fn thermal_profiles(&self) -> ThermalProfilesInfo {
        self.profile
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .info()
    }

    /// Changes the thermal profile and verifies it. Blocking.
    pub fn set_thermal_profile(&self, requested: ThermalProfileId) -> Result<Change, SetError> {
        let outcome = self
            .profile
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set(requested);
        // Whatever happened, the kernel's state may have changed: make the
        // next sample re-read it instead of serving a cached value.
        self.lock().sampler.invalidate_slow();
        outcome
    }

    /// Records the profile clients have been told about. Returns the
    /// previous value if `current` differs, so exactly one caller announces
    /// each change.
    pub fn announce_profile(
        &self,
        current: Option<ThermalProfileId>,
    ) -> Option<Option<ThermalProfileId>> {
        let mut last = self
            .announced_profile
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *last == current {
            return None;
        }
        Some(std::mem::replace(&mut *last, current))
    }

    /// Whether anyone wants live telemetry.
    pub fn has_subscribers(&self) -> bool {
        !self.lock().subscribers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_is_clamped() {
        assert_eq!(Config::new(Duration::from_millis(1)).interval, MIN_INTERVAL);
        assert_eq!(
            Config::new(Duration::from_secs(9999)).interval,
            MAX_INTERVAL
        );
        assert_eq!(Config::new(Duration::from_secs(1)).history_capacity(), 3600);
        assert_eq!(Config::new(Duration::from_secs(60)).history_capacity(), 60);
    }
}
