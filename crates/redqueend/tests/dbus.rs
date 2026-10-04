//! End-to-end tests: the daemon on a private D-Bus daemon, talking to real
//! clients, with hardware faked by the ANV15-51 fixture.

// The `zbus::interface` macro generates undocumented methods for the fake.
#![allow(missing_docs)]

use std::io::{self, BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use redqueend::authz::Authorizer;
use redqueend::persist::{ManualFlag, MemoryFlag};
use redqueend::runtime::{spawn_client_cleanup, spawn_core};
use redqueend::service::DaemonService;
use redqueend::state::{Backends, Config, FanIoFactory, ProfileIoFactory, Shared};
use rq_core::{Feature, SafetyConfig, TelemetrySample, ThermalProfileId};
use rq_hardware::SystemRoot;
use rq_hardware::profile::ProfileIo;
use rq_ipc::{BUS_NAME, ChoiceState, Client, ErrorKind, MAX_SUBSCRIBERS, OBJECT_PATH};
use rq_testkit::{FakeSystem, presets};
use zbus::connection::Builder;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PROFILE_FILE: &str = "sys/class/platform-profile/platform-profile-0/profile";
const HWMON: &str = "sys/class/hwmon/hwmon7";

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

/// How to build a test harness.
struct Opts {
    authorizer: Authorizer,
    profile: Option<ProfileIoFactory>,
    fans: Option<FanIoFactory>,
    /// Expose `pwmN` files, as a kernel with fan control would.
    pwm: bool,
}

impl Opts {
    fn new(authorizer: Authorizer) -> Self {
        Self {
            authorizer,
            profile: None,
            fans: None,
            pwm: false,
        }
    }
}

struct Harness {
    _bus: PrivateBus,
    fs: FakeSystem,
    shared: Arc<Shared>,
    flag: Arc<MemoryFlag>,
    server: zbus::Connection,
    address: String,
}

impl Harness {
    /// Read-only daemon (every privileged request is refused).
    async fn start() -> Option<Self> {
        Self::start_opts(Opts::new(Authorizer::DenyAll("read-only test".into()))).await
    }

    async fn start_with(authorizer: Authorizer, factory: Option<ProfileIoFactory>) -> Option<Self> {
        Self::start_opts(Opts {
            profile: factory,
            ..Opts::new(authorizer)
        })
        .await
    }

    /// `None` when `dbus-daemon` isn't installed (reported on stderr).
    async fn start_opts(opts: Opts) -> Option<Self> {
        let Some(bus) = PrivateBus::start() else {
            eprintln!("SKIPPED: dbus-daemon is not available");
            return None;
        };
        let fs = presets::anv15_51(true).ok()?;
        if opts.pwm {
            presets::add_acer_pwm(&fs).ok()?;
        }
        let flag = Arc::new(MemoryFlag::new());
        let mut backends = Backends::host_with_flag(SafetyConfig::default(), flag.clone());
        if let Some(f) = opts.profile {
            backends = backends.with_profile(f);
        }
        if let Some(f) = opts.fans {
            backends = backends.with_fans(f);
        }
        let shared = Arc::new(Shared::new(
            SystemRoot::at(fs.path()),
            Config::new(Duration::from_millis(250)),
            backends,
        ));
        let server = Builder::address(bus.address.as_str())
            .ok()?
            .name(BUS_NAME)
            .ok()?
            .serve_at(
                OBJECT_PATH,
                DaemonService::new(shared.clone(), Arc::new(opts.authorizer)),
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
            flag,
            server,
            address,
        })
    }

    async fn connect(&self) -> zbus::Result<zbus::Connection> {
        Builder::address(self.address.as_str())?.build().await
    }

    fn profile_file(&self) -> io::Result<String> {
        self.read(PROFILE_FILE)
    }

    fn read(&self, rel: &str) -> io::Result<String> {
        Ok(std::fs::read_to_string(self.fs.path().join(rel))?
            .trim()
            .to_owned())
    }

    /// `(pwm, pwm_enable)` of fan `n`.
    fn fan(&self, n: u8) -> io::Result<(String, String)> {
        Ok((
            self.read(&format!("{HWMON}/pwm{n}"))?,
            self.read(&format!("{HWMON}/pwm{n}_enable"))?,
        ))
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

/// A harness whose fake sysfs exposes fan control (`pwmN` files).
macro_rules! fan_harness {
    ($authorizer:expr) => {
        match Harness::start_opts(Opts {
            pwm: true,
            ..Opts::new($authorizer)
        })
        .await
        {
            Some(h) => h,
            None => return Ok(()),
        }
    };
}

/// Polls `f` until it returns true or `secs` pass.
async fn wait_until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..(secs * 20) {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f()
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
    // The fixture is a tested model on its tested BIOS, where the firmware
    // is known to reject `performance`: it is listed but disabled.
    for c in &info.choices {
        let expected = if c.id == ThermalProfileId::Performance {
            ChoiceState::Unsupported
        } else {
            ChoiceState::Available
        };
        assert_eq!(c.state, expected, "{:?}", c.id);
    }
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
    // An untested BIOS: nothing is known in advance, so the firmware decides.
    h.fs.file("/sys/class/dmi/id/bios_version", "V9.99")?;
    h.shared.rediscover();
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

#[tokio::test]
async fn profiles_known_to_be_rejected_never_reach_the_firmware() -> TestResult {
    // This model + BIOS was tested: `performance` is rejected by the
    // firmware, so the daemon refuses it without asking.
    let fw = Firmware::new(vec![], false); // this firmware would even accept it
    let h = harness!(Authorizer::AllowAll, fw.factory());
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;
    let err = client
        .set_thermal_profile(&ThermalProfileId::Performance)
        .await
        .expect_err("known");
    assert_eq!(err.kind(), Some(ErrorKind::Unsupported), "{err}");
    assert!(err.to_string().contains("verified on hardware"), "{err}");
    assert!(
        fw.writes.lock().map_err(|_| "poisoned")?.is_empty(),
        "no write was attempted"
    );
    assert!(
        client
            .set_thermal_profile(&ThermalProfileId::Quiet)
            .await
            .is_ok()
    );
    Ok(())
}

// ------------------------------------------------------------------- fans

#[tokio::test]
async fn fans_are_listed_with_their_limits() -> TestResult {
    let h = fan_harness!(Authorizer::DenyAll("read-only".into()));
    let conn = h.connect().await?;
    let info = Client::new(&conn).await?.fans().await?;
    assert!(info.available && info.controllable);
    let names: Vec<&str> = info.fans.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(names, ["cpu", "gpu"]);
    assert_eq!(info.fans[0].rpm, Some(rq_core::Rpm(2331)));
    assert_eq!(info.fans[0].mode, Some(rq_core::FanMode::Auto));
    assert!(
        !info.fans[0].role_verified,
        "role comes from channel order only"
    );
    assert_eq!(info.safety.min_percent, 30);
    assert_eq!(info.safety.critical_cpu_celsius, 90);
    assert!(!info.safety.tripped);
    Ok(())
}

#[tokio::test]
async fn without_pwm_support_fans_are_not_controllable() -> TestResult {
    let h = harness!(Authorizer::AllowAll);
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;
    let info = client.fans().await?;
    assert!(!info.available && !info.controllable && info.fans.is_empty());
    let err = client.set_fan_speed("cpu", 60).await.expect_err("no fans");
    assert_eq!(err.kind(), Some(ErrorKind::Unavailable), "{err}");
    let err = client.set_fan_mode("max").await.expect_err("no fans");
    assert_eq!(err.kind(), Some(ErrorKind::Unavailable), "{err}");
    assert!(
        client.set_fan_mode("auto").await.is_ok(),
        "returning to automatic never fails for lack of fans"
    );
    Ok(())
}

#[tokio::test]
async fn auto_needs_no_authorization_but_max_and_custom_do() -> TestResult {
    let h = fan_harness!(Authorizer::DenyAll("not allowed".into()));
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;

    let err = client
        .set_fan_mode("max")
        .await
        .expect_err("needs authorization");
    assert_eq!(err.kind(), Some(ErrorKind::NotAuthorized), "{err}");
    let err = client
        .set_fan_speed("cpu", 50)
        .await
        .expect_err("needs authorization");
    assert_eq!(err.kind(), Some(ErrorKind::NotAuthorized), "{err}");
    assert_eq!(h.fan(1)?, ("128".into(), "2".into()), "untouched");
    assert!(!h.flag.is_set());

    client
        .set_fan_mode("auto")
        .await
        .expect("auto is always allowed");
    Ok(())
}

#[tokio::test]
async fn bad_fan_requests_never_prompt_or_touch_hardware() -> TestResult {
    // DenyAll: had authorization run first these would say NotAuthorized.
    let h = fan_harness!(Authorizer::DenyAll("not allowed".into()));
    // A fresh client per request: each client has its own rate-limit budget
    // and this test is about validation, not throttling.
    for (fan, pct) in [
        ("nope", 50),
        ("", 50),
        ("acer/1", 50),
        ("cpu", 29),
        ("cpu", 0),
        ("cpu", 101),
        ("cpu", u32::MAX),
    ] {
        let conn = h.connect().await?;
        let err = Client::new(&conn)
            .await?
            .set_fan_speed(fan, pct)
            .await
            .expect_err("invalid");
        assert_eq!(
            err.kind(),
            Some(ErrorKind::InvalidArgument),
            "{fan:?} {pct}: {err}"
        );
    }
    for mode in ["", "turbo", "AUTO", "custom", "off"] {
        let conn = h.connect().await?;
        let err = Client::new(&conn)
            .await?
            .set_fan_mode(mode)
            .await
            .expect_err("invalid");
        assert_eq!(
            err.kind(),
            Some(ErrorKind::InvalidArgument),
            "{mode:?}: {err}"
        );
    }
    assert_eq!(h.fan(1)?, ("128".into(), "2".into()));
    assert_eq!(h.fan(2)?, ("128".into(), "2".into()));
    assert!(
        !h.flag.is_set(),
        "nothing was recorded because nothing happened"
    );
    Ok(())
}

#[tokio::test]
async fn authorized_manual_control_writes_verifies_and_announces() -> TestResult {
    let h = fan_harness!(Authorizer::AllowAll);
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;
    let mut changes = client.proxy().receive_fan_mode_changed().await?;

    let info = client.set_fan_speed("cpu", 60).await?;
    assert_eq!(
        h.fan(1)?,
        ("153".into(), "1".into()),
        "60 % on the 0-255 scale, manual mode"
    );
    assert_eq!(
        h.fan(2)?,
        ("128".into(), "2".into()),
        "the GPU fan stays automatic"
    );
    assert!(h.flag.is_set(), "the crash-recovery marker was recorded");
    let cpu = info.fans.iter().find(|f| f.id == "cpu").ok_or("cpu")?;
    assert_eq!(cpu.mode, Some(rq_core::FanMode::Custom));
    assert_eq!(cpu.duty_percent, Some(60));
    assert_eq!(cpu.requested_percent, Some(60));
    let signal = tokio::time::timeout(Duration::from_secs(3), changes.next())
        .await?
        .ok_or("signal")?;
    assert_eq!(signal.args()?.summary, "custom");

    client.set_fan_speed("gpu", 80).await?;
    assert_eq!(h.fan(2)?, ("204".into(), "1".into()));

    client.set_fan_mode("max").await?;
    assert_eq!(
        (h.fan(1)?.1, h.fan(2)?.1),
        ("0".into(), "0".into()),
        "full speed"
    );

    client.set_fan_mode("auto").await?;
    assert_eq!((h.fan(1)?.1, h.fan(2)?.1), ("2".into(), "2".into()));
    assert!(
        !h.flag.is_set(),
        "marker cleared once automatic control is confirmed"
    );
    Ok(())
}

#[tokio::test]
async fn overheating_hands_fans_back_through_the_real_telemetry_loop() -> TestResult {
    let h = fan_harness!(Authorizer::AllowAll);
    let _tasks = spawn_core(&h.shared, &h.server);
    let conn = h.connect().await?;
    let client = Client::new(&conn).await?;
    let mut events = client.proxy().receive_safety_event().await?;

    client.set_fan_speed("cpu", 50).await?;
    assert_eq!(h.fan(1)?.1, "1");

    // The CPU gets critically hot.
    h.fs.file(
        "/sys/devices/platform/coretemp.0/hwmon/hwmon5/temp1_input",
        "95000",
    )?;
    assert!(
        wait_until(8, || h.fan(1).is_ok_and(|(_, enable)| enable == "2")).await,
        "the daemon should have restored automatic control"
    );
    assert!(!h.flag.is_set());

    let event = tokio::time::timeout(Duration::from_secs(3), events.next())
        .await?
        .ok_or("event")?;
    let args = event.args()?;
    assert_eq!(args.code, "cpu_critical");
    assert!(args.message.contains("95"), "{}", args.message);

    // Manual control is locked out afterwards; automatic always works.
    let info = client.fans().await?;
    assert!(info.safety.tripped && info.safety.lockout_remaining_s > 0);
    let err = client
        .set_fan_speed("cpu", 50)
        .await
        .expect_err("locked out");
    assert_eq!(err.kind(), Some(ErrorKind::Unavailable), "{err}");
    assert!(err.to_string().contains("locked out"), "{err}");
    assert!(client.set_fan_mode("auto").await.is_ok());
    Ok(())
}

#[tokio::test]
async fn a_restart_after_a_crash_restores_automatic_control() -> TestResult {
    let h = fan_harness!(Authorizer::AllowAll);
    h.shared.set_fan_custom("cpu", 50)?;
    h.shared.set_fan_custom("gpu", 70)?;
    assert_eq!((h.fan(1)?.1, h.fan(2)?.1), ("1".into(), "1".into()));
    assert!(h.flag.is_set());

    // The daemon dies without cleaning up (marker still set). A new
    // instance starting on the same hardware must fix it.
    let backends = Backends::host_with_flag(SafetyConfig::default(), h.flag.clone());
    let reborn = Shared::new(
        SystemRoot::at(h.fs.path()),
        Config::new(Duration::from_secs(1)),
        backends,
    );
    assert_eq!(
        (h.fan(1)?.1, h.fan(2)?.1),
        ("2".into(), "2".into()),
        "fans are automatic again"
    );
    assert!(!h.flag.is_set());
    assert!(!reborn.manual_fans_active());
    Ok(())
}

#[tokio::test]
async fn losing_the_fan_interface_during_manual_control_is_reported() -> TestResult {
    let h = fan_harness!(Authorizer::AllowAll);
    h.shared.set_fan_custom("cpu", 50)?;
    for n in [1, 2] {
        std::fs::remove_file(h.fs.path().join(format!("{HWMON}/pwm{n}")))?;
        std::fs::remove_file(h.fs.path().join(format!("{HWMON}/pwm{n}_enable")))?;
    }
    let (_, trip) = h.shared.rediscover_full();
    assert_eq!(trip, Some(rq_core::TripReason::InterfaceLost));
    assert!(!h.shared.manual_fans_active());
    assert!(!h.shared.fans_info().controllable);
    Ok(())
}

#[tokio::test]
async fn shutdown_restores_the_fans() -> TestResult {
    let h = fan_harness!(Authorizer::AllowAll);
    h.shared.set_fan_max()?;
    assert_eq!((h.fan(1)?.1, h.fan(2)?.1), ("0".into(), "0".into()));
    h.shared.restore_fans()?; // what the daemon does on SIGTERM
    assert_eq!((h.fan(1)?.1, h.fan(2)?.1), ("2".into(), "2".into()));
    assert!(!h.flag.is_set());
    Ok(())
}

// ------------------------------------------------------------------ suspend

/// A stand-in for systemd-logind: hands out inhibitor locks (one end of a
/// socket pair) and lets the test announce sleep.
struct FakeLogind {
    /// Our end of every lock we handed out. Reading EOF means the daemon
    /// closed its copy, i.e. released the lock.
    locks: Arc<Mutex<Vec<std::os::unix::net::UnixStream>>>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl FakeLogind {
    async fn inhibit(
        &self,
        what: String,
        _who: String,
        _why: String,
        mode: String,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        self.locks
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("poisoned".into()))?
            .push(ours);
        self.requests
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("poisoned".into()))?
            .push((what, mode));
        Ok(zbus::zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(
            theirs,
        )))
    }

    #[zbus(signal)]
    async fn prepare_for_sleep(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        start: bool,
    ) -> zbus::Result<()>;
}

#[tokio::test(flavor = "multi_thread")]
async fn fans_are_handed_back_before_the_sleep_lock_is_released() -> TestResult {
    let h = fan_harness!(Authorizer::AllowAll);
    let locks = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let logind = Builder::address(h.address.as_str())?
        .name("org.freedesktop.login1")?
        .serve_at(
            "/org/freedesktop/login1",
            FakeLogind {
                locks: locks.clone(),
                requests: requests.clone(),
            },
        )?
        .build()
        .await?;

    let guard = redqueend::sleep::SleepGuard::connect(&h.server).await?;
    assert_eq!(
        *requests.lock().map_err(|_| "poisoned")?,
        [("sleep".to_owned(), "delay".to_owned())],
        "a *delay* inhibitor for *sleep* was requested"
    );
    let task = tokio::spawn(guard.run(h.shared.clone(), h.server.clone()));

    h.shared.set_fan_custom("cpu", 50)?;
    assert_eq!(h.fan(1)?.1, "1");

    // Before sleep is announced the lock is held: reading blocks (no EOF).
    {
        let probe = locks.lock().map_err(|_| "poisoned")?[0].try_clone()?;
        probe.set_read_timeout(Some(Duration::from_millis(300)))?;
        let mut probe = probe;
        let mut byte = [0u8; 1];
        let held = std::io::Read::read(&mut probe, &mut byte);
        assert!(
            matches!(held, Err(ref e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
            "the lock should be held before sleep is announced, got {held:?}"
        );
    }

    let emitter = zbus::object_server::SignalEmitter::new(&logind, "/org/freedesktop/login1")?;
    FakeLogind::prepare_for_sleep(&emitter, true).await?;

    // The lock must stay held until the fans are back under firmware control...
    let restored = wait_until(5, || h.fan(1).is_ok_and(|(_, enable)| enable == "2")).await;
    assert!(restored, "fans must be automatic once sleep is announced");
    // ...and then be released so the suspend can proceed.
    let first_lock = locks.lock().map_err(|_| "poisoned")?.remove(0);
    let released = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        first_lock
            .set_read_timeout(Some(Duration::from_secs(5)))
            .ok();
        let mut byte = [0u8; 1];
        let mut first_lock = first_lock;
        matches!(first_lock.read(&mut byte), Ok(0))
    })
    .await?;
    assert!(
        released,
        "the delay lock must be released after the fans are restored"
    );

    // On resume the daemon rescans and takes the lock again.
    let scans = h.shared.status().discoveries;
    FakeLogind::prepare_for_sleep(&emitter, false).await?;
    assert!(
        wait_until(5, || requests.lock().is_ok_and(|r| r.len() == 2)).await,
        "the lock must be retaken after resume"
    );
    assert!(
        h.shared.status().discoveries > scans,
        "hardware was rediscovered after resume"
    );
    task.abort();
    Ok(())
}
