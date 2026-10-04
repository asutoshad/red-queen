//! End-to-end tests: the daemon on a private D-Bus daemon, talking to real
//! clients, with hardware faked by the ANV15-51 fixture.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use redqueend::runtime::{spawn_client_cleanup, spawn_core};
use redqueend::service::DaemonService;
use redqueend::state::{Config, Shared};
use rq_core::{Feature, TelemetrySample};
use rq_hardware::SystemRoot;
use rq_ipc::{BUS_NAME, Client, MAX_SUBSCRIBERS, OBJECT_PATH};
use rq_testkit::{FakeSystem, presets};
use zbus::connection::Builder;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A private `dbus-daemon`, killed on drop.
struct PrivateBus {
    child: Child,
    address: String,
}

impl PrivateBus {
    fn start() -> Option<Self> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut line)
            .ok()?;
        let address = line.trim().to_owned();
        (!address.is_empty()).then_some(Self { child, address })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Harness {
    _bus: PrivateBus,
    _fs: FakeSystem,
    shared: Arc<Shared>,
    server: zbus::Connection,
    address: String,
}

impl Harness {
    /// `None` when `dbus-daemon` isn't installed (reported on stderr).
    async fn start() -> Option<Self> {
        let Some(bus) = PrivateBus::start() else {
            eprintln!("SKIPPED: dbus-daemon is not available");
            return None;
        };
        let fs = presets::anv15_51(true).ok()?;
        let shared = Arc::new(Shared::new(
            SystemRoot::at(fs.path()),
            Config::new(Duration::from_millis(250)),
        ));
        let server = Builder::address(bus.address.as_str())
            .ok()?
            .name(BUS_NAME)
            .ok()?
            .serve_at(OBJECT_PATH, DaemonService::new(shared.clone()))
            .ok()?
            .build()
            .await
            .ok()?;
        let address = bus.address.clone();
        Some(Self {
            _bus: bus,
            _fs: fs,
            shared,
            server,
            address,
        })
    }

    async fn connect(&self) -> zbus::Result<zbus::Connection> {
        Builder::address(self.address.as_str())?.build().await
    }
}

macro_rules! harness {
    () => {
        match Harness::start().await {
            Some(h) => h,
            None => return Ok(()),
        }
    };
}

#[tokio::test]
async fn reports_capabilities_and_identity() -> TestResult {
    let h = harness!();
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;

    let caps = client.capabilities().await?;
    let tp = caps
        .iter()
        .find(|c| c.feature == Feature::ThermalProfiles)
        .ok_or("no profiles")?;
    assert!(tp.supported);
    let fc = caps
        .iter()
        .find(|c| c.feature == Feature::FanControl)
        .ok_or("no fan control")?;
    assert!(!fc.supported, "fan control must not be faked");

    let id = client.hardware_identity().await?;
    assert_eq!(id.product_name.as_deref(), Some("Nitro ANV15-51"));
    let raw = serde_json::to_string(&id)?;
    assert!(!raw.contains("SECRET"), "identity leaked a secret");

    assert_eq!(client.proxy().version().await?, env!("CARGO_PKG_VERSION"));
    Ok(())
}

#[tokio::test]
async fn telemetry_and_history() -> TestResult {
    let h = harness!();
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;

    assert_eq!(client.telemetry().await?, None, "nothing sampled yet");
    assert!(client.history(60).await?.is_empty());

    h.shared.sample();
    h.shared.sample();
    let latest = client.telemetry().await?.ok_or("no sample")?;
    assert_eq!(latest.fans.len(), 2);
    assert_eq!(client.history(60).await?.len(), 2);

    let st = client.status().await?;
    assert_eq!(st.history_len, 2);
    assert_eq!(
        st.history_capacity,
        3600 * 4,
        "250 ms interval keeps 60 minutes"
    );
    assert_eq!(st.sample_interval_ms, 250);
    Ok(())
}

#[tokio::test]
async fn history_rejects_bad_arguments() -> TestResult {
    let h = harness!();
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;
    for bad in [0u32, 3601, u32::MAX] {
        let err = client
            .proxy()
            .get_history(bad)
            .await
            .expect_err("must be rejected");
        assert!(err.to_string().contains("InvalidArgs"), "{bad}: {err}");
    }
    assert!(client.proxy().get_history(1).await.is_ok());
    assert!(client.proxy().get_history(3600).await.is_ok());
    Ok(())
}

#[tokio::test]
async fn subscription_lifecycle_and_limit() -> TestResult {
    let h = harness!();
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;

    assert!(!h.shared.has_subscribers());
    client.proxy().subscribe().await?;
    client.proxy().subscribe().await?; // idempotent
    assert_eq!(h.shared.status().subscribers, 1);
    client.proxy().unsubscribe().await?;
    assert!(!h.shared.has_subscribers());

    // Fill every slot with distinct connections.
    let mut held = Vec::new();
    for _ in 0..MAX_SUBSCRIBERS {
        let c = h.connect().await?;
        Client::new(&c).await?.proxy().subscribe().await?;
        held.push(c);
    }
    let extra = h.connect().await?;
    let err = Client::new(&extra)
        .await?
        .proxy()
        .subscribe()
        .await
        .expect_err("limit");
    assert!(err.to_string().contains("LimitsExceeded"), "{err}");
    assert_eq!(h.shared.status().subscribers, MAX_SUBSCRIBERS);
    Ok(())
}

#[tokio::test]
async fn disconnected_clients_are_removed() -> TestResult {
    let h = harness!();
    let mut tasks = tokio::task::JoinSet::new();
    spawn_client_cleanup(&mut tasks, &h.shared, &h.server);

    let conn = h.connect().await?;
    Client::new(&conn).await?.proxy().subscribe().await?;
    assert_eq!(h.shared.status().subscribers, 1);
    drop(conn);

    for _ in 0..40 {
        if !h.shared.has_subscribers() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("subscriber was not cleaned up after disconnect".into())
}

#[tokio::test]
async fn live_telemetry_signals_reach_subscribers() -> TestResult {
    let h = harness!();
    let _tasks = spawn_core(&h.shared, &h.server);

    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;
    let mut signals = client.proxy().receive_telemetry_updated().await?;
    client.proxy().subscribe().await?;

    let signal = tokio::time::timeout(Duration::from_secs(5), signals.next())
        .await?
        .ok_or("signal stream ended")?;
    let sample: TelemetrySample = serde_json::from_str(&signal.args()?.sample)?;
    assert_eq!(sample.fans.len(), 2);
    assert!(sample.timestamp_ms > 0);
    Ok(())
}

#[tokio::test]
async fn rediscovery_notices_hardware_changes() -> TestResult {
    let h = harness!();
    let before = h.shared.status().discoveries;
    assert!(
        !h.shared.rediscover(),
        "unchanged hardware must not report a change"
    );
    assert_eq!(h.shared.status().discoveries, before + 1);

    // The driver is unloaded: the acer hwmon chip disappears.
    std::fs::remove_dir_all(h._fs.path().join("sys/devices/platform/acer-wmi/hwmon"))?;
    std::fs::remove_file(h._fs.path().join("sys/class/hwmon/hwmon7"))?;
    assert!(
        h.shared.rediscover(),
        "losing the fan sensors is a capability change"
    );
    let fans = h
        .shared
        .capabilities()
        .into_iter()
        .find(|c| c.feature == Feature::FanTelemetry)
        .ok_or("no capability")?;
    assert!(!fans.supported);
    Ok(())
}
