//! End-to-end tests: the daemon on a private D-Bus daemon, talking to real
//! clients, with hardware faked by the ANV15-51 fixture.

use std::io::{self, BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use redqueend::authz::Authorizer;
use redqueend::runtime::{spawn_client_cleanup, spawn_core};
use redqueend::service::DaemonService;
use redqueend::state::{Config, ProfileIoFactory, Shared};
use rq_core::{Feature, TelemetrySample, ThermalProfileId};
use rq_hardware::SystemRoot;
use rq_hardware::profile::ProfileIo;
use rq_ipc::{BUS_NAME, ChoiceState, Client, ErrorKind, MAX_SUBSCRIBERS, OBJECT_PATH};
use rq_testkit::{FakeSystem, presets};
use zbus::connection::Builder;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PROFILE_FILE: &str = "sys/class/platform-profile/platform-profile-0/profile";

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
    fs: FakeSystem,
    shared: Arc<Shared>,
    server: zbus::Connection,
    address: String,
}

impl Harness {
    /// Read-only daemon (every privileged request is refused).
    async fn start() -> Option<Self> {
        Self::start_with(Authorizer::DenyAll("read-only test".into()), None).await
    }

    /// `None` when `dbus-daemon` isn't installed (reported on stderr).
    async fn start_with(authorizer: Authorizer, factory: Option<ProfileIoFactory>) -> Option<Self> {
        let Some(bus) = PrivateBus::start() else {
            eprintln!("SKIPPED: dbus-daemon is not available");
            return None;
        };
        let fs = presets::anv15_51(true).ok()?;
        let root = SystemRoot::at(fs.path());
        let config = Config::new(Duration::from_millis(250));
        let shared = Arc::new(match factory {
            Some(f) => Shared::with_profile_factory(root, config, f),
            None => Shared::new(root, config),
        });
        let server = Builder::address(bus.address.as_str())
            .ok()?
            .name(BUS_NAME)
            .ok()?
            .serve_at(
                OBJECT_PATH,
                DaemonService::new(shared.clone(), Arc::new(authorizer)),
            )
            .ok()?
            .build()
            .await
            .ok()?;
        let address = bus.address.clone();
        Some(Self {
            _bus: bus,
            fs,
            shared,
            server,
            address,
        })
    }

    async fn connect(&self) -> zbus::Result<zbus::Connection> {
        Builder::address(self.address.as_str())?.build().await
    }

    fn profile_file(&self) -> io::Result<String> {
        Ok(std::fs::read_to_string(self.fs.path().join(PROFILE_FILE))?
            .trim()
            .to_owned())
    }
}

macro_rules! harness {
    () => {
        match Harness::start().await {
            Some(h) => h,
            None => return Ok(()),
        }
    };
    ($authorizer:expr) => {
        match Harness::start_with($authorizer, None).await {
            Some(h) => h,
            None => return Ok(()),
        }
    };
    ($authorizer:expr, $factory:expr) => {
        match Harness::start_with($authorizer, Some($factory)).await {
            Some(h) => h,
            None => return Ok(()),
        }
    };
}

/// Simulated firmware with configurable misbehaviour.
#[derive(Debug)]
struct Firmware {
    active: Mutex<ThermalProfileId>,
    reject: Vec<ThermalProfileId>,
    ignore_writes: bool,
    writes: Mutex<Vec<ThermalProfileId>>,
}

impl Firmware {
    fn new(reject: Vec<ThermalProfileId>, ignore_writes: bool) -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(ThermalProfileId::Balanced),
            reject,
            ignore_writes,
            writes: Mutex::new(Vec::new()),
        })
    }

    fn factory(self: &Arc<Self>) -> ProfileIoFactory {
        let fw = self.clone();
        Arc::new(move |_, _| Some(fw.clone() as Arc<dyn ProfileIo>))
    }
}

