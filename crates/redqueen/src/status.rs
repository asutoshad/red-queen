//! `redqueen status` and `redqueen daemon status`: read-only views of what
//! the daemon reports. All data comes over D-Bus.

use std::io::{self, Write};
use std::time::Duration;

use anyhow::{Context, bail};
use rq_core::{FanRole, TelemetrySample, TemperatureUnit, ThermalProfileId};
use rq_ipc::{ChoiceState, Client, ClientError, ErrorKind, FansInfo};

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

fn write_fans(out: &mut impl Write, info: &FansInfo, unit: TemperatureUnit) -> io::Result<()> {
    if !info.available {
        writeln!(out, "This machine exposes no fan sensors.")?;
        writeln!(
            out,
            "On the Acer Nitro ANV15-51 the acer_wmi driver needs the option predator_v4=1; see `redqueen probe`."
        )?;
        return Ok(());
    }
    writeln!(out, "Fans")?;
    for f in &info.fans {
        let rpm = f.rpm.map_or("n/a".to_owned(), |r| format!("{} RPM", r.0));
        let mode = match f.mode {
            Some(rq_core::FanMode::Auto) => "auto".to_owned(),
            Some(rq_core::FanMode::Max) => "max".to_owned(),
            Some(rq_core::FanMode::Custom) => format!(
                "manual {}",
                f.duty_percent.map_or("?".into(), |d| format!("{d} %"))
            ),
            None => "unknown".to_owned(),
        };
        let note = if f.role_verified {
            ""
        } else {
            "   (which fan this is comes from the driver's channel order; unverified)"
        };
        writeln!(out, "  {:<6} {rpm:<10} {mode}{note}", f.id)?;
    }
    if !info.controllable {
        writeln!(
            out,
            "\nFan speeds can be read, but this machine can't control them yet:"
        )?;
        writeln!(
            out,
            "the kernel driver has no fan control for this model. See `redqueen probe`."
        )?;
        return Ok(());
    }
    let s = &info.safety;
    let t = |c: u32| {
        let m = rq_core::MilliCelsius(i32::try_from(c * 1000).unwrap_or(i32::MAX));
        match unit {
            TemperatureUnit::Celsius => format!("{} °C", unit.convert(m).round()),
            TemperatureUnit::Fahrenheit => format!("{} °F", unit.convert(m).round()),
        }
    };
    writeln!(
        out,
        "\nSafety: manual speed is never below {} %; control returns to automatic if the CPU reaches {} or the GPU {}.",
        s.min_percent,
        t(s.critical_cpu_celsius),
        t(s.critical_gpu_celsius)
    )?;
    if s.tripped {
        writeln!(
            out,
            "Manual control is locked out for {} more seconds because {}.",
            s.lockout_remaining_s,
            s.reason.as_deref().unwrap_or("of a safety trip")
        )?;
    }
    Ok(())
}

/// `redqueen fan status`.
pub async fn fan_status(
    bus: Bus,
    unit: TemperatureUnit,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let conn = connect(bus).await?;
    let info = Client::new(&conn)
        .await
        .map_err(explain)?
        .fans()
        .await
        .map_err(explain)?;
    write_fans(out, &info, unit)?;
    Ok(())
}

/// `redqueen fan auto` and `redqueen fan max`.
pub async fn fan_mode(
    bus: Bus,
    mode: &str,
    unit: TemperatureUnit,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let conn = connect(bus).await?;
    let info = Client::new(&conn)
        .await
        .map_err(explain)?
        .set_fan_mode(mode)
        .await
        .map_err(explain)?;
    writeln!(
        out,
        "Fans are now under {} control (confirmed by the hardware).\n",
        if mode == "max" {
            "full-speed"
        } else {
            "automatic"
        }
    )?;
    write_fans(out, &info, unit)?;
    if mode == "max" {
        writeln!(
            out,
            "\nReturn to automatic control any time with: redqueen fan auto"
        )?;
    }
    Ok(())
}

/// `redqueen fan cpu <percent>` and `redqueen fan gpu <percent>`.
pub async fn fan_speed(
    bus: Bus,
    fan: &str,
    percent: u32,
    unit: TemperatureUnit,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let conn = connect(bus).await?;
    let info = Client::new(&conn)
        .await
        .map_err(explain)?
        .set_fan_speed(fan, percent)
        .await
        .map_err(explain)?;
    writeln!(
        out,
        "The {fan} fan is now under manual control at {percent} % (confirmed by the hardware).\n"
    )?;
    write_fans(out, &info, unit)?;
    writeln!(
        out,
        "\nManual control is watched by the safety layer. Return to automatic control any time with: redqueen fan auto"
    )?;
    Ok(())
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
            (_, ChoiceState::Unsupported) => "  not supported by this hardware",
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
