//! pbcd-sleep-hook: quiet maintenance wake ("nested suspend").
//!
//! Installed as /usr/lib/systemd/system-sleep/pbcd. systemd-sleep runs it
//! with `pre|post suspend|hibernate|...`. The post phase runs while
//! user.slice (GNOME, PipeWire, apps) is still frozen. If, and only if, this
//! wake is provably ours, we write the thresholds and suspend the kernel
//! again without returning, so user space never thaws. In every other case
//! we return at once, and the system resumes exactly as it normally would.
//!
//! It touches only: charge thresholds, the RTC wake alarm, /sys/power/state,
//! /sys/power/wakeup_count. Never Bluetooth, rfkill, or any app or device setting.
//!
//! A wake is ours only if all of these hold, otherwise it resumes normally:
//! maintenance has not run this sleep; the clock is within 5 s before to 60 s
//! after the planned time; the RTC wakeup count went up; no wakeup source
//! that a person would use changed (lid, power button, keyboard, USB devices);
//! the lid reads closed; and user.slice is not thawed. Re-suspend goes through
//! the wakeup_count handshake, so a lid opening at any moment aborts it.
//!
//! Chargers (Mains and USB power supplies) are judged by the timing instead.
//! Some laptops fire one on every wake while plugged in, and others fire one
//! about 3 s before the lid reads open. So a charger event alongside an on-time
//! RTC wake is accepted while AC is online, and the settle window is then
//! doubled, so a lid opening behind it still shows before re-suspend.

#[path = "../wakeplan.rs"]
mod wakeplan;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RTC: &str = "/sys/class/rtc/rtc0/wakealarm";