impl ProfileIo for Firmware {
    fn choices(&self) -> Vec<ThermalProfileId> {
        ThermalProfileId::parse_choices("low-power quiet balanced balanced-performance performance")
    }
    fn read_active(&self) -> io::Result<ThermalProfileId> {
        Ok(self
            .active
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?
            .clone())
    }
    fn write_active(&self, p: &ThermalProfileId) -> io::Result<()> {
        self.writes
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?
            .push(p.clone());
        if self.reject.contains(p) {
            return Err(io::Error::from_raw_os_error(5)); // EIO, as acer-wmi returns
        }
        if !self.ignore_writes {
            *self
                .active
                .lock()
                .map_err(|_| io::Error::other("poisoned"))? = p.clone();
        }
        Ok(())
    }
    fn describe(&self) -> String {
        "simulated-firmware".into()
    }
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
    std::fs::remove_dir_all(h.fs.path().join("sys/devices/platform/acer-wmi/hwmon"))?;
    std::fs::remove_file(h.fs.path().join("sys/class/hwmon/hwmon7"))?;
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

// ---------------------------------------------------------------- write path

#[tokio::test]
async fn profile_choices_are_listed() -> TestResult {
    let h = harness!();
    let conn = h.connect().await?;
    let info = Client::new(&conn).await?.thermal_profiles().await?;
    assert!(info.available);
    assert_eq!(info.active, Some(ThermalProfileId::Balanced));
    assert_eq!(info.choices.len(), 5);
    assert!(
        info.choices
            .iter()
            .all(|c| c.state == ChoiceState::Available)
    );
    Ok(())
}

#[tokio::test]
async fn authorized_profile_change_is_written_verified_and_announced() -> TestResult {
    let h = harness!(Authorizer::AllowAll);
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;
    let mut changes = client.proxy().receive_thermal_profile_changed().await?;

    let result = client.set_thermal_profile(&ThermalProfileId::Quiet).await?;
    assert_eq!(result.requested, ThermalProfileId::Quiet);
    assert_eq!(result.active, ThermalProfileId::Quiet);
    assert_eq!(
        h.profile_file()?,
        "quiet",
        "the kernel attribute really changed"
    );

    let signal = tokio::time::timeout(Duration::from_secs(3), changes.next())
        .await?
        .ok_or("signal stream ended")?;
    let args = signal.args()?;
    assert_eq!(
        (args.previous.as_str(), args.current.as_str()),
        ("balanced", "quiet")
    );

    // The cache was invalidated: the very next sample shows the new profile.
    assert_eq!(
        h.shared.sample().thermal_profile,
        Some(ThermalProfileId::Quiet)
    );
    Ok(())
}

#[tokio::test]
async fn unauthorized_requests_change_nothing() -> TestResult {
    let h = harness!();
    let conn = h.connect().await?;
    let err = Client::new(&conn)
        .await?
        .set_thermal_profile(&ThermalProfileId::Quiet)
        .await
        .expect_err("must be refused");
    assert_eq!(err.kind(), Some(ErrorKind::NotAuthorized), "{err}");
    assert_eq!(h.profile_file()?, "balanced");
    Ok(())
}

#[tokio::test]
async fn bad_input_is_rejected_before_authorization_and_before_hardware() -> TestResult {
    // DenyAll: if validation ran *after* authorization these would report
    // NotAuthorized. InvalidArgument proves bad input never reaches polkit
    // (and so never produces a password prompt) or the hardware.
    let h = harness!();
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;
    // (A NUL byte can't be tested here: D-Bus itself refuses such strings.)
    let long = "a".repeat(65);
    for bad in [
        "",
        "Quiet",
        "../../etc/passwd",
        "quiet\n",
        "a b",
        "a_b",
        long.as_str(),
    ] {
        let err = client
            .proxy()
            .set_thermal_profile(bad)
            .await
            .expect_err("invalid");
        let err = rq_ipc::ClientError::from(err);
        assert_eq!(
            err.kind(),
            Some(ErrorKind::InvalidArgument),
            "{bad:?}: {err}"
        );
    }
    assert_eq!(h.profile_file()?, "balanced");
    Ok(())
}

#[tokio::test]
async fn profiles_the_hardware_does_not_offer_are_refused() -> TestResult {
    let h = harness!(Authorizer::AllowAll);
    let conn = h.connect().await?;
    let err = Client::new(&conn)
        .await?
        .set_thermal_profile(&ThermalProfileId::Other("turbo".into()))
        .await
        .expect_err("not offered");
    assert_eq!(err.kind(), Some(ErrorKind::InvalidArgument), "{err}");
    assert_eq!(h.profile_file()?, "balanced");
    Ok(())
}

#[tokio::test]
async fn firmware_rejection_marks_the_profile_unsupported() -> TestResult {
    let fw = Firmware::new(vec![ThermalProfileId::Performance], false);
    let h = harness!(Authorizer::AllowAll, fw.factory());
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;

    let err = client
        .set_thermal_profile(&ThermalProfileId::Performance)
        .await
        .expect_err("rejected");
    assert_eq!(err.kind(), Some(ErrorKind::Rejected), "{err}");
    assert!(err.to_string().contains("still 'balanced'"), "{err}");

    let info = client.thermal_profiles().await?;
    assert_eq!(
        info.active,
        Some(ThermalProfileId::Balanced),
        "real state retained"
    );
    let perf = info
        .choices
        .iter()
        .find(|c| c.id == ThermalProfileId::Performance)
        .ok_or("choice")?;
    assert_eq!(perf.state, ChoiceState::Unsupported);

    let writes = fw.writes.lock().map_err(|_| "poisoned")?.len();
    let err = client
        .set_thermal_profile(&ThermalProfileId::Performance)
        .await
        .expect_err("disabled");
    assert_eq!(err.kind(), Some(ErrorKind::Unsupported), "{err}");
    assert_eq!(
        fw.writes.lock().map_err(|_| "poisoned")?.len(),
        writes,
        "firmware not retried"
    );

    assert!(
        client
            .set_thermal_profile(&ThermalProfileId::Quiet)
            .await
            .is_ok(),
        "others still work"
    );
    Ok(())
}

#[tokio::test]
async fn accepted_but_unapplied_changes_report_the_real_state() -> TestResult {
    let fw = Firmware::new(vec![], true);
    let h = harness!(Authorizer::AllowAll, fw.factory());
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;
    let err = client
        .set_thermal_profile(&ThermalProfileId::Quiet)
        .await
        .expect_err("unconfirmed");
    assert_eq!(err.kind(), Some(ErrorKind::NotConfirmed), "{err}");
    assert!(
        err.to_string().contains("active profile is 'balanced'"),
        "{err}"
    );
    assert_eq!(
        client.thermal_profiles().await?.active,
        Some(ThermalProfileId::Balanced)
    );
    Ok(())
}

#[tokio::test]
async fn requests_are_rate_limited_before_authorization() -> TestResult {
    // DenyAll: a request that passes the limiter is refused as NotAuthorized;
    // once the burst is spent the limiter answers first, so a flood can never
    // reach polkit and spam password prompts.
    let h = harness!();
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;
    for i in 0..8 {
        let err = client
            .set_thermal_profile(&ThermalProfileId::Quiet)
            .await
            .expect_err("denied");
        assert_eq!(
            err.kind(),
            Some(ErrorKind::NotAuthorized),
            "request {i}: {err}"
        );
    }
    let err = client
        .set_thermal_profile(&ThermalProfileId::Quiet)
        .await
        .expect_err("limited");
    assert_eq!(err.kind(), Some(ErrorKind::RateLimited), "{err}");

    // Another client has its own budget.
    let other = h.connect().await?;
    let err = Client::new(&other)
        .await?
        .set_thermal_profile(&ThermalProfileId::Quiet)
        .await
        .expect_err("denied");
    assert_eq!(err.kind(), Some(ErrorKind::NotAuthorized));
    Ok(())
}

#[tokio::test]
async fn machines_without_profiles_say_so() -> TestResult {
    let h = harness!(Authorizer::AllowAll, Arc::new(|_, _| None));
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;
    let info = client.thermal_profiles().await?;
    assert!(!info.available && info.choices.is_empty());
    let err = client
        .set_thermal_profile(&ThermalProfileId::Quiet)
        .await
        .expect_err("unavailable");
    assert_eq!(err.kind(), Some(ErrorKind::Unavailable), "{err}");
    Ok(())
}
