# Security Model

The Red Queen controls cooling and power hardware, so it is designed to give
as little power as possible to as little code as possible.

## Trust boundaries

```
 untrusted                          trusted
 ─────────────────────────────┬──────────────────────────────
 redqueen (GUI / CLI)         │  redqueend (system daemon)
 redqueen-agent               │    • fixed method allow-list
 any other local program      │    • validates every argument
 imported profile files       │    • polkit check per action
                              │    • writes only known sysfs files
```

Everything on the left is treated as untrusted input, including the
project's own application.

## Principles

- **The application never runs as root.** Only `redqueend` is privileged.
- **No shell.** Nothing runs `sudo`, `tee`, `echo`, `modprobe` or similar to
  change hardware. The daemon uses kernel interfaces directly, and the
  application uses D-Bus.
- **No generic access.** There is no "write file" or "run command" method.
  Every D-Bus method maps to one hardware operation.
- **Validate everything.** Fan percentages must be integers within the
  backend's allowed range; profile and device IDs must match what discovery
  found. Anything else is rejected with an error.
- **Paths come from discovery, never from clients.** Clients refer to
  devices by ID; the daemon maps IDs to sysfs paths it discovered itself.
- **Read back every write** and report mismatches (see `docs/architecture.md`).

## D-Bus interface

Bus name `io.github.asutoshad.RedQueen.Daemon` on the system bus.

| Method | Authorization |
|---|---|
| `GetCapabilities`, `GetStatus`, `GetTelemetry`, `GetHistory`, `GetHardwareIdentity` | none (read-only, no personal data) |
| `SetThermalProfile` | `io.github.asutoshad.RedQueen.set-profile` |
| `SetFanMode` (Auto) | none: returning to firmware control is always allowed |
| `SetFanMode` (Max/Custom), `SetFanSpeed`, `SetFanCurve` | `io.github.asutoshad.RedQueen.control-fans` |
| `SetBatteryChargeLimit`, `StartBatteryCalibration`, `SetUsbCharging` | `io.github.asutoshad.RedQueen.battery` |
| `SetLcdOverdrive`, `SetBootSound`, `SetKeyboardBacklightTimeout` | `io.github.asutoshad.RedQueen.firmware-settings` |
| `EnableAcerGamingInterface` (writes the fixed `/etc/modprobe.d/red-queen.conf`) | `io.github.asutoshad.RedQueen.manage-driver` |

The D-Bus bus policy lets anyone call the daemon but only root own its name,
so no other program can impersonate it.

### polkit defaults (for an active local session)
| Action | Default |
|---|---|
| `set-profile` | allowed (`yes`) |
| `control-fans` | administrator password, remembered for the session (`auth_admin_keep`) |
| `battery`, `firmware-settings` | `auth_admin_keep` |
| `manage-driver` | administrator password every time (`auth_admin`) |
| any action from inactive or remote sessions | `auth_admin` or denied |

Administrators can override these with polkit rules in `/etc/polkit-1/rules.d/`.

### Abuse resistance
- Writes are rate-limited and coalesced per client, so a misbehaving client
  can't flood the embedded controller.
- Payload sizes (for example fan-curve point counts) are bounded.
- Telemetry signals are sent only to subscribed clients.

## systemd hardening

`redqueend` runs as root but with **no Linux capabilities**: sysfs hardware
files are owned by root, so ordinary file permissions suffice and
capabilities such as `CAP_SYS_MODULE` or `CAP_SYS_ADMIN` aren't needed.

| Setting | Value | Why |
|---|---|---|
| `CapabilityBoundingSet=` | empty | no privileged kernel operations |
| `NoNewPrivileges=` | yes | |
| `ProtectSystem=` | strict | whole filesystem read-only… |
| `ReadWritePaths=` | `/var/lib/red-queen /etc/modprobe.d` | …except its own state and the single driver-option file |
| `ProtectKernelTunables=` | **no** | must write `platform_profile` and hwmon files in `/sys` |
| `ProtectKernelModules=` | yes | never loads or unloads modules |
| `ProtectHome=`, `PrivateTmp=` | yes | no access to user files |
| `DevicePolicy=` | closed | only NVIDIA control devices allowed, for NVML |
| `RestrictAddressFamilies=` | `AF_UNIX AF_NETLINK` | D-Bus and kernel device events only; **no network** |
| `SystemCallFilter=` | `@system-service` | |
| `MemoryDenyWriteExecute=`, `LockPersonality=`, `RestrictRealtime=`, `RestrictSUIDSGID=` | yes | |

The result is checked with `systemd-analyze security redqueend.service`.

## Firmware and drivers

- The stock `acer_wmi` driver is never replaced, blacklisted or patched.
- The optional companion kernel module is never installed automatically. It
  calls only a fixed list of firmware methods, validates every value, and
  loads only on tested models.
- Secure Boot is never disabled; if a module needs signing, the user is shown
  how to enrol a key with `mokutil`.
- No BIOS flashing and no boot-logo writing. Firmware updates go through
  fwupd only.

## Privacy

`redqueen probe` produces hardware reports for bug reports. It never includes
serial numbers, product UUIDs, MAC addresses, hostname, user name, or any file
contents outside hardware descriptions. Redaction is covered by tests.

Linking applications to profiles reads process names from `/proc` inside the
user's own session. That list never leaves the machine and is never sent to
the root daemon.

## Reporting a vulnerability

Please report security issues privately through GitHub's "Report a
vulnerability" (Security tab) rather than as a public issue.
