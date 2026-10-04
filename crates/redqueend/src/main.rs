//! `redqueend`: the Red Queen system daemon.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, ValueEnum};
use redqueend::authz::Authorizer;
use redqueend::config::{SAFETY_FILE, load_safety};
use redqueend::persist::{DEFAULT_FLAG_PATH, FileFlag};
use redqueend::restore::{Restored, restore_fans_standalone};
use redqueend::runtime::{spawn_client_cleanup, spawn_core};
use redqueend::service::DaemonService;
use redqueend::sleep::SleepGuard;
use redqueend::state::{Backends, Config, Shared};
use rq_hardware::SystemRoot;
use rq_ipc::{BUS_NAME, OBJECT_PATH};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Clone, Copy, ValueEnum)]
enum Bus {
    /// The system bus (production).
    System,
    /// The session bus (development only).
    Session,
}

/// The Red Queen system daemon.
#[derive(Parser)]
#[command(name = "redqueend", version, about)]
struct Args {
    /// Which bus to serve on.
    #[arg(long, value_enum, default_value = "system")]
    bus: Bus,
    /// Telemetry interval in milliseconds (250–60000).
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,
    /// Hand the fans back to the firmware if a previous run left them under
    /// manual control, then exit. Run by the service's stop hook.
    #[arg(long)]
    restore_auto: bool,
}

fn init_logging() {
    let filter = EnvFilter::try_from_env("REDQUEEN_LOG").unwrap_or_else(|_| "info".into());
    let registry = tracing_subscriber::registry().with(filter);
    // Under systemd (stderr is the journal) use native journald fields;
    // otherwise log to the terminal.
    if std::env::var_os("JOURNAL_STREAM").is_some()
        && !std::io::stderr().is_terminal()
        && let Ok(journald) = tracing_journald::layer()
    {
        registry.with(journald).init();
        return;
    }
    registry
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
}

fn load_limits() -> rq_core::SafetyConfig {
    let (config, warnings) = load_safety(Path::new(SAFETY_FILE));
    for w in warnings {
        warn!("{w}");
    }
    config
}

fn restore_auto() -> anyhow::Result<()> {
    let flag = Arc::new(FileFlag::new(DEFAULT_FLAG_PATH));
    match restore_fans_standalone(&SystemRoot::host(), flag, load_limits())? {
        Restored::NothingToDo => info!("no manual fan control to undo"),
        Restored::NoInterface => {
            warn!(
                "manual control may be active but no fan interface is available; the marker stays"
            )
        }
        Restored::Done => info!("fans returned to automatic control"),
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_logging();

    if args.restore_auto {
        return restore_auto();
    }

    let config = Config::new(Duration::from_millis(args.interval_ms));
    let backends = Backends::host(load_limits());
    let safety = backends.safety;
    // Creating the shared state also finishes any restore a crashed
    // previous run left undone.
    let shared = Arc::new(Shared::new(SystemRoot::host(), config, backends));
    info!(
        version = env!("CARGO_PKG_VERSION"),
        interval_ms = args.interval_ms,
        min_fan_percent = safety.min_percent.get(),
        critical_cpu_celsius = safety.critical_cpu.celsius() as u32,
        critical_gpu_celsius = safety.critical_gpu.celsius() as u32,
        "starting redqueend"
    );

    let authorizer = Arc::new(match args.bus {
        Bus::System => Authorizer::polkit().await.context("connecting to polkit")?,
        Bus::Session => Authorizer::DenyAll(
            "hardware control is disabled on the development (session) bus".into(),
        ),
    });

    let builder = match args.bus {
        Bus::System => zbus::connection::Builder::system(),
        Bus::Session => zbus::connection::Builder::session(),
    }
    .context("connecting to the message bus")?;
    let conn = builder
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, DaemonService::new(shared.clone(), authorizer))?
        .build()
        .await
        .with_context(|| format!("claiming {BUS_NAME} (is the D-Bus policy installed?)"))?;
    info!(name = BUS_NAME, "serving on D-Bus");

    let mut tasks = spawn_core(&shared, &conn);
    spawn_client_cleanup(&mut tasks, &shared, &conn);

    // Suspend handling needs logind on the system bus.
    if matches!(args.bus, Bus::System) {
        match SleepGuard::connect(&conn).await {
            Ok(guard) => {
                tasks.spawn(guard.run(shared.clone(), conn.clone()));
            }
            Err(e) => warn!(error = %e, "no suspend handling (is systemd-logind running?)"),
        }
    }

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut failed = false;
    tokio::select! {
        _ = term.recv() => info!("SIGTERM received, shutting down"),
        _ = int.recv() => info!("SIGINT received, shutting down"),
        finished = async {
            match tasks.join_next().await {
                Some(r) => r,
                None => std::future::pending().await,
            }
        } => {
            // A background task (telemetry, rediscovery, suspend handling)
            // ended: nothing is supervising the fans any more. Hand them
            // back and exit so systemd restarts a healthy daemon.
            error!(result = ?finished, "a core task ended unexpectedly");
            failed = true;
        }
    }

    // Whatever the reason, leave the fans under firmware control.
    let worker = shared.clone();
    match tokio::task::spawn_blocking(move || worker.restore_fans()).await {
        Ok(Ok(())) => info!("fans are under automatic control"),
        Ok(Err(e)) => {
            error!(error = %e, "could not confirm automatic fan control; the stop hook will retry")
        }
        Err(e) => error!(error = %e, "restoring the fans failed"),
    }
    tasks.shutdown().await;
    if failed {
        anyhow::bail!("exiting after a background task failed");
    }
    Ok(())
}