/// Test seam: in debug builds, PBCD_HOOK_ROOT prefixes every system path so
/// the decision logic can run against a fake tree. Release builds ignore it.
fn sys(path: &str) -> PathBuf {
    #[cfg(debug_assertions)]
    if let Ok(root) = std::env::var("PBCD_HOOK_ROOT") {
        return PathBuf::from(root).join(path.trim_start_matches('/'));
    }
    PathBuf::from(path)
}
const AWAKE_CAP: Duration = Duration::from_secs(30);
const WRITE_DEADLINE: Duration = Duration::from_secs(5);
/// Minimum time from kernel resume to re-suspend. The lid can read closed for
/// about 3 s after resume, and our own write causes power_supply events that need to settle.
const SETTLE: Duration = Duration::from_millis(3000);
/// The settle window when a charger also fired. Twice the usual, because on
/// some laptops a charger event is the first sign of a lid opening.
const SETTLE_CHARGER: Duration = Duration::from_millis(6000);
/// Typical delay from kernel resume to post-hook start.
const POST_START: Duration = Duration::from_millis(600);
const EARLY_S: i64 = 5;
const LATE_S: i64 = 60;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn read(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// Wakeup source name -> event_count.
fn snapshot() -> HashMap<String, u64> {
    let mut m = HashMap::new();
    if let Ok(rd) = fs::read_dir(sys("/sys/class/wakeup")) {
        for e in rd.flatten() {
            let p = e.path();
            if let (Some(name), Some(c)) = (
                read(p.join("name")),
                read(p.join("event_count")).and_then(|v| v.parse().ok()),
            ) {
                // Several sources can share a name. Key by the directory too.
                m.insert(format!("{}|{}", e.file_name().to_string_lossy(), name), c);
            }
        }
    }
    m
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Rtc,
    /// A Mains or USB power supply.
    Charger,
    Other,
}

/// Wakeup sources whose event count went up, as (kind, "name(+n)").
fn changed(before: &HashMap<String, u64>, after: &HashMap<String, u64>) -> Vec<(Source, String)> {
    let rtc_dirs = rtc_wakeup_dirs();
    let mut v: Vec<(Source, String)> = after
        .iter()
        .filter(|(k, c)| before.get(*k).is_some_and(|b| b != *c))
        .map(|(k, c)| {
            let (dir, name) = k.split_once('|').unwrap_or(("", k));
            let kind = if rtc_dirs.iter().any(|d| d == dir)
                || name == "rtc_cmos"
                || name.starts_with("alarmtimer")
            {
                Source::Rtc
            } else if is_charger(name) {
                Source::Charger
            } else {
                Source::Other
            };
            (kind, format!("{name}(+{})", c - before[k]))
        })
        .collect();
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

/// Is `name` a Mains or USB power supply? Read from its `type`, so a keyboard,
/// lid or power button can never count as one, whatever it's called.
fn is_charger(name: &str) -> bool {
    !name.contains('/')
        && matches!(
            read(sys("/sys/class/power_supply").join(name).join("type")).as_deref(),
            Some("Mains") | Some("USB")
        )
}

fn labels(ch: &[(Source, String)], kind: Source) -> String {
    ch.iter()
        .filter(|(k, _)| *k == kind)
        .map(|(_, s)| s.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The wakeup sources that belong to rtc0's device. Their name is the device's,
/// which isn't always "rtc_cmos"
fn rtc_wakeup_dirs() -> Vec<String> {
    let Ok(rtc) = fs::canonicalize(sys("/sys/class/rtc/rtc0/device")) else {
        return Vec::new();
    };
    let Ok(rd) = fs::read_dir(sys("/sys/class/wakeup")) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| fs::canonicalize(e.path().join("device")).is_ok_and(|d| d == rtc))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn lid_closed() -> bool {
    // No lid device (desktop, or lid switch missing) means "not closed" (fail-safe).
    fs::read_dir(sys("/proc/acpi/button/lid"))
        .ok()
        .and_then(|mut rd| rd.next())
        .and_then(|e| e.ok())
        .and_then(|e| read(e.path().join("state")))
        .is_some_and(|s| s.contains("closed"))
}

fn ac_online() -> bool {
    let Ok(rd) = fs::read_dir(sys("/sys/class/power_supply")) else {
        return false;
    };
    rd.flatten().any(|e| {
        let p = e.path();
        matches!(read(p.join("type")).as_deref(), Some("Mains") | Some("USB"))
            && read(p.join("online")).as_deref() == Some("1")
    })
}

fn user_slice_frozen() -> Option<bool> {
    read(sys("/sys/fs/cgroup/user.slice/cgroup.events"))?
        .lines()
        .find_map(|l| l.strip_prefix("frozen ").map(|v| v == "1"))
}

struct Hook {
    dir: PathBuf,
    t0: Instant,
}

impl Hook {
    fn log(&self, decision: &str, detail: String) {
        let awake_ms = self.t0.elapsed().as_millis() as u64;
        let line = format!("pbcd-sleep-hook: {decision}: {detail} (awake {awake_ms}ms)");
        let _ = fs::write(sys("/dev/kmsg"), format!("<6>{line}\n"));
        eprintln!("{line}");
        wakeplan::append_log(
            &self.dir,
            &wakeplan::LogLine {
                ts: now(),
                decision: decision.into(),
                detail,
                awake_ms,
            },
        );
    }

    /// Give the system back to systemd-sleep: a normal resume.
    fn normal(&self, why: impl Into<String>) -> ! {
        let _ = fs::write(sys(RTC), "0");
        self.log("normal_resume", why.into());
        std::process::exit(0);
    }
}

/// Write the thresholds in the profile-safe order, then read them back. It
/// runs in a thread with a deadline, because firmware writes can hang.
fn write_thresholds(bat: &Path, start: u8, end: u8, gap: u8) -> Result<(), String> {
    let bat = bat.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = (|| -> Result<(), String> {
            let rd = |a: &str| {
                read(bat.join(a))
                    .and_then(|v| v.parse::<u8>().ok())
                    .ok_or(format!("read {a}"))
            };
            let cur_start = rd("charge_control_start_threshold")?;
            let w = |a: &str, v: u8| {
                fs::write(bat.join(a), v.to_string()).map_err(|e| format!("write {a}={v}: {e}"))
            };
            // End first, unless that would break start + gap <= end.
            if cur_start.saturating_add(gap) <= end {
                w("charge_control_end_threshold", end)?;
                w("charge_control_start_threshold", start)?;
            } else {
                w("charge_control_start_threshold", start)?;
                w("charge_control_end_threshold", end)?;
            }
            let (s, e) = (
                rd("charge_control_start_threshold")?,
                rd("charge_control_end_threshold")?,
            );
            if (s, e) != (start, end) {
                return Err(format!("read back {s}/{e}, wanted {start}/{end}"));
            }
            Ok(())
        })();
        let _ = tx.send(r);
    });
    rx.recv_timeout(WRITE_DEADLINE)
        .unwrap_or_else(|_| Err("timed out".into()))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (phase, kind) = (
        args.get(1).map(String::as_str),
        args.get(2).map(String::as_str),
    );
    if kind != Some("suspend") {
        return;
    }
    let dir =
        PathBuf::from(std::env::var("PBCD_RUNTIME_DIR").unwrap_or_else(|_| wakeplan::DIR.into()));
    let Some(plan) = wakeplan::read(&dir) else {
        return;
    };
    let h = Hook {
        dir: dir.clone(),
        t0: Instant::now(),
    };
    let snap_path = dir.join("wake-snapshot");
    let done = wakeplan::done_path(&dir);

    if phase == Some("pre") {
        let _ = fs::write(
            &snap_path,
            serde_json::to_vec(&snapshot()).unwrap_or_default(),
        );
        if !done.exists() && plan.wake_at > now() {
            let _ = fs::write(sys(RTC), "0");
            match fs::write(sys(RTC), plan.wake_at.to_string()) {
                Ok(()) => h.log("armed", format!("rtc wake at {}", plan.wake_at)),
                Err(e) => h.log("arm_failed", format!("{e}")),
            }
        }
        return;
    }
    if phase != Some("post") {
        return;
    }

    let mut base: HashMap<String, u64> = fs::read(&snap_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut nested = 0u32;
    loop {
        let t_resume = Instant::now();
        let after = snapshot();
        let ch = changed(&base, &after);
        let charger = ch.iter().any(|(k, _)| *k == Source::Charger);
        let t = now();
        h.log(
            "post",
            format!(
                "lid_closed={} frozen={:?} ac={} wake=[{}] charger=[{}] t-wake={}s nested={nested}",
                lid_closed(),
                user_slice_frozen(),
                ac_online(),
                ch.iter()
                    .filter(|(k, _)| *k != Source::Charger)
                    .map(|(_, s)| s.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                labels(&ch, Source::Charger),
                t - plan.wake_at
            ),
        );

        // Proof that this wake is ours. Anything else is a normal resume.
        if done.exists() {
            h.normal("maintenance already done this sleep");
        }
        if t < plan.wake_at - EARLY_S || t > plan.wake_at + LATE_S {
            h.normal("not at scheduled time");
        }
        if !ch.iter().any(|(k, _)| *k == Source::Rtc) {
            h.normal("rtc did not fire");
        }
        if ch.iter().any(|(k, _)| *k == Source::Other) {
            h.normal(format!(
                "other wake sources: {}",
                labels(&ch, Source::Other)
            ));
        }
        // A charger event with no AC means it was just unplugged: someone is there.
        if charger && !ac_online() {
            h.normal(format!(
                "charger unplugged: {}",
                labels(&ch, Source::Charger)
            ));
        }
        if !lid_closed() {
            h.normal("lid not closed");
        }
        if user_slice_frozen() == Some(false) {
            h.normal("user.slice not frozen (unexpected); not interfering");
        }
        let _ = fs::write(&done, b"1");
        if !ac_online() {
            // On battery the thresholds don't matter. Sleep on without writing.
            h.log("skip_write", "ac offline".into());
        } else {
            // Every battery in the plan. A pulled second battery (present=0, or its
            // directory gone) is skipped. Any other failure is retried once, then
            // falls back to a normal resume.
            let mut r: Result<(), String> = Ok(());
            let mut wrote = 0;
            for bat in &plan.batteries {
                if !bat.exists() || read(bat.join("present")).as_deref() == Some("0") {
                    continue;
                }
                let mut one = write_thresholds(bat, plan.start, plan.end, plan.min_gap);
                if one.is_err() {
                    std::thread::sleep(Duration::from_millis(500));
                    one = write_thresholds(bat, plan.start, plan.end, plan.min_gap);
                }
                match one {
                    Ok(()) => wrote += 1,
                    Err(e) => {
                        let name = bat
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        r = Err(format!("{name}: {e}"));
                        break;
                    }
                }
            }
            if r.is_ok() && wrote == 0 {
                r = Err("no battery from the plan is present".into());
            }
            match r {
                Ok(()) => h.log(
                    "wrote",
                    format!(
                        "{}/{} on {wrote} batter{}",
                        plan.start,
                        plan.end,
                        if wrote == 1 { "y" } else { "ies" }
                    ),
                ),
                // Resume normally. pbcd sees this in the log and tops off from user space.
                Err(e) => h.normal(format!("threshold write failed: {e}")),
            }
        }

        // Settle window, polling the lid.
        let settle = if charger { SETTLE_CHARGER } else { SETTLE }.saturating_sub(POST_START);
        while t_resume.elapsed() < settle {
            if !lid_closed() {
                h.normal("lid opened during settle window");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if h.t0.elapsed() > AWAKE_CAP {
            h.normal("awake cap reached");
        }

        // Race-free re-suspend through the kernel's wakeup_count handshake.
        base = snapshot();
        let Some(wc) = read(sys("/sys/power/wakeup_count")) else {
            h.normal("wakeup_count unreadable")
        };
        if !lid_closed() {
            h.normal("lid opened before re-suspend");
        }
        if fs::write(sys("/sys/power/wakeup_count"), &wc).is_err() {
            h.normal("wakeup event during window (handshake refused)");
        }
        h.log("resuspend", format!("nested={}", nested + 1));
        if fs::write(sys("/sys/power/state"), "mem").is_err() {
            h.normal("kernel refused suspend (wakeup pending)");
        }
        nested += 1;
        // Back from the nested sleep. Loop to re-check this wake.
    }
}
