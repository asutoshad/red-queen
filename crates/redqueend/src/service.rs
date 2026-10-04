//! The D-Bus service. Read-only: every method returns data, none changes
//! hardware, and every argument is validated.

// The `zbus::interface` macro generates undocumented helper methods
// (property change notifiers); the items written here are all documented.
#![allow(missing_docs)]

use std::sync::Arc;

use rq_ipc::{MAX_HISTORY_SECONDS, OBJECT_PATH};
use zbus::fdo;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;

use crate::state::Shared;

/// Object implementing `io.github.asutoshad.RedQueen.Daemon1`.
pub struct DaemonService {
    shared: Arc<Shared>,
}

impl DaemonService {
    /// Wraps the shared state.
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
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
