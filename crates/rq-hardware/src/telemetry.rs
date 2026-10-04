//! Periodic telemetry sampling.
//!
//! A [`Sampler`] is built from a [`SystemSnapshot`] and only reads the
//! files discovery found. When hardware changes (hot-plug, driver reload,
//! resume) the owner rebuilds it from a fresh snapshot instead of keeping
//! stale paths.

use std::time::{Duration, Instant};

use rq_core::{
    BatteryState, BatterySummary, CpuStatus, FanReading, GpuStatus, MemoryStatus, MilliCelsius,
    Rpm, TelemetrySample, ThermalProfileId,
};

use crate::capabilities::identify_fans;
use crate::gpu::GpuInfo;
use crate::power_supply::SupplyKind;
use crate::root::SystemRoot;
use crate::snapshot::SystemSnapshot;

const HWMON: &str = "/sys/class/hwmon";
const PSU: &str = "/sys/class/power_supply";
const RAPL: &str = "/sys/class/powercap/intel-rapl:0";

/// Battery, AC and the thermal profile change slowly, and reading them makes
/// the kernel call into firmware (the profile read alone costs about 5 ms of
/// CPU on the ANV15-51), so they are refreshed less often than the rest.
const SLOW_REFRESH: Duration = Duration::from_secs(5);

/// Values refreshed on the slow schedule.
#[derive(Debug, Clone, Default)]
struct SlowValues {
    battery: Option<BatterySummary>,
    ac_online: Option<bool>,
    thermal_profile: Option<ThermalProfileId>,
}

/// Where each value comes from, resolved once per discovery.
#[derive(Debug, Clone, Default)]
struct Sources {
    cpu_temp: Option<String>,
    gpu_temp: Option<String>,
    dgpu_runtime_status: Option<String>,
    fans: Vec<(String, rq_core::FanRole, String)>,
    battery_dir: Option<String>,
    ac_dir: Option<String>,
    cpu_freq: Vec<String>,
    rapl: bool,
    profile: Option<String>,
}

/// Busy and total jiffies for one CPU line of `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

/// Samples telemetry. Keeps the previous counters needed for rates.
#[derive(Debug)]
pub struct Sampler {
    root: SystemRoot,
    src: Sources,
    prev_cpu: Vec<CpuTimes>,
    prev_rapl: Option<(u64, Instant)>,
    slow: Option<(Instant, SlowValues)>,
    slow_refresh: Duration,
}

impl Sampler {
    /// Builds a sampler for what `snap` discovered.
    pub fn new(root: SystemRoot, snap: &SystemSnapshot) -> Self {
        let src = resolve_sources(&root, snap);
        Self {
            root,
            src,
            prev_cpu: Vec::new(),
            prev_rapl: None,
            slow: None,
            slow_refresh: SLOW_REFRESH,
        }
    }

    /// Forgets cached slow values so the next sample re-reads them, for
    /// example right after a hardware write.
    pub fn invalidate_slow(&mut self) {
        self.slow = None;
    }

    /// Changes how often battery, AC and the thermal profile are re-read.
    #[must_use]
    pub fn with_slow_refresh(mut self, every: Duration) -> Self {
        self.slow_refresh = every;
        self
    }

    fn slow_values(&mut self) -> SlowValues {
        if let Some((at, values)) = &self.slow
            && at.elapsed() < self.slow_refresh
        {
            return values.clone();
        }
        let values = SlowValues {
            battery: self.battery(),
            ac_online: self
                .src
                .ac_dir
                .as_ref()
                .and_then(|d| self.root.read_parse::<u8>(format!("{d}/online")))
                .map(|v| v != 0),
            thermal_profile: self
                .src
                .profile
                .as_ref()
                .and_then(|p| self.root.read_string(p))
                .map(|s| ThermalProfileId::from_kernel_name(&s)),
        };
        self.slow = Some((Instant::now(), values.clone()));
        values
    }

    /// Takes one sample. `now_ms` is Unix time; rates use the monotonic
    /// clock internally.
    pub fn sample(&mut self, now_ms: u64) -> TelemetrySample {
        let slow = self.slow_values();
        TelemetrySample {
            timestamp_ms: now_ms,
            cpu: self.cpu(),
            gpu: self.gpu(),
            memory: self.memory(),
            fans: self.fans(),
            battery: slow.battery,
            ac_online: slow.ac_online,
            thermal_profile: slow.thermal_profile,
            uptime_s: self
                .root
                .read_string("/proc/uptime")
                .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
                .map(|v| v as u64),
        }
    }

