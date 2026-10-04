//! `redqueen`: The Red Queen command line (and, later, the desktop app).

mod status;
mod summary;

use std::io::{self, Write};
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use rq_core::TemperatureUnit;
use rq_hardware::{ProbeContext, ProbeReport, SystemRoot};
use tracing_subscriber::EnvFilter;

/// The Red Queen: control center for Acer Nitro laptops.
#[derive(Parser)]
#[command(name = "redqueen", version, about)]
struct Cli {
    /// Which message bus the daemon is on (`session` is for development).
    #[arg(long, value_enum, default_value = "system", global = true)]
    bus: Bus,
    /// Show temperatures in Fahrenheit.
    #[arg(long, global = true)]
    fahrenheit: bool,
    #[command(subcommand)]
    command: Command,
}

/// A D-Bus message bus.
#[derive(Clone, Copy, ValueEnum)]
pub enum Bus {
    /// The system bus.
    System,
    /// The session bus.
    Session,
}

#[derive(Subcommand)]
enum Command {
    /// Detect hardware and report what this machine supports.
    Probe(ProbeArgs),
    /// Show live status from the daemon.
    Status,
    /// Daemon commands.
    #[command(subcommand)]
    Daemon(DaemonCommand),
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Show whether the daemon is running and how it is configured.
    Status,
}

#[derive(Args)]
#[group(multiple = false)]
struct ProbeArgs {
    /// Machine-readable JSON with personal identifiers removed,
    /// suitable for attaching to an issue report.
    #[arg(long)]
    json: bool,
    /// Human-readable summary (default).
    #[arg(long)]
    summary: bool,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("REDQUEEN_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(io::stderr)
        .init();

    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("redqueen: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let unit = if cli.fahrenheit {
        TemperatureUnit::Fahrenheit
    } else {
        TemperatureUnit::Celsius
    };
    match cli.command {
        Command::Probe(args) => probe(&args),
        Command::Status => block_on(status::status(cli.bus, unit, &mut io::stdout().lock())),
        Command::Daemon(DaemonCommand::Status) => {
            block_on(status::daemon_status(cli.bus, &mut io::stdout().lock()))
        }
    }
}

fn block_on<F: std::future::Future<Output = anyhow::Result<()>>>(f: F) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?
        .block_on(f)
}

fn probe(args: &ProbeArgs) -> anyhow::Result<()> {
    let root = SystemRoot::host();
    let ctx = ProbeContext {
        tool_version: format!("redqueen {}", env!("CARGO_PKG_VERSION")),
        desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
        session_type: std::env::var("XDG_SESSION_TYPE").ok(),
        sensitive: personal_tokens(),
    };
    let report = ProbeReport::collect(&root, &ctx);
    let mut out = io::stdout().lock();
    if args.json {
        let json = report.to_redacted_json(&root, &ctx);
        serde_json::to_writer_pretty(&mut out, &json).context("writing JSON")?;
        writeln!(out)?;
    } else {
        summary::write(&mut out, &report)?;
    }
    Ok(())
}

/// The current user's name and home directory, which must not appear in
/// reports.
fn personal_tokens() -> Vec<String> {
    let mut tokens: Vec<String> = ["USER", "LOGNAME", "HOME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .filter(|v| !v.is_empty() && v != "/")
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}
