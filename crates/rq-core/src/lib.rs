//! Domain types shared by every part of The Red Queen.
//!
//! This crate performs no I/O. Hardware backends translate raw kernel
//! values into these types so that model-specific details never leak into
//! the daemon, the GUI or the command line.

pub mod capability;
pub mod fan;
pub mod identity;
pub mod power;
pub mod safety;
pub mod telemetry;
pub mod thermal;
pub mod units;

pub use capability::{Backend, CapabilityStatus, Feature, Maturity, Reason};
pub use fan::{FanMode, FanRole, RoleSource};
pub use identity::{HardwareIdentity, KernelInfo, OsInfo};
pub use power::BatteryState;
pub use safety::{
    HARD_MIN_PERCENT, SafetyConfig, SafetyFile, SafetyMonitor, SupervisedFan, TripReason,
};
pub use telemetry::{
    BatterySummary, CpuStatus, FanReading, GpuStatus, History, MemoryStatus, TelemetrySample,
};
pub use thermal::{InvalidProfileName, ThermalProfileId};
pub use units::{DutyScale, MilliCelsius, Percent, PercentOutOfRange, Rpm, TemperatureUnit};
