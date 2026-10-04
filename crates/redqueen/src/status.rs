//! `redqueen status` and `redqueen daemon status`: read-only views of what
//! the daemon reports. All data comes over D-Bus.

use std::io::{self, Write};
use std::time::Duration;

use anyhow::{Context, bail};
use rq_core::{FanRole, TelemetrySample, TemperatureUnit, ThermalProfileId};
use rq_ipc::{ChoiceState, Client, ClientError, ErrorKind};

use crate::Bus;

/// Connects to the daemon, with an actionable error if it isn't there.
async fn connect(bus: Bus) -> anyhow::Result<zbus::Connection> {
    let conn = match bus {
        Bus::System => zbus::Connection::system().await,
        Bus::Session => zbus::Connection::session().await,
    }
    .context("cannot connect to the D-Bus message bus")?;
    Ok(conn)
}

fn explain(e: ClientError) -> anyhow::Error {
    let text = e.to_string();
    if let Some(kind) = e.kind() {
        return match kind {
            ErrorKind::NotAuthorized => anyhow::anyhow!("not authorized: {text}"),
            _ => anyhow::anyhow!("{text}"),
        };
    }
    if text.contains("ServiceUnknown") || text.contains("NameHasNoOwner") {
        anyhow::anyhow!("the Red Queen daemon is not running (check: systemctl status redqueend)")
    } else {
        anyhow::Error::new(e)
    }
}

/// `redqueen profile list`.
pub async fn profile_list(bus: Bus, out: &mut impl Write) -> anyhow::Result<()> {
    let conn = connect(bus).await?;
    let info = Client::new(&conn)
        .await
        .map_err(explain)?
        .thermal_profiles()
        .await
        .map_err(explain)?;
    if !info.available {
        writeln!(out, "This machine has no controllable thermal profile.")?;
        writeln!(
            out,
            "On the Acer Nitro ANV15-51 the acer_wmi driver needs the option predator_v4=1; see `redqueen probe`."
        )?;
        return Ok(());
    }
    writeln!(out, "Thermal profiles")?;
    for c in &info.choices {
        let mark = if info.active.as_ref() == Some(&c.id) {
            "*"
        } else {
            " "
        };
        let note = match (info.active.as_ref() == Some(&c.id), c.state) {
            (_, ChoiceState::Unsupported) => "  not supported by the firmware (rejected earlier)",
            (true, _) => "  active",
            _ => "",
        };
        writeln!(out, "  {mark} {}{note}", c.id.kernel_name())?;
    }
    writeln!(
        out,
        "\nNot every advertised profile is accepted by the firmware."
    )?;
    Ok(())
}

/// `redqueen profile set <name>`.
pub async fn profile_set(bus: Bus, name: &str, out: &mut impl Write) -> anyhow::Result<()> {
    let profile =
        ThermalProfileId::parse_untrusted(name).map_err(|e| anyhow::anyhow!("'{name}': {e}"))?;
    let conn = connect(bus).await?;
    let client = Client::new(&conn).await.map_err(explain)?;
    let result = client
        .set_thermal_profile(&profile)
        .await
        .map_err(explain)?;
    writeln!(
        out,
        "Thermal profile is now '{}' (confirmed by the hardware).",
        result.active.kernel_name()
    )?;
    Ok(())
}

/// `redqueen daemon status`.
pub async fn daemon_status(bus: Bus, out: &mut impl Write) -> anyhow::Result<()> {
    let conn = connect(bus).await?;
    let client = Client::new(&conn).await.map_err(explain)?;
    let st = client.status().await.map_err(explain)?;
    writeln!(out, "redqueend {} is running", st.version)?;
    writeln!(out, "  uptime              {}", duration(st.uptime_s))?;
    writeln!(out, "  sampling interval   {} ms", st.sample_interval_ms)?;
    writeln!(
        out,
        "  history             {} / {} samples",
        st.history_len, st.history_capacity
    )?;
    writeln!(out, "  live subscribers    {}", st.subscribers)?;
    writeln!(out, "  hardware scans      {}", st.discoveries)?;
    Ok(())
}

