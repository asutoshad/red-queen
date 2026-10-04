//! Discovery and capability evaluation against the ANV15-51 fixtures.

use rq_core::{Backend, FanRole, Feature, Maturity, Reason, RoleSource, ThermalProfileId};
use rq_hardware::capabilities::{evaluate, identify_fans};
use rq_hardware::{ProbeContext, ProbeReport, SystemRoot, SystemSnapshot};
use rq_testkit::presets::{self, PLANTED_SECRETS};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn cap(snap: &SystemSnapshot, f: Feature) -> rq_core::CapabilityStatus {
    evaluate(snap)
        .into_iter()
        .find(|c| c.feature == f)
        .unwrap_or_else(|| panic!("capability {f:?} missing"))
}

fn predator_reason() -> Reason {
    Reason::DriverOptionRequired {
        module: "acer_wmi".into(),
        option: "predator_v4=1".into(),
    }
}

#[test]
fn identity_and_environment() -> TestResult {
    let fs = presets::anv15_51(true)?;
    let snap = SystemSnapshot::discover(&SystemRoot::at(fs.path()));
    assert_eq!(
        snap.identity.product_name.as_deref(),
        Some("Nitro ANV15-51")
    );
    assert_eq!(snap.identity.bios_version.as_deref(), Some("V1.60"));
    assert_eq!(snap.kernel.release.as_deref(), Some("7.1.5+kali-amd64"));
    assert_eq!(snap.os.id.as_deref(), Some("kali"));
    assert!(snap.acer.module_loaded && snap.acer.gaming_interface);
    assert!(snap.acer.predator_v4_enabled());
    assert!(snap.acer.hotkeys_input);
    Ok(())
}

#[test]
fn stock_kernel_explains_missing_features() -> TestResult {
    let fs = presets::anv15_51(false)?;
    let snap = SystemSnapshot::discover(&SystemRoot::at(fs.path()));
    assert!(!snap.platform_profile.available());
    assert!(snap.acer_hwmon().is_none());
    for f in [
        Feature::ThermalProfiles,
        Feature::FanTelemetry,
        Feature::CpuFan,
        Feature::FanControl,
    ] {
        let c = cap(&snap, f);
        assert!(!c.supported, "{f:?}");
        assert_eq!(c.reason, Some(predator_reason()), "{f:?}");
    }
    Ok(())
}

#[test]
fn predator_v4_exposes_profiles_and_fans() -> TestResult {
    let fs = presets::anv15_51(true)?;
    let snap = SystemSnapshot::discover(&SystemRoot::at(fs.path()));

    let legacy = snap
        .platform_profile
        .legacy
        .as_ref()
        .ok_or("no legacy profile")?;
    assert_eq!(legacy.active, Some(ThermalProfileId::Balanced));
    assert_eq!(legacy.choices.len(), 5);
    assert_eq!(
        snap.platform_profile.handlers[0].name.as_deref(),
        Some("acer-wmi")
    );

    let tp = cap(&snap, Feature::ThermalProfiles);
    assert!(tp.supported && tp.writable && tp.requires_privilege);
    assert_eq!(
        tp.maturity,
        Maturity::Detected,
        "never 'supported' before hardware tests"
    );
    assert_eq!(tp.backend, Some(Backend::PlatformProfile));

    let acer = snap.acer_hwmon().ok_or("no acer hwmon")?;
    assert_eq!(acer.fans.len(), 2);
    assert_eq!(acer.fans[0].input, Some(rq_core::Rpm(2331)));
    assert!(acer.pwms.is_empty());

    let fans = identify_fans(&snap);
    assert_eq!(fans[0].role, FanRole::Cpu);
    assert_eq!(fans[1].role, FanRole::Gpu);
    assert!(
        fans.iter()
            .all(|f| f.role_source == RoleSource::DriverChannelOrder)
    );

    assert!(cap(&snap, Feature::CpuFan).supported);
    assert!(cap(&snap, Feature::GpuFan).supported);
    Ok(())
}

#[test]
fn fan_control_is_not_faked() -> TestResult {
    let fs = presets::anv15_51(true)?;
    let snap = SystemSnapshot::discover(&SystemRoot::at(fs.path()));
    let c = cap(&snap, Feature::FanControl);
    assert!(!c.supported && !c.writable);
    assert_eq!(c.reason, Some(Reason::PendingKernelSupport));
    Ok(())
}

