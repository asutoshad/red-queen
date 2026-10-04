//! The D-Bus service. Read-only: every method returns data, none changes
//! hardware, and every argument is validated.

// The `zbus::interface` macro generates undocumented helper methods
// (property change notifiers); the items written here are all documented.
#![allow(missing_docs)]

use std::sync::Arc;

use rq_core::ThermalProfileId;
use rq_ipc::{DaemonError, MAX_HISTORY_SECONDS, OBJECT_PATH};
use tracing::{info, warn};
use zbus::fdo;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;

use crate::authz::{Action, AuthError, Authorizer};
use crate::profile::SetError;
use crate::state::Shared;

/// Object implementing `io.github.asutoshad.RedQueen.Daemon1`.
pub struct DaemonService {
    shared: Arc<Shared>,
    authorizer: Arc<Authorizer>,
}

impl DaemonService {
    /// Wraps the shared state and the authorizer for privileged methods.
    pub fn new(shared: Arc<Shared>, authorizer: Arc<Authorizer>) -> Self {
        Self { shared, authorizer }
    }

    /// The common front half of every hardware-changing method: rate limit
    /// first (so a flood can't spam password prompts), then authorization.
    /// Callers validate their arguments *between* the two steps.
    fn rate_limit(&self, client: &str) -> Result<(), DaemonError> {
        if self.shared.allow_request(client) {
            Ok(())
        } else {
            Err(DaemonError::RateLimited(
                "too many requests; slow down".into(),
            ))
        }
    }

    async fn authorize(&self, header: &Header<'_>, action: Action) -> Result<(), DaemonError> {
        self.authorizer
            .check(header, action)
            .await
            .map_err(|e| match e {
                AuthError::Denied(why) => DaemonError::NotAuthorized(why),
                AuthError::Unavailable(why) => {
                    DaemonError::NotAuthorized(format!("authorization unavailable: {why}"))
                }
            })
    }
}

/// The caller's unix user id, for the audit log. Best effort.
async fn caller_uid(conn: &zbus::Connection, header: &Header<'_>) -> Option<u32> {
    let sender = header.sender()?.clone();
    zbus::fdo::DBusProxy::new(conn)
        .await
        .ok()?
        .get_connection_unix_user(zbus::names::BusName::Unique(sender))
        .await
        .ok()
}

fn set_error(e: SetError) -> DaemonError {
    match e {
        SetError::Unavailable => {
            DaemonError::Unavailable("this machine has no controllable thermal profile".into())
        }
        SetError::InvalidArgument(m) => DaemonError::InvalidArgument(m),
        SetError::Unsupported(m) => DaemonError::Unsupported(m),
        SetError::Rejected(m) => DaemonError::Rejected(m),
        SetError::NotConfirmed(m) => DaemonError::NotConfirmed(m),
        SetError::Failed(m) => DaemonError::Failed(m),
    }
}

fn json<T: serde::Serialize>(value: &T) -> fdo::Result<String> {
    serde_json::to_string(value).map_err(|e| fdo::Error::Failed(format!("encoding failed: {e}")))
}

fn sender(header: &Header<'_>) -> fdo::Result<String> {
    header
        .sender()
        .map(ToString::to_string)
        .ok_or_else(|| fdo::Error::Failed("message has no sender".into()))
}

#[zbus::interface(name = "io.github.asutoshad.RedQueen.Daemon1")]
impl DaemonService {
    /// Capability statuses as a JSON array.
    async fn get_capabilities(&self) -> fdo::Result<String> {
        json(&self.shared.capabilities())
    }

    /// Daemon status as JSON.
    async fn get_status(&self) -> fdo::Result<String> {
        json(&self.shared.status())
    }

    /// The newest telemetry sample as JSON (`null` before the first).
    async fn get_telemetry(&self) -> fdo::Result<String> {
        json(&self.shared.latest())
    }

    /// Samples from the last `seconds` seconds (1 to 3600) as a JSON array.
    async fn get_history(&self, seconds: u32) -> fdo::Result<String> {
        if seconds == 0 || seconds > MAX_HISTORY_SECONDS {
            return Err(fdo::Error::InvalidArgs(format!(
                "seconds must be between 1 and {MAX_HISTORY_SECONDS}"
            )));
        }
        json(&self.shared.history(seconds))
    }

