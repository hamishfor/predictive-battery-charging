#![allow(dead_code)] // shared with pbcd; the hook uses only part of it
//! The maintenance plan shared between `pbcd` and `pbcd-sleep-hook`.
//!
//! pbcd writes it before sleep, inside its logind delay inhibitor. The hook
//! reads it during systemd-sleep's post phase. It's a small JSON file in
//! /run, so the hook needs no IPC and keeps working even if pbcd is hung.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DIR: &str = "/run/pbcd";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Unix seconds to wake at.
    pub wake_at: i64,
    /// Thresholds to write on that wake.
    pub start: u8,
    pub end: u8,
    /// Battery sysfs directories, e.g. /sys/class/power_supply/BAT0 and BAT1 on a
    /// laptop with two batteries. Every one gets the same thresholds.
    /// Accepts the older single `battery` field as a one-item list.
    #[serde(alias = "battery", deserialize_with = "one_or_many")]
    pub batteries: Vec<PathBuf>,
    /// Profile rule: start must be at least this far below end.
    pub min_gap: u8,
    /// Written by `pbcctl selftest`. pbcd leaves a pending selftest plan in place.
    #[serde(default)]
    pub selftest: bool,
}

/// A selftest plan that hasn't fired yet. pbcd must not overwrite it before sleep.
pub fn pending_selftest(dir: &Path, now: i64) -> bool {
    read(dir).is_some_and(|p| p.selftest && p.wake_at > now)
}

/// A selftest plan due within the last 15 minutes, or still to come. pbcd
/// leaves its log for `pbcctl selftest` to read. An older one was abandoned.
pub fn recent_selftest(dir: &Path, now: i64) -> bool {
    read(dir).is_some_and(|p| p.selftest && p.wake_at > now - 15 * 60)
}

pub fn plan_path(dir: &Path) -> PathBuf {
    dir.join("wake-plan.json")
}
pub fn done_path(dir: &Path) -> PathBuf {
    dir.join("wake-done")
}
pub fn log_path(dir: &Path) -> PathBuf {
    dir.join("wake-log.jsonl")
}

pub fn write(dir: &Path, p: &Plan) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(".wake-plan.tmp");
    std::fs::write(&tmp, serde_json::to_vec(p)?)?;
    std::fs::rename(tmp, plan_path(dir))?;
    let _ = std::fs::remove_file(done_path(dir));
    Ok(())
}

pub fn read(dir: &Path) -> Option<Plan> {
    serde_json::from_slice(&std::fs::read(plan_path(dir)).ok()?).ok()
}

pub fn clear(dir: &Path) {
    let _ = std::fs::remove_file(plan_path(dir));
    let _ = std::fs::remove_file(done_path(dir));
}

/// One line per hook decision, read back by pbcd after resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub ts: i64,
    pub decision: String,
    pub detail: String,
    pub awake_ms: u64,
}

pub fn append_log(dir: &Path, l: &LogLine) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(dir))
    {
        let _ = writeln!(f, "{}", serde_json::to_string(l).unwrap_or_default());
    }
}

/// Read and truncate the hook log.
pub fn take_log(dir: &Path) -> Vec<LogLine> {
    let p = log_path(dir);
    let out = std::fs::read_to_string(&p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let _ = std::fs::remove_file(p);
    out
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<PathBuf>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(PathBuf),
        Many(Vec<PathBuf>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(p) => vec![p],
        OneOrMany::Many(v) => v,
    })
}
