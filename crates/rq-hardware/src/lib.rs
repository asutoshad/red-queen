//! Hardware discovery and backends for The Red Queen.
//!
//! Everything is resolved through a [`SystemRoot`], so the same code runs
//! against the real `/` or a fake tree in tests. Nothing here spawns
//! external commands.

pub mod acer;
pub mod capabilities;
pub mod fan;
pub mod gpu;
pub mod hwmon;
pub mod identity;
pub mod models;
pub mod platform_profile;
pub mod power_supply;
pub mod probe;
pub mod profile;
pub mod redact;
pub mod root;
pub mod snapshot;
pub mod telemetry;

pub use probe::{ProbeContext, ProbeReport};
pub use root::SystemRoot;
pub use snapshot::SystemSnapshot;
pub use telemetry::Sampler;
