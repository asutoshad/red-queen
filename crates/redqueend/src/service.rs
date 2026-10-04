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
use crate::fans::FanError;
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

impl DaemonService {
    /// Common tail of the fan methods: log the outcome, announce the new
    /// mode, and return the state read back from the hardware.
    async fn fans_reply(
        &self,
        outcome: Result<(), FanError>,
        what: &str,
        client: &str,
        uid: Option<u32>,
        emitter: &SignalEmitter<'_>,
    ) -> Result<String, DaemonError> {
        if let Err(e) = outcome {
            warn!(what, client, uid, error = ?e, "fan control request failed");
            return Err(fan_error(e));
        }
        let shared = self.shared.clone();
        let (summary, info) =
            tokio::task::spawn_blocking(move || (shared.fan_summary(), shared.fans_info()))
                .await
                .map_err(|e| DaemonError::Failed(e.to_string()))?;
        info!(what, summary, client, uid, "fan control changed");
        let _ = Self::fan_mode_changed(emitter, summary.to_owned()).await;
        serde_json::to_string(&info).map_err(|e| DaemonError::Failed(e.to_string()))
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

fn fan_error(e: FanError) -> DaemonError {
    match e {
        FanError::Unavailable(m) | FanError::LockedOut(m) => DaemonError::Unavailable(m),
        FanError::InvalidArgument(m) => DaemonError::InvalidArgument(m),
        FanError::NotConfirmed(m) => DaemonError::NotConfirmed(m),
        FanError::Failed(m) => DaemonError::Failed(m),
    }
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

    /// Fan state, limits and safety status as JSON.
    async fn get_fans(&self) -> fdo::Result<String> {
        let shared = self.shared.clone();
        let info = tokio::task::spawn_blocking(move || shared.fans_info())
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        json(&info)
    }

    /// Sets every fan to `auto` (always allowed) or `max` (needs
    /// authorization). Read back and verified before returning.
    async fn set_fan_mode(
        &self,
        mode: String,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<String, DaemonError> {
        let client = sender(&header).map_err(|e| DaemonError::Failed(e.to_string()))?;
        self.rate_limit(&client)?;
        let max = match mode.as_str() {
            "auto" => false,
            "max" => true,
            _ => {
                return Err(DaemonError::InvalidArgument(
                    "mode must be 'auto' or 'max' (use SetFanSpeed for a manual speed)".into(),
                ));
            }
        };
        if max {
            self.shared.check_manual_fans_allowed().map_err(fan_error)?;
            self.authorize(&header, Action::ControlFans).await?;
        }
        let shared = self.shared.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            if max {
                shared.set_fan_max()
            } else {
                shared.set_fan_auto()
            }
        })
        .await
        .map_err(|e| DaemonError::Failed(e.to_string()))?;
        let uid = caller_uid(conn, &header).await;
        self.fans_reply(outcome, &format!("mode {mode}"), &client, uid, &emitter)
            .await
    }

    /// Puts one fan under manual control at `percent` (never below the safe
    /// minimum). Needs authorization. Read back and verified.
    async fn set_fan_speed(
        &self,
        fan: String,
        percent: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<String, DaemonError> {
        let client = sender(&header).map_err(|e| DaemonError::Failed(e.to_string()))?;
        self.rate_limit(&client)?;
        // Arguments, availability and lockout are checked before anyone is
        // asked for a password.
        self.shared
            .validate_fan_custom(&fan, percent)
            .map_err(fan_error)?;
        self.authorize(&header, Action::ControlFans).await?;
        let shared = self.shared.clone();
        let target = fan.clone();
        let outcome = tokio::task::spawn_blocking(move || shared.set_fan_custom(&target, percent))
            .await
            .map_err(|e| DaemonError::Failed(e.to_string()))?;
        let uid = caller_uid(conn, &header).await;
        self.fans_reply(
            outcome,
            &format!("{fan} at {percent}%"),
            &client,
            uid,
            &emitter,
        )
        .await
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
    /// Fan control changed: `auto`, `max` or `custom`.
    async fn fan_mode_changed(emitter: &SignalEmitter<'_>, summary: String) -> zbus::Result<()>;

    #[zbus(signal)]
    /// The safety layer handed fan control back to the firmware.
    async fn safety_event(
        emitter: &SignalEmitter<'_>,
        code: String,
        message: String,
    ) -> zbus::Result<()>;

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

/// Emits `FanModeChanged`.
pub async fn emit_fan_mode_changed(conn: &zbus::Connection, summary: &str) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(conn, OBJECT_PATH)?;
    DaemonService::fan_mode_changed(&emitter, summary.to_owned()).await
}

/// Emits `SafetyEvent` for a trip that returned fans to the firmware.
pub async fn emit_safety_event(
    conn: &zbus::Connection,
    reason: &rq_core::TripReason,
) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(conn, OBJECT_PATH)?;
    let code = serde_json::to_value(reason)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_str().map(str::to_owned)))
        .unwrap_or_else(|| "unknown".to_owned());
    DaemonService::safety_event(&emitter, code, reason.to_string()).await
}
