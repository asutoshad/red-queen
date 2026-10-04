//! The D-Bus interface between `redqueend` and its clients.
//!
//! Structured payloads are JSON strings of the types defined in `rq-core`
//! and here, so the interface can grow without breaking older clients.
//! Every value a client *sends* (in later API additions) is a plain typed
//! argument that the daemon validates.

use rq_core::{CapabilityStatus, HardwareIdentity, TelemetrySample};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Well-known bus name of the daemon.
pub const BUS_NAME: &str = "io.github.asutoshad.RedQueen.Daemon";
/// Object path of the daemon.
pub const OBJECT_PATH: &str = "/io/github/asutoshad/RedQueen/Daemon";
/// Interface name (the trailing number is the API version).
pub const INTERFACE: &str = "io.github.asutoshad.RedQueen.Daemon1";
/// Longest history window a client may request, in seconds.
pub const MAX_HISTORY_SECONDS: u32 = 3600;
/// Most clients that may subscribe to live telemetry at once.
pub const MAX_SUBSCRIBERS: usize = 32;

/// Daemon health and configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Daemon version.
    pub version: String,
    /// Seconds since the daemon started.
    pub uptime_s: u64,
    /// Telemetry interval in milliseconds.
    pub sample_interval_ms: u64,
    /// History buffer size in samples.
    pub history_capacity: usize,
    /// Samples currently stored.
    pub history_len: usize,
    /// Clients subscribed to live telemetry.
    pub subscribers: usize,
    /// How many times hardware has been (re)discovered.
    pub discoveries: u64,
    /// Unix time (ms) of the last discovery.
    pub last_discovery_ms: u64,
}

mod proxy {
    //! Generated D-Bus proxy (the macro's output can't carry docs).
    #![allow(missing_docs)]

    /// The D-Bus proxy used by clients.
    #[zbus::proxy(
        interface = "io.github.asutoshad.RedQueen.Daemon1",
        default_service = "io.github.asutoshad.RedQueen.Daemon",
        default_path = "/io/github/asutoshad/RedQueen/Daemon"
    )]
    pub trait Daemon {
        /// Capability statuses (JSON array of `CapabilityStatus`).
        fn get_capabilities(&self) -> zbus::Result<String>;
        /// Daemon status (JSON `DaemonStatus`).
        fn get_status(&self) -> zbus::Result<String>;
        /// Latest sample (JSON `TelemetrySample`, or `null` before the first).
        fn get_telemetry(&self) -> zbus::Result<String>;
        /// Samples from the last `seconds` seconds (JSON array).
        fn get_history(&self, seconds: u32) -> zbus::Result<String>;
        /// Hardware identity (JSON `HardwareIdentity`).
        fn get_hardware_identity(&self) -> zbus::Result<String>;
        /// Starts `TelemetryUpdated` signals for this client.
        fn subscribe(&self) -> zbus::Result<()>;
        /// Stops `TelemetryUpdated` signals for this client.
        fn unsubscribe(&self) -> zbus::Result<()>;

        /// A new sample (JSON `TelemetrySample`). Only sent while at least one
        /// client is subscribed.
        #[zbus(signal)]
        fn telemetry_updated(&self, sample: String) -> zbus::Result<()>;
        /// Hardware was rediscovered and capabilities changed.
        #[zbus(signal)]
        fn capabilities_changed(&self) -> zbus::Result<()>;

        /// Daemon version.
        #[zbus(property)]
        fn version(&self) -> zbus::Result<String>;
    }
}

pub use proxy::DaemonProxy;

/// Errors from [`Client`].
#[derive(Debug, Error)]
pub enum ClientError {
    /// D-Bus failure (daemon not running, access denied, ...).
    #[error("D-Bus: {0}")]
    Dbus(#[from] zbus::Error),
    /// The daemon sent data this client can't decode.
    #[error("invalid data from daemon: {0}")]
    Decode(#[from] serde_json::Error),
}

/// Typed wrapper over [`DaemonProxy`].
#[derive(Debug, Clone)]
pub struct Client<'a> {
    proxy: DaemonProxy<'a>,
}

impl<'a> Client<'a> {
    /// Connects to the daemon on `conn` (system bus normally; session or
    /// peer-to-peer in development and tests).
    pub async fn new(conn: &zbus::Connection) -> Result<Self, ClientError> {
        Ok(Self {
            proxy: DaemonProxy::new(conn).await?,
        })
    }

    /// Wraps an existing proxy.
    pub fn from_proxy(proxy: DaemonProxy<'a>) -> Self {
        Self { proxy }
    }

    /// The raw proxy, for signals.
    pub fn proxy(&self) -> &DaemonProxy<'a> {
        &self.proxy
    }

    /// Capability statuses.
    pub async fn capabilities(&self) -> Result<Vec<CapabilityStatus>, ClientError> {
        Ok(serde_json::from_str(&self.proxy.get_capabilities().await?)?)
    }

    /// Daemon status.
    pub async fn status(&self) -> Result<DaemonStatus, ClientError> {
        Ok(serde_json::from_str(&self.proxy.get_status().await?)?)
    }

    /// Latest telemetry sample.
    pub async fn telemetry(&self) -> Result<Option<TelemetrySample>, ClientError> {
        Ok(serde_json::from_str(&self.proxy.get_telemetry().await?)?)
    }

    /// Telemetry history.
    pub async fn history(&self, seconds: u32) -> Result<Vec<TelemetrySample>, ClientError> {
        Ok(serde_json::from_str(
            &self.proxy.get_history(seconds).await?,
        )?)
    }

    /// Hardware identity.
    pub async fn hardware_identity(&self) -> Result<HardwareIdentity, ClientError> {
        Ok(serde_json::from_str(
            &self.proxy.get_hardware_identity().await?,
        )?)
    }
}
