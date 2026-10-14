//! Runs the debug pbcd-sleep-hook binary against a fake /sys and /proc tree.
//! It covers every "is this wake ours?" branch listed in the pbcd-sleep-hook module doc
//! without root. The fake /sys/power/state is a directory, so the
//! re-suspend write fails and the hook ends with "kernel refused suspend".
//! The test then checks that the re-suspend was attempted.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn put(root: &Path, rel: &str, v: &str) {
    let p = root.join(rel.trim_start_matches('/'));
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, v).unwrap();
}

struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new(lid: &str, ac: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        put(
            r,
            "/proc/acpi/button/lid/LID/state",
            &format!("state:      {lid}\n"),
        );
        put(r, "/sys/class/power_supply/AC/type", "Mains");
        put(
            r,
            "/sys/class/power_supply/AC/online",
            if ac { "1" } else { "0" },
        );
        put(r, "/sys/class/power_supply/BAT0/type", "Battery");
        put(
            r,
            "/sys/class/power_supply/BAT0/charge_control_start_threshold",
            "75",
        );
        put(
            r,
            "/sys/class/power_supply/BAT0/charge_control_end_threshold",
            "80",
        );
        put(
            r,
            "/sys/fs/cgroup/user.slice/cgroup.events",
            "populated 1\nfrozen 1\n",
        );
        put(r, "/sys/class/rtc/rtc0/wakealarm", "");
        put(r, "/sys/power/wakeup_count", "42");
        fs::create_dir_all(r.join("sys/power/state")).unwrap();
        put(r, "/dev/kmsg", "");
        for (i, name) in ["rtc_cmos", "PNP0C0D:00", "serio0", "AC"]
            .iter()
            .enumerate()
        {
            put(r, &format!("/sys/class/wakeup/wakeup{i}/name"), name);
            put(r, &format!("/sys/class/wakeup/wakeup{i}/event_count"), "10");
        }
        fs::create_dir_all(r.join("run")).unwrap();
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn plan(&self, wake_at: i64) {
        let bat = self.root().join("sys/class/power_supply/BAT0");
        let plan = serde_json::json!({"wake_at": wake_at, "start": 0, "end": 100, "battery": bat, "min_gap": 1});
        fs::write(self.root().join("run/wake-plan.json"), plan.to_string()).unwrap();
    }

    /// Plan in the multi-battery format.
    fn plan_batteries(&self, wake_at: i64, names: &[&str]) {
        let bats: Vec<_> = names
            .iter()
            .map(|n| self.root().join(format!("sys/class/power_supply/{n}")))
            .collect();
        let plan = serde_json::json!({"wake_at": wake_at, "start": 0, "end": 100, "batteries": bats, "min_gap": 1});
        fs::write(self.root().join("run/wake-plan.json"), plan.to_string()).unwrap();
    }

    fn add_bat1(&self, present: bool) {
        let b = "/sys/class/power_supply/BAT1";
        put(self.root(), &format!("{b}/type"), "Battery");
        put(
            self.root(),
            &format!("{b}/present"),
            if present { "1" } else { "0" },
        );
        put(
            self.root(),
            &format!("{b}/charge_control_start_threshold"),
            "75",
        );
        put(
            self.root(),
            &format!("{b}/charge_control_end_threshold"),
            "80",
        );
    }

    fn thresholds_of(&self, bat: &str) -> String {
        let b = self.root().join(format!("sys/class/power_supply/{bat}"));
        let r = |a: &str| fs::read_to_string(b.join(a)).unwrap().trim().to_string();
        format!(
            "{}/{}",
            r("charge_control_start_threshold"),
            r("charge_control_end_threshold")
        )
    }

    fn bump(&self, source: &str) {
        for e in fs::read_dir(self.root().join("sys/class/wakeup")).unwrap() {
            let d = e.unwrap().path();
            if fs::read_to_string(d.join("name")).unwrap() == source {
                fs::write(d.join("event_count"), "11").unwrap();
            }
        }
    }

    /// Make rtc0's device the PNP device "00:04", with a wakeup source of that
    /// name.
    fn add_pnp_rtc(&self) {
        let r = self.root();
        let dev = r.join("sys/devices/pnp0/00:04");
        fs::create_dir_all(&dev).unwrap();
        std::os::unix::fs::symlink(&dev, r.join("sys/class/rtc/rtc0/device")).unwrap();
        let w = r.join("sys/class/wakeup/wakeup9");
        fs::create_dir_all(&w).unwrap();
        fs::write(w.join("name"), "00:04").unwrap();
        fs::write(w.join("event_count"), "10").unwrap();
        std::os::unix::fs::symlink(&dev, w.join("device")).unwrap();
    }

    fn run_kind(&self, phase: &str, kind: &str) -> (String, Duration) {
        let t = Instant::now();
        let out = Command::new(env!("CARGO_BIN_EXE_pbcd-sleep-hook"))
            .args([phase, kind])
            .env("PBCD_HOOK_ROOT", self.root())
            .env("PBCD_RUNTIME_DIR", self.root().join("run"))
            .output()
            .unwrap();
        assert!(out.status.success(), "hook must always exit 0");
        (
            String::from_utf8_lossy(&out.stderr).into_owned(),
            t.elapsed(),
        )
    }

    fn run(&self, phase: &str) -> (String, Duration) {
        self.run_kind(phase, "suspend")
    }

    /// Arm a plan due now, run pre, then mark the RTC as the wake source.
    fn armed_rtc_wake(&self) {
        self.plan(now());
        self.run("pre");
        self.bump("rtc_cmos");
    }

    fn thresholds(&self) -> String {
        let b = self.root().join("sys/class/power_supply/BAT0");
        let r = |a: &str| fs::read_to_string(b.join(a)).unwrap().trim().to_string();
        format!(
            "{}/{}",
            r("charge_control_start_threshold"),
            r("charge_control_end_threshold")
        )
    }
}

