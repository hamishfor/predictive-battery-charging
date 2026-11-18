//! Every message pbcd shows the user (GNOME widget, `pbcctl status`), and when each one fires.
//! Alerts go into `Core::alerts` (newest last, capped at 20); the widget shows the newest.
//! Pause reasons go into `Ctl::paused` and become the widget's subtitle.

use pbc_hw::thresholds::Thresholds;
use std::fmt;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum Alert {
    /// `Actuator::check`: on AC, the end was raised above the level, and the battery still
    /// isn't charging `resume_grace_s` later. Alerts once per raise.
    NotCharging { since: i64, capacity: u8 },
    /// `Actuator::check`: a battery's thresholds differ from what pbcd last wrote
    /// (UPower, TLP, a user). pbcd rewrites them on the same tick.
    Overwritten {
        battery: String,
        expected: Thresholds,
        found: Thresholds,
    },
    /// `Ctl::step`: writing thresholds to sysfs failed, or the firmware read back other values.
    WriteFailed(String),
    /// `check_upower` with `upower_switch = false`: UPower's own limit is on, so pbcd pauses.
    UpowerLimitOn,
}

impl fmt::Display for Alert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotCharging { capacity, .. } => write!(
                f,
                "On AC at {capacity}% but not charging, something went wrong trying to resume charging"
            ),
            Self::Overwritten {
                battery,
                expected,
                found,
            } => write!(
                f,
                "{battery} thresholds changed by something else: {found}, expected {expected}"
            ),
            Self::WriteFailed(e) => write!(f, "write failed: {e}"),
            Self::UpowerLimitOn => f.write_str(Pause::UpowerLimitOn.text()),
        }
    }
}

/// Why pbcd isn't writing thresholds (`upower::verdict`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    /// `upower_switch = true`: "Maximize Charge" chosen in GNOME Settings. pbcd releases to 0/100.
    MaximizeCharge,
    /// `upower_switch = false`: UPower's limit is on; also raised as `Alert::UpowerLimitOn`.
    UpowerLimitOn,
}

impl Pause {
    pub fn text(self) -> &'static str {
        match self {
            Self::MaximizeCharge => {
                "Off: \"Maximize Charge\" is selected in Settings → Power → Battery Charging"
            }
            Self::UpowerLimitOn => {
                "UPower's charge limit is on (Settings → Power → Preserve Battery Health). pbcd won't write thresholds until it's off"
            }
        }
    }
}

impl fmt::Display for Pause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}
