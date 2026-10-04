//! `redqueend`: the Red Queen system daemon.

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, ValueEnum};
use redqueend::authz::Authorizer;
use redqueend::runtime::{spawn_client_cleanup, spawn_core};
use redqueend::service::DaemonService;
use redqueend::state::{Config, Shared};
use rq_hardware::SystemRoot;
use rq_ipc::{BUS_NAME, OBJECT_PATH};
use std::sync::Arc;
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_logging();

    let config = Config::new(Duration::from_millis(args.interval_ms));
    let shared = Arc::new(Shared::new(SystemRoot::host(), config));
    info!(
        version = env!("CARGO_PKG_VERSION"),
        interval_ms = args.interval_ms,
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

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => info!("SIGTERM received, shutting down"),
        _ = int.recv() => info!("SIGINT received, shutting down"),
    }
    tasks.shutdown().await;
    Ok(())
}