#[test]
fn no_plan_is_a_no_op() {
    let f = Fake::new("closed", true);
    let (log, _) = f.run("post");
    assert!(log.is_empty(), "{log}");
}

#[test]
fn pre_arms_rtc() {
    let f = Fake::new("closed", true);
    let at = now() + 3600;
    f.plan(at);
    let (log, _) = f.run("pre");
    assert!(log.contains("armed"), "{log}");
    let rtc = fs::read_to_string(f.root().join("sys/class/rtc/rtc0/wakealarm")).unwrap();
    assert_eq!(rtc, at.to_string());
}

#[test]
fn our_wake_writes_and_resuspends() {
    let f = Fake::new("closed", true);
    f.armed_rtc_wake();
    let (log, took) = f.run("post");
    assert!(log.contains("wrote: 0/100"), "{log}");
    assert!(log.contains("resuspend"), "{log}");
    assert_eq!(f.thresholds(), "0/100");
    assert!(
        took >= Duration::from_millis(2300),
        "settle window, took {took:?}"
    );
}

#[test]
fn lid_open_is_normal_resume_without_delay() {
    let f = Fake::new("open", true);
    f.armed_rtc_wake();
    let (log, took) = f.run("post");
    assert!(log.contains("normal_resume: lid not closed"), "{log}");
    assert_eq!(f.thresholds(), "75/80", "no write on a user wake");
    assert!(
        took < Duration::from_millis(500),
        "no delay on a user wake: {took:?}"
    );
}

#[test]
fn lid_wake_source_is_normal_resume() {
    let f = Fake::new("closed", true);
    f.armed_rtc_wake();
    f.bump("PNP0C0D:00");
    let (log, _) = f.run("post");
    assert!(log.contains("other wake sources: PNP0C0D:00"), "{log}");
    assert_eq!(f.thresholds(), "75/80");
}

