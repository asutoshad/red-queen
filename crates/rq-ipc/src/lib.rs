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

/// Prefix of the daemon's D-Bus error names.
pub const ERROR_PREFIX: &str = "io.github.asutoshad.RedQueen.Error";

/// Why a request failed. Mirrors [`DaemonError`] without the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The caller isn't allowed to do this (or dismissed the password prompt).
    NotAuthorized,
    /// An argument was malformed, out of range or names an unknown thing.
    InvalidArgument,
    /// This hardware or driver can't do it.
    Unsupported,
    /// The firmware refused the change.
    Rejected,
    /// The change was accepted but the hardware didn't confirm it.
    NotConfirmed,
    /// Too many requests from this client.
    RateLimited,
    /// The needed interface isn't available right now.
    Unavailable,
    /// Anything else.
    Failed,
}

impl ErrorKind {
    const ALL: [(Self, &'static str); 8] = [
        (Self::NotAuthorized, "NotAuthorized"),
        (Self::InvalidArgument, "InvalidArgument"),
        (Self::Unsupported, "Unsupported"),
        (Self::Rejected, "Rejected"),
        (Self::NotConfirmed, "NotConfirmed"),
        (Self::RateLimited, "RateLimited"),
        (Self::Unavailable, "Unavailable"),
        (Self::Failed, "Failed"),
    ];

    /// Parses a full D-Bus error name such as
    /// `io.github.asutoshad.RedQueen.Error.Rejected`.
    pub fn from_dbus_name(name: &str) -> Option<Self> {
        let short = name.strip_prefix(ERROR_PREFIX)?.strip_prefix('.')?;
        Self::ALL.iter().find(|(_, n)| *n == short).map(|(k, _)| *k)
    }
}

/// Errors the daemon returns over D-Bus.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "io.github.asutoshad.RedQueen.Error")]
pub enum DaemonError {
    /// Transport-level failure.
    #[zbus(error)]
    ZBus(zbus::Error),
    /// See [`ErrorKind::NotAuthorized`].
    NotAuthorized(String),
    /// See [`ErrorKind::InvalidArgument`].
    InvalidArgument(String),
    /// See [`ErrorKind::Unsupported`].
    Unsupported(String),
    /// See [`ErrorKind::Rejected`].
    Rejected(String),
    /// See [`ErrorKind::NotConfirmed`].
    NotConfirmed(String),
    /// See [`ErrorKind::RateLimited`].
    RateLimited(String),
    /// See [`ErrorKind::Unavailable`].
    Unavailable(String),
    /// See [`ErrorKind::Failed`].
    Failed(String),
}

/// Whether a profile can be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChoiceState {
    /// Advertised by the driver and not known to fail.
    Available,
    /// The firmware rejected it earlier; it stays disabled until the
    /// daemon restarts or the hardware interface changes.
    Unsupported,
}

/// One selectable thermal profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileChoice {
    /// The profile.
    pub id: rq_core::ThermalProfileId,
    /// Whether it can be selected.
    pub state: ChoiceState,
}

/// Thermal profile state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThermalProfilesInfo {
    /// A controllable profile interface exists.
    pub available: bool,
    /// The profile the kernel reports as active.
    pub active: Option<rq_core::ThermalProfileId>,
    /// Profiles the driver advertises.
    pub choices: Vec<ProfileChoice>,
}

/// Outcome of a confirmed profile change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetProfileResult {
    /// What was asked for.
    pub requested: rq_core::ThermalProfileId,
    /// What the kernel reports afterwards. Equal to `requested`: a
    /// mismatch is returned as [`ErrorKind::NotConfirmed`] instead.
    pub active: rq_core::ThermalProfileId,
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
        /// Thermal profile choices and state (JSON `ThermalProfilesInfo`).
        fn get_thermal_profiles(&self) -> zbus::Result<String>;
        /// Switches the thermal profile (JSON `SetProfileResult`). Needs
        /// authorization; read back and verified by the daemon.
        fn set_thermal_profile(&self, profile: &str) -> zbus::Result<String>;
        /// Starts `TelemetryUpdated` signals for this client.
        fn subscribe(&self) -> zbus::Result<()>;
        /// Stops `TelemetryUpdated` signals for this client.
        fn unsubscribe(&self) -> zbus::Result<()>;

        /// A new sample (JSON `TelemetrySample`). Only sent while at least one
        /// client is subscribed.
        #[zbus(signal)]
        fn telemetry_updated(&self, sample: String) -> zbus::Result<()>;
        /// The thermal profile changed (by any client, or by the firmware).
        #[zbus(signal)]
        fn thermal_profile_changed(&self, previous: String, current: String) -> zbus::Result<()>;
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
    Dbus(zbus::Error),
    /// The daemon refused or failed the request.
    #[error("{message}")]
    Daemon {
        /// Category.
        kind: ErrorKind,
        /// Explanation from the daemon.
        message: String,
    },
    /// The daemon sent data this client can't decode.
    #[error("invalid data from daemon: {0}")]
    Decode(#[from] serde_json::Error),
}

impl From<zbus::Error> for ClientError {
    fn from(e: zbus::Error) -> Self {
        if let zbus::Error::MethodError(name, description, _) = &e
            && let Some(kind) = ErrorKind::from_dbus_name(name.as_str())
        {
            return Self::Daemon {
                kind,
                message: description.clone().unwrap_or_else(|| format!("{kind:?}")),
            };
        }
        Self::Dbus(e)
    }
}

impl ClientError {
    /// The daemon's error category, if this is a daemon error.
    pub fn kind(&self) -> Option<ErrorKind> {
        match self {
            Self::Daemon { kind, .. } => Some(*kind),
            _ => None,
        }
    }
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

    /// Thermal profile choices and state.
    pub async fn thermal_profiles(&self) -> Result<ThermalProfilesInfo, ClientError> {
        Ok(serde_json::from_str(
            &self.proxy.get_thermal_profiles().await?,
        )?)
    }

    /// Switches the thermal profile. Returns only once the daemon has read
    /// the new profile back from the kernel.
    pub async fn set_thermal_profile(
        &self,
        profile: &rq_core::ThermalProfileId,
    ) -> Result<SetProfileResult, ClientError> {
        let reply = self
            .proxy
            .set_thermal_profile(profile.kernel_name())
            .await?;
        Ok(serde_json::from_str(&reply)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_names_round_trip() {
        for (kind, short) in ErrorKind::ALL {
            let full = format!("{ERROR_PREFIX}.{short}");
            assert_eq!(ErrorKind::from_dbus_name(&full), Some(kind));
        }
        assert_eq!(
            ErrorKind::from_dbus_name("org.freedesktop.DBus.Error.Failed"),
            None
        );
        assert_eq!(
            ErrorKind::from_dbus_name(&format!("{ERROR_PREFIX}.Nope")),
            None
        );
        assert_eq!(
            ErrorKind::from_dbus_name(&format!("{ERROR_PREFIX}Rejected")),
            None
        );
    }
}
