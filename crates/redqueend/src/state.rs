//! Shared daemon state: discovery results, the sampler and the history ring.

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rq_core::{CapabilityStatus, HardwareIdentity, History, TelemetrySample};
use rq_hardware::{Sampler, SystemRoot, SystemSnapshot, capabilities};
use rq_ipc::{DaemonStatus, MAX_SUBSCRIBERS};

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
    inner: Mutex<Inner>,
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
        let snap = SystemSnapshot::discover(&root);
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
            inner: Mutex::new(inner),
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
