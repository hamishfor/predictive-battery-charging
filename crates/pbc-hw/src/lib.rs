//! Hardware access layer for predictive battery charging.
//!
//! Everything is resolved relative to a [`SysRoot`], so tests and dry runs can
//! point at a fake sysfs tree instead of the real `/sys`.

pub mod battery;
pub mod profile;
pub mod report;
pub mod sysfs;
pub mod thresholds;
pub mod wakeplan;

pub use battery::{combine, AcState, Battery, BatteryStatus, PowerSnapshot};
pub use profile::{DmiInfo, Profile};
pub use sysfs::SysRoot;
pub use thresholds::{plan_writes, ThresholdError, Thresholds};