/// `redqueen status`.
pub async fn status(bus: Bus, unit: TemperatureUnit, out: &mut impl Write) -> anyhow::Result<()> {
    let conn = connect(bus).await?;
    let client = Client::new(&conn).await.map_err(explain)?;
    let id = client.hardware_identity().await.map_err(explain)?;

    let mut sample = None;
    for _ in 0..10 {
        sample = client.telemetry().await.map_err(explain)?;
        if sample
            .as_ref()
            .is_some_and(|s| s.cpu.usage_percent.is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let Some(s) = sample else {
        bail!("the daemon has not produced a sample yet; try again")
    };

    writeln!(
        out,
        "{} {}  (BIOS {})\n",
        id.vendor.as_deref().unwrap_or(""),
        id.product_name.as_deref().unwrap_or("unknown model"),
        id.bios_version.as_deref().unwrap_or("?")
    )?;
    write_sample(out, &s, unit)?;
    Ok(())
}

fn write_sample(
    out: &mut impl Write,
    s: &TelemetrySample,
    unit: TemperatureUnit,
) -> io::Result<()> {
    let temp = |t: Option<rq_core::MilliCelsius>| match (t, unit) {
        (Some(t), TemperatureUnit::Celsius) => format!("{:.0} °C", unit.convert(t)),
        (Some(t), TemperatureUnit::Fahrenheit) => format!("{:.0} °F", unit.convert(t)),
        (None, _) => "n/a".to_owned(),
    };
    let row = |out: &mut dyn Write, k: &str, v: String| writeln!(out, "  {k:<16} {v}");

    row(
        out,
        "Thermal profile",
        s.thermal_profile
            .as_ref()
            .map_or("n/a".into(), |p| p.kernel_name().to_owned()),
    )?;

    let c = &s.cpu;
    let mut cpu = vec![temp(c.temperature)];
    cpu.extend(c.usage_percent.map(|u| format!("{u:.0} %")));
    cpu.extend(
        c.avg_freq_mhz
            .map(|f| format!("{:.2} GHz", f64::from(f) / 1000.0)),
    );
    cpu.extend(
        c.package_power_mw
            .map(|p| format!("{:.1} W", f64::from(p) / 1000.0)),
    );
    row(out, "CPU", cpu.join("   "))?;

    if let Some(g) = &s.gpu {
        row(
            out,
            "GPU",
            if g.asleep {
                "asleep (not queried, to save power)".into()
            } else {
                temp(g.temperature)
            },
        )?;
    }

    if s.fans.is_empty() {
        row(out, "Fans", "no fan sensors available".into())?;
    } else {
        let fans: Vec<String> = s
            .fans
            .iter()
            .map(|f| {
                let role = match f.role {
                    FanRole::Cpu => "CPU",
                    FanRole::Gpu => "GPU",
                    FanRole::Unknown => f.id.as_str(),
                };
                format!(
                    "{role} {}",
                    f.rpm.map_or("n/a".into(), |r| format!("{} RPM", r.0))
                )
            })
            .collect();
        row(out, "Fans", fans.join("   "))?;
    }

    let m = &s.memory;
    if let Some(p) = m.used_percent() {
        row(
            out,
            "Memory",
            format!(
                "{:.1} / {:.1} GiB ({p:.0} %)",
                gib(m.used_kib()),
                gib(m.total_kib)
            ),
        )?;
    }

    if let Some(b) = &s.battery {
        let mut parts = Vec::new();
        parts.extend(b.percent.map(|p| format!("{p} %")));
        parts.extend(b.state.as_ref().map(|st| format!("{st:?}").to_lowercase()));
        parts.extend(
            b.power_mw
                .map(|p| format!("{:.1} W", f64::from(p) / 1000.0)),
        );
        row(out, "Battery", parts.join("   "))?;
    }
    if let Some(ac) = s.ac_online {
        row(
            out,
            "AC adapter",
            if ac { "connected" } else { "unplugged" }.into(),
        )?;
    }
    if let Some(up) = s.uptime_s {
        row(out, "Uptime", duration(up))?;
    }
    Ok(())
}

fn gib(kib: u64) -> f64 {
    kib as f64 / 1024.0 / 1024.0
}

fn duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m} min"),
        (0, _) => format!("{h} h {m} min"),
        _ => format!("{d} d {h} h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(90), "1 min");
        assert_eq!(duration(3700), "1 h 1 min");
        assert_eq!(duration(90_000), "1 d 1 h");
    }
}
