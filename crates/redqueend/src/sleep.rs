//! Suspend safety: hand the fans back to the firmware before the system
//! sleeps, and rediscover the hardware after it wakes.
//!
//! A *delay* inhibitor lock makes logind wait for us after announcing
//! `PrepareForSleep(true)`; releasing the lock lets the suspend proceed.

use std::sync::Arc;

use futures_util::StreamExt;
use tracing::{info, warn};

use crate::service::{emit_capabilities_changed, emit_fan_mode_changed, emit_safety_event};
use crate::state::Shared;

mod proxy {
    //! Generated D-Bus proxy for the part of logind we use.
    #![allow(missing_docs)]

    #[zbus::proxy(
        interface = "org.freedesktop.login1.Manager",
        default_service = "org.freedesktop.login1",
        default_path = "/org/freedesktop/login1"
    )]
    pub trait Login1Manager {
        fn inhibit(
            &self,
            what: &str,
            who: &str,
            why: &str,
            mode: &str,
        ) -> zbus::Result<zbus::zvariant::OwnedFd>;

        #[zbus(signal)]
        fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
    }
}

use proxy::Login1ManagerProxy;

/// Holds the sleep delay lock and reacts to suspend and resume.
pub struct SleepGuard {
    manager: Login1ManagerProxy<'static>,
    signals: proxy::PrepareForSleepStream,
    lock: Option<zbus::zvariant::OwnedFd>,
}

async fn take_lock(manager: &Login1ManagerProxy<'_>) -> zbus::Result<zbus::zvariant::OwnedFd> {
    manager
        .inhibit(
            "sleep",
            "The Red Queen",
            "Return the fans to automatic control before sleeping",
            "delay",
        )
        .await
}

impl SleepGuard {
    /// Connects to logind and takes the delay lock. Fails (and the daemon
    /// carries on without suspend handling) on systems without logind.
    ///
    /// The signal subscription is made *before* the lock is taken, so once
    /// the lock is held no sleep announcement can be missed.
    pub async fn connect(conn: &zbus::Connection) -> zbus::Result<Self> {
        let manager = Login1ManagerProxy::new(conn).await?;
        let signals = manager.receive_prepare_for_sleep().await?;
        let lock = Some(take_lock(&manager).await?);
        Ok(Self {
            manager,
            signals,
            lock,
        })
    }

    /// Reacts to sleep and wake-up announcements until the signal stream
    /// ends (logind went away), which the caller treats as fatal.
    pub async fn run(mut self, shared: Arc<Shared>, conn: zbus::Connection) {
        info!("holding a sleep delay lock to restore the fans before suspend");
        while let Some(signal) = self.signals.next().await {
            let Ok(args) = signal.args() else { continue };
            if args.start {
                let worker = shared.clone();
                match tokio::task::spawn_blocking(move || worker.restore_fans()).await {
                    Ok(Ok(())) => info!("fans returned to automatic control before sleep"),
                    Ok(Err(e)) => {
                        warn!(error = %e, "could not confirm automatic fan control before sleep")
                    }
                    Err(e) => warn!(error = %e, "restoring fans before sleep failed"),
                }
                let _ = emit_fan_mode_changed(&conn, "auto").await;
                // Releasing the lock lets the suspend go ahead.
                self.lock = None;
            } else {
                info!("resumed from sleep; rediscovering hardware");
                let worker = shared.clone();
                if let Ok((changed, trip)) =
                    tokio::task::spawn_blocking(move || worker.rediscover_full()).await
                {
                    if let Some(reason) = trip {
                        let _ = emit_safety_event(&conn, &reason).await;
                    }
                    if changed {
                        let _ = emit_capabilities_changed(&conn).await;
                    }
                }
                match take_lock(&self.manager).await {
                    Ok(lock) => self.lock = Some(lock),
                    Err(e) => warn!(error = %e, "could not retake the sleep lock"),
                }
            }
        }
        warn!("logind notifications ended");
    }
}
