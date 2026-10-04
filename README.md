# The Red Queen

A Linux-native control center for Acer Nitro gaming laptops: thermal profiles,
fan control, hardware monitoring, and more. It discovers what the hardware
actually exposes, drives it through standard kernel interfaces wherever
possible, and runs privileged operations in a small, hardened system daemon.

> **Status: early development.** Not usable yet.

**Primary target:** Acer Nitro V 15 (ANV15-51). Other Nitro and Predator
models are planned through capability-detecting backends.

## Documentation

- [How it works](docs/architecture.md)
- [Security model](docs/security.md)

## Building from source

On Kali or Debian:

```sh
sudo apt install cargo rustc build-essential pkgconf libgtk-4-dev libadwaita-1-dev gettext
cargo build --release
cargo test
```

Check what your laptop supports (read-only, no root needed):

```sh
cargo run --release -p redqueen -- probe            # human-readable
cargo run --release -p redqueen -- probe --json     # for issue reports
```

The JSON report never contains serial numbers, UUIDs, MAC addresses,
the hostname or the user name.

## Disclaimer

This project is not affiliated with Acer. Acer, Nitro, NitroSense and Predator
are trademarks of Acer Inc. The Red Queen is an independent, unofficial project
and contains no Acer code or assets.

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE).