    fn cpu(&mut self) -> CpuStatus {
        let now = self
            .root
            .read_string("/proc/stat")
            .map(|s| parse_proc_stat(&s))
            .unwrap_or_default();
        let usage: Vec<Option<f32>> = if self.prev_cpu.len() == now.len() {
            self.prev_cpu
                .iter()
                .zip(&now)
                .map(|(a, b)| usage(*a, *b))
                .collect()
        } else {
            Vec::new()
        };
        self.prev_cpu = now;

        let freqs: Vec<u32> = self
            .src
            .cpu_freq
            .iter()
            .filter_map(|p| self.root.read_parse::<u32>(p))
            .map(|khz| khz / 1000)
            .collect();
        let avg_freq_mhz = (!freqs.is_empty()).then(|| {
            (freqs.iter().map(|f| u64::from(*f)).sum::<u64>() / freqs.len() as u64) as u32
        });

        CpuStatus {
            usage_percent: usage.first().copied().flatten(),
            per_core_percent: usage.iter().skip(1).map(|u| u.unwrap_or(0.0)).collect(),
            avg_freq_mhz,
            max_freq_mhz: freqs.iter().copied().max(),
            temperature: self.read_temp(self.src.cpu_temp.as_deref()),
            package_power_mw: self.rapl_power(),
        }
    }

    fn rapl_power(&mut self) -> Option<u32> {
        if !self.src.rapl {
            return None;
        }
        let energy: u64 = self.root.read_parse(format!("{RAPL}/energy_uj"))?;
        let now = Instant::now();
        let prev = self.prev_rapl.replace((energy, now));
        let (old, then) = prev?;
        let max: u64 = self
            .root
            .read_parse(format!("{RAPL}/max_energy_range_uj"))
            .unwrap_or(0);
        let delta = if energy >= old {
            energy - old
        } else if max > old {
            max - old + energy
        } else {
            return None;
        };
        let ms = now.duration_since(then).as_millis() as u64;
        (ms > 0).then(|| u32::try_from(delta / ms).unwrap_or(u32::MAX))
    }

    fn gpu(&self) -> Option<GpuStatus> {
        let status = self.src.dgpu_runtime_status.as_ref()?;
        let asleep = matches!(
            self.root.read_string(status).as_deref(),
            Some("suspended" | "suspending")
        );
        let temperature = if asleep {
            None
        } else {
            self.read_temp(self.src.gpu_temp.as_deref())
        };
        Some(GpuStatus {
            asleep,
            temperature,
        })
    }

    fn memory(&self) -> MemoryStatus {
        self.root
            .read_string("/proc/meminfo")
            .map(|s| parse_meminfo(&s))
            .unwrap_or_default()
    }

    fn fans(&self) -> Vec<FanReading> {
        self.src
            .fans
            .iter()
            .map(|(id, role, path)| FanReading {
                id: id.clone(),
                role: *role,
                rpm: self.root.read_parse::<u32>(path).map(Rpm),
            })
            .collect()
    }

    fn battery(&self) -> Option<BatterySummary> {
        let d = self.src.battery_dir.as_ref()?;
        let n = |f: &str| self.root.read_parse::<i64>(format!("{d}/{f}"));
        let power_uw = n("power_now").or_else(|| {
            let (i, v) = (n("current_now")?, n("voltage_now")?);
            Some(i.checked_mul(v)? / 1_000_000)
        });
        Some(BatterySummary {
            percent: self
                .root
                .read_parse::<u8>(format!("{d}/capacity"))
                .filter(|p| *p <= 100),
            state: self
                .root
                .read_string(format!("{d}/status"))
                .map(|s| BatteryState::from_kernel(&s)),
            power_mw: power_uw.and_then(|p| u32::try_from(p.unsigned_abs() / 1000).ok()),
        })
    }

    /// Reads a millidegree file. Zero means "no reading" (for example the
    /// acer GPU sensor while the GPU is off), never 0 °C.
    fn read_temp(&self, path: Option<&str>) -> Option<MilliCelsius> {
        self.root
            .read_parse::<i32>(path?)
            .filter(|v| *v != 0)
            .map(MilliCelsius)
    }
}