#[test]
fn pwm_files_enable_fan_control() -> TestResult {
    let fs = presets::anv15_51(true)?;
    let h = "/sys/devices/platform/acer-wmi/hwmon/hwmon7";
    fs.file_mode(&format!("{h}/pwm1"), "128", 0o644)?
        .file_mode(&format!("{h}/pwm1_enable"), "2", 0o644)?;
    let snap = SystemSnapshot::discover(&SystemRoot::at(fs.path()));
    let c = cap(&snap, Feature::FanControl);
    assert!(c.supported && c.writable);
    assert_eq!(c.backend, Some(Backend::AcerWmiHwmon));
    Ok(())
}

#[test]
fn battery_and_ac() -> TestResult {
    let fs = presets::anv15_51(true)?;
    let snap = SystemSnapshot::discover(&SystemRoot::at(fs.path()));
    let names: Vec<&str> = snap
        .power_supplies
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, ["ACAD", "BAT1"], "peripheral batteries are skipped");
    let bat = snap.power_supplies[1]
        .battery
        .as_ref()
        .ok_or("no battery")?;
    assert_eq!(bat.capacity_percent, Some(87));
    assert_eq!(bat.health_percent, Some(95));
    assert_eq!(bat.cycle_count, Some(42));
    assert!(!bat.charge_control.available());

    let limit = cap(&snap, Feature::BatteryChargeLimit);
    assert!(!limit.supported);
    assert_eq!(limit.reason, Some(Reason::CompanionModuleRequired));
    Ok(())
}

#[test]
fn gpus_and_no_wake() -> TestResult {
    let fs = presets::anv15_51(true)?;
    // A hwmon device belonging to the sleeping dGPU must not be read.
    let dgpu = "/sys/devices/pci0000:00/0000:00:01.0/0000:01:00.0";
    let h = format!("{dgpu}/hwmon/hwmon9");
    fs.file(&format!("{h}/name"), "gpu-sensor")?
        .file(&format!("{h}/temp1_input"), "55000")?
        .symlink(&format!("{h}/device"), dgpu)?
        .symlink("/sys/class/hwmon/hwmon9", &h)?;

    let snap = SystemSnapshot::discover(&SystemRoot::at(fs.path()));
    assert_eq!(snap.gpus.len(), 2, "non-display PCI devices are ignored");
    let nv = snap
        .gpus
        .iter()
        .find(|g| g.vendor_id == 0x10de)
        .ok_or("no nvidia")?;
    assert_eq!(nv.driver.as_deref(), Some("nvidia"));
    assert!(nv.is_asleep());
    assert_eq!(snap.nvidia.driver_version.as_deref(), Some("550.163.01"));

    let chip = snap
        .hwmon
        .iter()
        .find(|c| c.name.as_deref() == Some("gpu-sensor"))
        .ok_or("chip")?;
    assert!(chip.skipped_asleep);
    assert_eq!(chip.temps[0].input, None);

    let nvml = cap(&snap, Feature::NvidiaTelemetry);
    assert!(nvml.supported);
    assert_eq!(nvml.backend, Some(Backend::Nvml));
    Ok(())
}

#[test]
fn nouveau_needs_proprietary_driver() -> TestResult {
    let fs = presets::anv15_51(true)?;
    let dgpu = "/sys/devices/pci0000:00/0000:00:01.0/0000:01:00.0";
    std::fs::remove_file(fs.path().join(&dgpu[1..]).join("driver"))?;
    fs.dir("/sys/bus/pci/drivers/nouveau")?
        .symlink(&format!("{dgpu}/driver"), "/sys/bus/pci/drivers/nouveau")?;
    let snap = SystemSnapshot::discover(&SystemRoot::at(fs.path()));
    let c = cap(&snap, Feature::NvidiaTelemetry);
    assert_eq!(
        c.reason,
        Some(Reason::ProprietaryDriverRequired {
            current_driver: Some("nouveau".into())
        })
    );
    Ok(())
}

#[test]
fn unsafe_features_are_unsupported_by_design() -> TestResult {
    let fs = presets::anv15_51(true)?;
    let snap = SystemSnapshot::discover(&SystemRoot::at(fs.path()));
    for f in [Feature::BootLogo, Feature::AudioEnhancement] {
        assert_eq!(cap(&snap, f).reason, Some(Reason::UnsupportedByDesign));
    }
    assert!(!cap(&snap, Feature::RgbKeyboard).supported);
    assert!(!cap(&snap, Feature::KeyboardBacklight).supported);
    Ok(())
}

#[test]
fn every_feature_is_evaluated_once() -> TestResult {
    let fs = presets::anv15_51(true)?;
    let caps = evaluate(&SystemSnapshot::discover(&SystemRoot::at(fs.path())));
    let mut features: Vec<Feature> = caps.iter().map(|c| c.feature).collect();
    let n = features.len();
    features.sort();
    features.dedup();
    assert_eq!(features.len(), n, "duplicate capability");
    assert_eq!(n, 21);
    Ok(())
}

