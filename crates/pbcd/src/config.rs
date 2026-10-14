//! /etc/pbcd/config.toml. Every field is optional, so a missing file means defaults.

use pbc_model::control::SleepFallback;
use pbc_model::predict::ScheduleRule;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// SQLite database path.
    pub db: PathBuf,
    /// Extra hardware profiles directory (in addition to the built-in ones).
    pub profiles_dir: Option<PathBuf>,
    /// Seconds between battery samples while awake.
    pub sample_interval_s: u64,
    /// Keep raw samples this many days. Events and sessions are kept forever.
    pub sample_retention_days: i64,
    /// Merge unplug/replug gaps shorter than this (seconds).
    pub merge_gap_s: i64,
    /// Sysfs root. Only changed for tests.
    pub sysfs: PathBuf,
    /// `control` (the default: hold and top off once a routine is learned) or
    /// `collect` (learn only, never write).
    pub mode: Mode,
    /// Manual schedules.
    pub schedule: Vec<ScheduleRule>,
    /// Always charge below this capacity, whatever the prediction says.
    pub floor_pct: u8,
    /// Hold range. A confident prediction holds at `hold_min`, a weak one near
    /// `hold_max`, and no prediction charges to 100%.
    pub hold_min: u8,
    pub hold_max: u8,
    /// While an unplug may be near, step up to this level before the final charge to 100%.
    pub ready_pct: u8,
    /// Ignore predictions below this confidence.
    pub min_confidence: f64,
    /// Start the top-off this many minutes earlier than the estimate.
    pub margin_min: i64,
    /// Quiet maintenance wake (see pbcd-sleep-hook). `auto`: on for validated
    /// models and for machines whose own checks passed, off after any failure.
    pub quiet_wake: QuietWake,
    /// With `quiet_wake = "auto"` on an untested model, try it and let the
    /// self-checks decide. Off by default: an untested wake could be visible.
    pub quiet_wake_try_untested: bool,
    /// Removed in 0.3.1: chargers are now judged by the wake's timing on every
    /// laptop. Still accepted, so a config from 0.3.0 keeps working.
    #[serde(rename = "quiet_wake_ignore", skip_serializing)]
    pub removed_quiet_wake_ignore: Option<toml::Value>,
    /// If the top-off is due within this many minutes of sleeping, start it before sleep.
    pub presleep_window_min: i64,
    /// With no quiet wake: `full` (start the top-off before sleeping) or `hold`.
    pub sleep_fallback: SleepFallback,
    /// Value the firmware keeps while powered off, on laptops that keep it. `hold` (default) or `full`.
    pub shutdown: ShutdownValue,
    /// Seconds to wait for charging to resume after switching to full before alerting.
    pub resume_grace_s: i64,
    /// Runtime directory shared with pbcd-sleep-hook.
    pub runtime_dir: PathBuf,
    /// Use GNOME Settings → Power → Battery Charging as the on/off switch:
    /// "Preserve Battery Health" turns Predictive Battery Charging on, "Maximize Charge" turns it off.
    pub upower_switch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuietWake {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShutdownValue {
    Hold,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Observe and learn only.
    Collect,
    /// Hold and top off. Needs a matching hardware profile.
    Control,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db: "/var/lib/pbcd/pbcd.db".into(),
            profiles_dir: None,
            sample_interval_s: 60,
            sample_retention_days: 180,
            merge_gap_s: 300,
            sysfs: "/sys".into(),
            mode: Mode::Control,
            schedule: Vec::new(),
            floor_pct: 25,
            hold_min: 70,
            hold_max: 99,
            ready_pct: 90,
            min_confidence: 0.3,
            margin_min: 15,
            quiet_wake: QuietWake::Auto,
            quiet_wake_try_untested: false,
            removed_quiet_wake_ignore: None,
            presleep_window_min: 30,
            sleep_fallback: SleepFallback::Full,
            shutdown: ShutdownValue::Hold,
            resume_grace_s: 300,
            runtime_dir: "/run/pbcd".into(),
            upower_switch: true,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dist/config.toml, uncommented, parses and matches the defaults it claims to show.
    #[test]
    fn shipped_config_shows_the_defaults() {
        let shipped = include_str!("../../../dist/config.toml");
        let mut on = String::new();
        for l in shipped.lines() {
            let l = l.strip_prefix("# ").unwrap_or("");
            let setting = l.split_once(" = ").is_some_and(|(k, _)| {
                !k.is_empty() && k.chars().all(|c| c.is_ascii_lowercase() || c == '_')
            });
            // The [[schedule]] block is an example, not a default.
            if setting && !l.starts_with("days") && !l.starts_with("full_by") {
                on.push_str(l);
                on.push('\n');
            }
        }
        let c: Config = toml::from_str(&on).expect("dist/config.toml settings parse");
        let d = Config::default();
        assert_eq!(
            format!("{c:?}"),
            format!("{d:?}"),
            "dist/config.toml shows values that aren't the defaults"
        );
        assert!(on.lines().count() >= 15, "found only:\n{on}");
    }

    #[test]
    fn removed_quiet_wake_ignore_still_loads() {
        let c: Config =
            toml::from_str(r#"quiet_wake_ignore = ["ucsi-source-psy-USBC000:001"]"#).unwrap();
        assert!(c.removed_quiet_wake_ignore.is_some());
    }

    #[test]
    fn empty_is_default() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.sample_interval_s, 60);
        assert_eq!(c.mode, Mode::Control);
    }

    #[test]
    fn schedule_parses() {
        let c: Config = toml::from_str(
            r#"
            sample_interval_s = 30
            [[schedule]]
            days = ["mon", "tue", "wed", "thu", "fri"]
            full_by = "08:00"
            "#,
        )
        .unwrap();
        assert_eq!(c.schedule.len(), 1);
        pbc_model::predict::Schedule::new(&c.schedule).unwrap();
    }

    #[test]
    fn control_options_parse() {
        let c: Config = toml::from_str(
            r#"
            mode = "control"
            hold_min = 75
            hold_max = 95
            quiet_wake = "off"
            sleep_fallback = "hold"
            shutdown = "full"
            "#,
        )
        .unwrap();
        assert_eq!(c.mode, Mode::Control);
        assert_eq!((c.hold_min, c.hold_max), (75, 95));
        assert_eq!(c.quiet_wake, QuietWake::Off);
        assert_eq!(c.sleep_fallback, SleepFallback::Hold);
        assert_eq!(c.shutdown, ShutdownValue::Full);
    }

    #[test]
    fn unknown_keys_rejected() {
        assert!(toml::from_str::<Config>("bogus = 1").is_err());
    }
}