    /// Hardware identity as JSON (no serial numbers or UUIDs).
    async fn get_hardware_identity(&self) -> fdo::Result<String> {
        json(&self.shared.identity())
    }

    /// Thermal profile choices and state as JSON.
    async fn get_thermal_profiles(&self) -> fdo::Result<String> {
        let shared = self.shared.clone();
        let info = tokio::task::spawn_blocking(move || shared.thermal_profiles())
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        json(&info)
    }

    /// Switches the thermal profile. Rate limited, validated, authorized
    /// by polkit, written, read back and verified before returning.
    async fn set_thermal_profile(
        &self,
        profile: String,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<String, DaemonError> {
        let client = sender(&header).map_err(|e| DaemonError::Failed(e.to_string()))?;
        self.rate_limit(&client)?;
        let requested = ThermalProfileId::parse_untrusted(&profile)
            .map_err(|e| DaemonError::InvalidArgument(e.to_string()))?;
        self.authorize(&header, Action::SetProfile).await?;

        let shared = self.shared.clone();
        let wanted = requested.clone();
        let outcome = tokio::task::spawn_blocking(move || shared.set_thermal_profile(wanted))
            .await
            .map_err(|e| DaemonError::Failed(e.to_string()))?;
        let uid = caller_uid(conn, &header).await;

        match outcome {
            Ok(change) => {
                info!(
                    profile = change.result.active.kernel_name(),
                    previous = change.previous.as_ref().map(ThermalProfileId::kernel_name),
                    client,
                    uid,
                    "thermal profile changed"
                );
                if let Some(prev) = self
                    .shared
                    .announce_profile(Some(change.result.active.clone()))
                {
                    let prev = prev
                        .as_ref()
                        .map_or("", ThermalProfileId::kernel_name)
                        .to_owned();
                    let _ = Self::thermal_profile_changed(
                        &emitter,
                        prev,
                        change.result.active.kernel_name().to_owned(),
                    )
                    .await;
                }
                serde_json::to_string(&change.result)
                    .map_err(|e| DaemonError::Failed(e.to_string()))
            }
            Err(e) => {
                warn!(profile = requested.kernel_name(), client, uid, error = ?e, "thermal profile change failed");
                Err(set_error(e))
            }
        }
    }

    /// Starts live `TelemetryUpdated` signals while any client is subscribed.
    async fn subscribe(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let client = sender(&header)?;
        self.shared
            .subscribe(&client)
            .map_err(|_| fdo::Error::LimitsExceeded("too many telemetry subscribers".into()))
    }

    /// Withdraws the caller's subscription.
    async fn unsubscribe(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.shared.unsubscribe(&sender(&header)?);
        Ok(())
    }

    #[zbus(signal)]
    /// A new telemetry sample (JSON). Emitted only while a client is subscribed.
    async fn telemetry_updated(emitter: &SignalEmitter<'_>, sample: String) -> zbus::Result<()>;

    #[zbus(signal)]
    /// The thermal profile changed, by any client or by the firmware.
    async fn thermal_profile_changed(
        emitter: &SignalEmitter<'_>,
        previous: String,
        current: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    /// Hardware was rediscovered and the capabilities differ.
    async fn capabilities_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    /// Daemon version.
    async fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }
}

/// Emits `TelemetryUpdated` (only called while someone is subscribed).
pub async fn emit_telemetry(
    conn: &zbus::Connection,
    sample: &rq_core::TelemetrySample,
) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(conn, OBJECT_PATH)?;
    let payload = serde_json::to_string(sample).map_err(|e| zbus::Error::Failure(e.to_string()))?;
    DaemonService::telemetry_updated(&emitter, payload).await
}

/// Emits `CapabilitiesChanged`.
pub async fn emit_capabilities_changed(conn: &zbus::Connection) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(conn, OBJECT_PATH)?;
    DaemonService::capabilities_changed(&emitter).await
}

/// Emits `ThermalProfileChanged` for a change the firmware made.
pub async fn emit_profile_changed(
    conn: &zbus::Connection,
    previous: Option<&ThermalProfileId>,
    current: &ThermalProfileId,
) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(conn, OBJECT_PATH)?;
    DaemonService::thermal_profile_changed(
        &emitter,
        previous
            .map_or("", ThermalProfileId::kernel_name)
            .to_owned(),
        current.kernel_name().to_owned(),
    )
    .await
}