#[test]
fn probe_report_never_leaks_secrets() -> TestResult {
    for predator in [false, true] {
        let fs = presets::anv15_51(predator)?;
        let root = SystemRoot::at(fs.path());
        let ctx = ProbeContext {
            tool_version: "test".into(),
            desktop: Some("XFCE".into()),
            session_type: Some("x11".into()),
            sensitive: vec!["testuser".into()],
        };
        let json = ProbeReport::collect(&root, &ctx)
            .to_redacted_json(&root, &ctx)
            .to_string();
        for secret in PLANTED_SECRETS {
            assert!(!json.contains(secret), "leaked {secret}");
        }
        assert!(!json.contains("aa:bb:cc:dd:ee:ff"), "leaked a MAC address");
        assert!(!json.contains("Headphones"), "leaked an input device name");
        assert!(!json.contains("serial"), "serial field present");
        assert!(json.contains("Nitro ANV15-51"));
    }
    Ok(())
}

#[test]
fn empty_system_does_not_panic() -> TestResult {
    let fs = rq_testkit::FakeSystem::new()?;
    let root = SystemRoot::at(fs.path());
    let report = ProbeReport::collect(&root, &ProbeContext::default());
    assert!(report.capabilities.iter().all(|c| !c.supported));
    assert!(!report.summary.acer_wmi);
    Ok(())
}

#[test]
fn telemetry_sample() -> TestResult {
    use rq_core::{BatteryState, MilliCelsius, Rpm};
    let fs = presets::anv15_51(true)?;
    let root = SystemRoot::at(fs.path());
    let snap = SystemSnapshot::discover(&root);
    let mut sampler = rq_hardware::Sampler::new(root, &snap);

    let first = sampler.sample(1_000);
    assert_eq!(
        first.cpu.usage_percent, None,
        "no rate before the second sample"
    );
    assert_eq!(
        first.cpu.temperature,
        Some(MilliCelsius(48_000)),
        "coretemp package sensor"
    );
    assert_eq!(first.cpu.avg_freq_mhz, Some(3000));
    assert_eq!(first.cpu.max_freq_mhz, Some(3600));
    assert_eq!(first.memory.used_percent(), Some(50.0));
    assert_eq!(first.uptime_s, Some(12345));
    assert_eq!(first.ac_online, Some(false));
    assert_eq!(first.thermal_profile, Some(ThermalProfileId::Balanced));
    let bat = first.battery.as_ref().ok_or("battery")?;
    assert_eq!(bat.percent, Some(87));
    assert_eq!(bat.state, Some(BatteryState::Discharging));
    assert_eq!(bat.power_mw, Some(18_960), "1.2 A x 15.8 V");
    assert_eq!(first.fans.len(), 2);
    assert_eq!(first.fans[0].id, "acer/1");
    assert_eq!(first.fans[0].role, FanRole::Cpu);
    assert_eq!(first.fans[0].rpm, Some(Rpm(2331)));

    let gpu = first.gpu.as_ref().ok_or("gpu")?;
    assert!(gpu.asleep);
    assert_eq!(gpu.temperature, None, "sleeping GPU is not read");

    // Advance counters: 2000 more jiffies, 1000 of them busy.
    fs.file(
        "/proc/stat",
        "cpu  1800 0 700 9000 100 0 0 0 0 0\ncpu0 900 0 350 4500 50 0 0 0 0 0\ncpu1 900 0 350 4500 50 0 0 0 0 0\nintr 0",
    )?;
    let second = sampler.sample(2_000);
    assert_eq!(second.cpu.usage_percent, Some(50.0));
    assert_eq!(second.cpu.per_core_percent, vec![50.0, 50.0]);
    Ok(())
}

#[test]
fn awake_gpu_zero_reading_is_none() -> TestResult {
    let fs = presets::anv15_51(true)?;
    fs.file(
        "/sys/devices/pci0000:00/0000:00:01.0/0000:01:00.0/power/runtime_status",
        "active",
    )?;
    let root = SystemRoot::at(fs.path());
    let snap = SystemSnapshot::discover(&root);
    let mut sampler = rq_hardware::Sampler::new(root, &snap);
    let gpu = sampler.sample(0).gpu.ok_or("gpu")?;
    assert!(!gpu.asleep);
    assert_eq!(
        gpu.temperature, None,
        "acer temp2 = 0 means no reading, not 0 C"
    );
    fs.file(
        "/sys/devices/platform/acer-wmi/hwmon/hwmon7/temp2_input",
        "44000",
    )?;
    assert_eq!(
        sampler.sample(1).gpu.and_then(|g| g.temperature),
        Some(rq_core::MilliCelsius(44_000))
    );
    Ok(())
}