#[test]
fn keyboard_or_ac_event_is_normal_resume() {
    for src in ["serio0", "AC"] {
        let f = Fake::new("closed", true);
        f.armed_rtc_wake();
        f.bump(src);
        let (log, _) = f.run("post");
        assert!(log.contains("other wake sources"), "{src}: {log}");
    }
}

#[test]
fn wrong_time_is_normal_resume() {
    let f = Fake::new("closed", true);
    f.plan(now() + 3600);
    f.run("pre");
    f.bump("rtc_cmos");
    let (log, _) = f.run("post");
    assert!(log.contains("not at scheduled time"), "{log}");
}

#[test]
fn rtc_not_fired_is_normal_resume() {
    let f = Fake::new("closed", true);
    f.plan(now());
    f.run("pre");
    let (log, _) = f.run("post");
    assert!(log.contains("rtc did not fire"), "{log}");
}

#[test]
fn rtc_named_after_its_pnp_device_is_ours() {
    let f = Fake::new("closed", true);
    f.add_pnp_rtc();
    f.plan(now());
    f.run("pre");
    f.bump("00:04");
    let (log, _) = f.run("post");
    assert!(log.contains("wrote: 0/100"), "{log}");
    assert!(log.contains("resuspend"), "{log}");
}

impl Fake {
    /// A USB-C power source with a wakeup entry of its own, as on a ThinkPad T480.
    fn add_ucsi(&self) {
        let r = self.root();
        let n = "ucsi-source-psy-USBC000:001";
        put(r, &format!("/sys/class/power_supply/{n}/type"), "USB");
        put(r, &format!("/sys/class/power_supply/{n}/online"), "0");
        put(r, "/sys/class/wakeup/wakeup8/name", n);
        put(r, "/sys/class/wakeup/wakeup8/event_count", "10");
    }

    fn plan_ignoring(&self, wake_at: i64, ignore: &[&str]) {
        let bat = self.root().join("sys/class/power_supply/BAT0");
        let plan = serde_json::json!({"wake_at": wake_at, "start": 0, "end": 100, "batteries": [bat], "min_gap": 1, "ignore_wake": ignore});
        fs::write(self.root().join("run/wake-plan.json"), plan.to_string()).unwrap();
    }
}

#[test]
fn unlisted_charger_wake_is_normal_resume() {
    let f = Fake::new("closed", true);
    f.add_ucsi();
    f.armed_rtc_wake();
    f.bump("ucsi-source-psy-USBC000:001");
    let (log, _) = f.run("post");
    assert!(
        log.contains("other wake sources: ucsi-source-psy-USBC000:001"),
        "{log}"
    );
}

#[test]
fn listed_charger_wake_is_ours_with_a_longer_settle() {
    let f = Fake::new("closed", true);
    f.add_ucsi();
    f.plan_ignoring(now(), &["ucsi-source-psy-*"]);
    f.run("pre");
    f.bump("rtc_cmos");
    f.bump("ucsi-source-psy-USBC000:001");
    let (log, took) = f.run("post");
    assert!(
        log.contains("ignored=[ucsi-source-psy-USBC000:001(+1)]"),
        "{log}"
    );
    assert!(log.contains("resuspend"), "{log}");
    assert!(
        took >= Duration::from_millis(5300),
        "longer settle, took {took:?}"
    );
}

#[test]
fn ignore_list_cannot_hide_a_non_charger() {
    let f = Fake::new("closed", true);
    f.plan_ignoring(now(), &["serio0", "PNP0C0D:00"]);
    f.run("pre");
    f.bump("rtc_cmos");
    f.bump("serio0");
    let (log, _) = f.run("post");
    assert!(log.contains("other wake sources: serio0"), "{log}");
}

