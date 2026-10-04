//! Background tasks: telemetry ticks, hot-plug watching, client cleanup.

use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use rustix::event::{PollFd, PollFlags, poll};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::service::{emit_capabilities_changed, emit_telemetry};
use crate::state::Shared;

/// Subsystems whose events can change what the hardware exposes.
const WATCHED_SUBSYSTEMS: [&str; 5] = ["hwmon", "power_supply", "platform-profile", "pci", "wmi"];
/// Wait this long after the last event before rediscovering, so a burst
/// of events (driver load, resume) causes one discovery.
const DEBOUNCE: Duration = Duration::from_millis(750);
/// Rediscover this often even without events, as a safety net.
const SAFETY_REDISCOVERY: Duration = Duration::from_secs(300);

/// Starts the telemetry and rediscovery tasks. Dropping the returned set
/// stops them.
pub fn spawn_core(shared: &Arc<Shared>, conn: &zbus::Connection) -> JoinSet<()> {
    let mut tasks = JoinSet::new();
    tasks.spawn(telemetry_loop(shared.clone(), conn.clone()));
    tasks.spawn(rediscovery_loop(
        shared.clone(),
        conn.clone(),
        start_udev_watcher(),
    ));
    tasks
}

/// Removes subscribers whose bus connection has gone away. Only for real
/// bus connections (not peer-to-peer ones).
pub fn spawn_client_cleanup(
    tasks: &mut JoinSet<()>,
    shared: &Arc<Shared>,
    conn: &zbus::Connection,
) {
    tasks.spawn(client_cleanup(shared.clone(), conn.clone()));
}

async fn telemetry_loop(shared: Arc<Shared>, conn: zbus::Connection) {
    let mut tick = tokio::time::interval(shared.interval());
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let worker = shared.clone();
        let sample = match tokio::task::spawn_blocking(move || worker.sample()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "sampling task failed");
                continue;
            }
        };
        if shared.has_subscribers()
            && let Err(e) = emit_telemetry(&conn, &sample).await
        {
            debug!(error = %e, "could not emit telemetry");
        }
    }
}

async fn rediscovery_loop(
    shared: Arc<Shared>,
    conn: zbus::Connection,
    mut events: mpsc::Receiver<()>,
) {
    let mut safety = tokio::time::interval(SAFETY_REDISCOVERY);
    safety.tick().await; // the first tick is immediate; discovery just ran
    loop {
        tokio::select! {
            ev = events.recv() => {
                if ev.is_none() {
                    // Watcher is gone; fall back to the safety timer only.
                    safety.tick().await;
                } else {
                    tokio::time::sleep(DEBOUNCE).await;
                    while events.try_recv().is_ok() {}
                }
            }
            _ = safety.tick() => {}
        }
        let worker = shared.clone();
        match tokio::task::spawn_blocking(move || worker.rediscover()).await {
            Ok(true) => {
                info!("hardware capabilities changed");
                if let Err(e) = emit_capabilities_changed(&conn).await {
                    debug!(error = %e, "could not emit CapabilitiesChanged");
                }
            }
            Ok(false) => debug!("rediscovered hardware, no change"),
            Err(e) => warn!(error = %e, "rediscovery task failed"),
        }
    }
}

async fn client_cleanup(shared: Arc<Shared>, conn: zbus::Connection) {
    let result: zbus::Result<()> = async {
        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
        let mut stream = dbus.receive_name_owner_changed().await?;
        while let Some(signal) = stream.next().await {
            let Ok(args) = signal.args() else { continue };
            if args.new_owner().is_none() && args.name().as_str().starts_with(':') {
                shared.unsubscribe(args.name().as_str());
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        warn!(error = %e, "client cleanup stopped");
    }
}

/// Watches udev on a dedicated thread (the monitor socket is blocking and
/// its devices aren't `Send`) and sends a unit value per burst of events.
fn start_udev_watcher() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel(1);
    let spawned = std::thread::Builder::new()
        .name("udev-watch".into())
        .spawn(move || {
            if let Err(e) = watch_udev(&tx) {
                warn!(error = %e, "udev monitoring unavailable; relying on periodic rediscovery");
            }
        });
    if let Err(e) = spawned {
        warn!(error = %e, "could not start the udev thread");
    }
    rx
}

fn watch_udev(tx: &mpsc::Sender<()>) -> std::io::Result<()> {
    let mut builder = udev::MonitorBuilder::new()?;
    for subsystem in WATCHED_SUBSYSTEMS {
        builder = builder.match_subsystem(subsystem)?;
    }
    let socket = builder.listen()?;
    info!("watching udev for hardware changes");
    loop {
        let mut fds = [PollFd::new(&socket, PollFlags::IN)];
        poll(&mut fds, None)?;
        // Drain everything queued; one wake-up covers the whole burst.
        let mut any = false;
        for _event in socket.iter() {
            any = true;
        }
        if any {
            // A full channel already has a pending wake-up.
            let _ = tx.try_send(());
        }
        if tx.is_closed() {
            return Ok(());
        }
        let _ = socket.as_fd();
    }
}