fn resolve_sources(root: &SystemRoot, snap: &SystemSnapshot) -> Sources {
    let chip_file = |sysfs: &str, file: String| format!("{HWMON}/{sysfs}/{file}");

    let coretemp = snap
        .hwmon
        .iter()
        .find(|c| c.name.as_deref() == Some("coretemp"));
    let acer = snap.acer_hwmon();
    let cpu_temp = coretemp
        .and_then(|c| {
            c.temps
                .iter()
                .find(|t| t.label.as_deref().is_some_and(|l| l.starts_with("Package")))
                .or_else(|| c.temps.first())
                .map(|t| chip_file(&c.sysfs_name, format!("temp{}_input", t.index)))
        })
        .or_else(|| {
            // acer-wmi channel 1 is the CPU sensor.
            acer.filter(|c| c.temps.iter().any(|t| t.index == 1))
                .map(|c| chip_file(&c.sysfs_name, "temp1_input".into()))
        });

    let dgpu: Option<&GpuInfo> = snap
        .gpus
        .iter()
        .find(|g| g.boot_vga == Some(false) && g.vendor != crate::gpu::GpuVendor::Intel);
    // acer-wmi channel 2 is the GPU sensor.
    let gpu_temp = acer
        .filter(|c| c.temps.iter().any(|t| t.index == 2))
        .map(|c| chip_file(&c.sysfs_name, "temp2_input".into()));

    let fans = identify_fans(snap)
        .into_iter()
        .filter_map(|f| {
            let chip = snap
                .hwmon
                .iter()
                .find(|c| c.name.as_deref() == Some(f.chip.as_str()))?;
            Some((
                format!("{}/{}", f.chip, f.index),
                f.role,
                chip_file(&chip.sysfs_name, format!("fan{}_input", f.index)),
            ))
        })
        .collect();

    let supply = |kind: SupplyKind| {
        snap.power_supplies
            .iter()
            .find(|p| p.kind == kind)
            .map(|p| format!("{PSU}/{}", p.name))
    };

    let cpu_freq = root
        .list_dir("/sys/devices/system/cpu")
        .into_iter()
        .filter(|n| {
            n.strip_prefix("cpu")
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|n| format!("/sys/devices/system/cpu/{n}/cpufreq/scaling_cur_freq"))
        .filter(|p| root.exists(p))
        .collect();

    let profile = crate::profile::select_paths(&snap.platform_profile).map(|p| p.profile);

    Sources {
        cpu_temp,
        gpu_temp,
        dgpu_runtime_status: dgpu
            .map(|g| format!("/sys/bus/pci/devices/{}/power/runtime_status", g.address)),
        fans,
        battery_dir: supply(SupplyKind::Battery),
        ac_dir: supply(SupplyKind::Mains),
        cpu_freq,
        rapl: snap.rapl.present,
        profile,
    }
}

/// Parses the `cpu` lines of `/proc/stat`: aggregate first, then each CPU.
fn parse_proc_stat(text: &str) -> Vec<CpuTimes> {
    text.lines()
        .filter(|l| l.starts_with("cpu"))
        .filter_map(|l| {
            let v: Vec<u64> = l
                .split_whitespace()
                .skip(1)
                .filter_map(|x| x.parse().ok())
                .collect();
            if v.len() < 4 {
                return None;
            }
            // user nice system idle iowait irq softirq steal (guest is
            // already counted in user).
            let total: u64 = v.iter().take(8).sum();
            let idle = v[3] + v.get(4).copied().unwrap_or(0);
            Some(CpuTimes {
                busy: total.saturating_sub(idle),
                total,
            })
        })
        .collect()
}

fn usage(prev: CpuTimes, now: CpuTimes) -> Option<f32> {
    let total = now.total.checked_sub(prev.total)?;
    let busy = now.busy.checked_sub(prev.busy)?;
    (total > 0).then(|| (busy as f32 * 100.0 / total as f32).clamp(0.0, 100.0))
}

fn parse_meminfo(text: &str) -> MemoryStatus {
    let mut m = MemoryStatus::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(key), Some(val)) = (it.next(), it.next().and_then(|v| v.parse::<u64>().ok()))
        else {
            continue;
        };
        match key {
            "MemTotal:" => m.total_kib = val,
            "MemAvailable:" => m.available_kib = val,
            "SwapTotal:" => m.swap_total_kib = val,
            "SwapFree:" => m.swap_free_kib = val,
            _ => {}
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_usage() {
        let a =
            parse_proc_stat("cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 50 0 50 400 0 0 0 0 0 0\nintr 1");
        let b = parse_proc_stat(
            "cpu  200 0 200 1400 0 0 0 0 0 0\ncpu0 150 0 50 400 0 0 0 0 0 0\nintr 1",
        );
        assert_eq!(a.len(), 2);
        assert_eq!(usage(a[0], b[0]), Some(25.0));
        assert_eq!(usage(a[1], b[1]), Some(100.0));
        assert_eq!(usage(b[0], a[0]), None, "counter went backwards");
        assert_eq!(usage(a[0], a[0]), None, "no time passed");
    }

    #[test]
    fn meminfo() {
        let m = parse_meminfo(
            "MemTotal:  16000000 kB\nMemFree: 1 kB\nMemAvailable: 8000000 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        );
        assert_eq!(m.total_kib, 16_000_000);
        assert_eq!(m.available_kib, 8_000_000);
        assert_eq!(m.used_percent(), Some(50.0));
    }
}