#[test]
fn listed_charger_wake_off_ac_is_normal_resume() {
    let f = Fake::new("closed", false);
    f.add_ucsi();
    f.plan_ignoring(now(), &["ucsi-source-psy-USBC000:001"]);
    f.run("pre");
    f.bump("rtc_cmos");
    f.bump("ucsi-source-psy-USBC000:001");
    let (log, _) = f.run("post");
    assert!(log.contains("ignored source fired while off AC"), "{log}");
}

#[test]
fn already_done_is_normal_resume() {
    let f = Fake::new("closed", true);
    f.plan(now());
    f.run("pre");
    fs::write(f.root().join("run/wake-done"), "1").unwrap();
    f.bump("rtc_cmos");
    let (log, _) = f.run("post");
    assert!(log.contains("already done"), "{log}");
}

#[test]
fn on_battery_skips_write_but_still_resuspends() {
    let f = Fake::new("closed", false);
    f.armed_rtc_wake();
    let (log, _) = f.run("post");
    assert!(log.contains("skip_write: ac offline"), "{log}");
    assert_eq!(f.thresholds(), "75/80");
    assert!(log.contains("resuspend"), "{log}");
}

#[test]
fn unfrozen_user_slice_is_normal_resume() {
    let f = Fake::new("closed", true);
    put(
        f.root(),
        "/sys/fs/cgroup/user.slice/cgroup.events",
        "populated 1\nfrozen 0\n",
    );
    f.armed_rtc_wake();
    let (log, _) = f.run("post");
    assert!(log.contains("user.slice not frozen"), "{log}");
}

#[test]
fn write_failure_is_normal_resume() {
    let f = Fake::new("closed", true);
    let p = f
        .root()
        .join("sys/class/power_supply/BAT0/charge_control_end_threshold");
    fs::remove_file(&p).unwrap();
    fs::create_dir(&p).unwrap();
    f.armed_rtc_wake();
    let (log, _) = f.run("post");
    assert!(log.contains("threshold write failed"), "{log}");
}

#[test]
fn decisions_are_logged_for_pbcd() {
    let f = Fake::new("open", true);
    f.armed_rtc_wake();
    f.run("post");
    let log = fs::read_to_string(f.root().join("run/wake-log.jsonl")).unwrap();
    assert!(
        log.lines()
            .any(|l| l.contains("\"decision\":\"normal_resume\"")),
        "{log}"
    );
}

#[test]
fn hibernate_is_ignored() {
    let f = Fake::new("closed", true);
    f.plan(now());
    let (log, _) = f.run_kind("post", "hibernate");
    assert!(log.is_empty(), "{log}");
}

#[test]
fn power_bridge_writes_both_batteries() {
    let f = Fake::new("closed", true);
    f.add_bat1(true);
    f.plan_batteries(now(), &["BAT0", "BAT1"]);
    f.run("pre");
    f.bump("rtc_cmos");
    let (log, _) = f.run("post");
    assert!(log.contains("wrote: 0/100 on 2 batteries"), "{log}");
    assert_eq!(f.thresholds_of("BAT0"), "0/100");
    assert_eq!(f.thresholds_of("BAT1"), "0/100");
}

#[test]
fn pulled_rear_battery_is_skipped() {
    let f = Fake::new("closed", true);
    f.add_bat1(false);
    f.plan_batteries(now(), &["BAT0", "BAT1"]);
    f.run("pre");
    f.bump("rtc_cmos");
    let (log, _) = f.run("post");
    assert!(log.contains("wrote: 0/100 on 1 battery"), "{log}");
    assert_eq!(f.thresholds_of("BAT0"), "0/100");
    assert_eq!(f.thresholds_of("BAT1"), "75/80", "absent battery untouched");
}

#[test]
fn old_single_battery_plan_still_works() {
    // Plan in the older single-battery format: { "battery": "..." }.
    let f = Fake::new("closed", true);
    f.armed_rtc_wake();
    let (log, _) = f.run("post");
    assert!(log.contains("wrote: 0/100 on 1 battery"), "{log}");
}
