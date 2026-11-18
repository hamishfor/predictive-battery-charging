//! Writes the controller's target to the hardware, safely.
//!
//! Safety rules:
//! - Never write on hardware without a matching profile (fail closed).
//! - Below the floor (default 25%), always charge (to `max_pct`), whatever the controller says.
//!   A limit set by hand keeps its end, but its start is raised so charging begins.
//! - Read back after every write. If the firmware disagrees, report an error and don't retry in a tight loop.
//! - After switching to Full while on AC and below the ceiling, check that charging actually starts.
//!
//! Multiple batteries (laptops with two batteries, for example ThinkPad Power Bridge
//! models with an internal BAT0 plus a swappable BAT1):
//! - The same target is written to every battery that has threshold files. The
//!   firmware chooses which battery to charge, and charges each up to its own end
//!   threshold. So identical thresholds make the pair behave like one battery.
//! - The floor uses the *combined* charge, which is how much runtime is actually
//!   left. A nearly empty second battery alongside a full internal one isn't an emergency.
//! - Batteries are re-discovered on every call, so hot-swapping BAT1 is handled.
//!   A newly inserted battery gets the current target on the next sample.

use crate::alert::Alert;
use pbc_hw::profile::ThresholdRules;
use pbc_hw::thresholds::{apply, Thresholds};
use pbc_hw::{combine, Battery, BatteryStatus, PowerSnapshot, Profile, SysRoot};
use pbc_model::control::Target;
use std::collections::BTreeMap;

pub struct Actuator {
    root: SysRoot,
    profile: Profile,
    pub floor_pct: u8,
    /// Where the floor rule charges to. 100 is full.
    pub max_pct: u8,
    /// Battery name -> thresholds we last wrote (or confirmed) on it.
    last_written: BTreeMap<String, Thresholds>,
    /// (when we switched to Full, combined capacity then). Used to check charging resumed.
    resume_check: Option<(i64, u8)>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum Outcome {
    Unchanged,
    /// `batteries` lists every battery written this call (all of them, on a laptop with several batteries).
    Wrote {
        batteries: Vec<String>,
        from: Option<Thresholds>,
        to: Thresholds,
    },
    /// Floor rule forced charging to `max_pct`.
    Floor {
        to: Thresholds,
    },
}

pub const FULL: Thresholds = Thresholds { start: 0, end: 100 };

/// How far below the hold level charging resumes.
pub const RESUME_GAP: u8 = 5;

impl Actuator {
    pub fn new(
        root: SysRoot,
        profile: Profile,
        floor_pct: u8,
        max_pct: u8,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            root,
            profile,
            floor_pct,
            max_pct,
            last_written: BTreeMap::new(),
            resume_check: None,
        })
    }

    /// Batteries we can control right now (present, with threshold files).
    pub fn batteries(&self) -> Vec<Battery> {
        Battery::discover(&self.root)
            .unwrap_or_default()
            .into_iter()
            .filter(|b| b.has_thresholds())
            .collect()
    }

    fn combined(&self, bats: &[Battery]) -> PowerSnapshot {
        combine(&bats.iter().map(|b| b.snapshot()).collect::<Vec<_>>())
    }

    /// Thresholds for a target level. 100 is the firmware default (no limit).
    /// Below that, charging resumes `resume_gap` points under the end, so the
    /// firmware doesn't top up after every small drop. The profile's limits and
    /// minimum gap are respected.
    pub fn thresholds_for(&self, t: Target) -> Thresholds {
        if t.is_full() {
            return FULL;
        }
        let r = &self.profile.thresholds;
        let end = t.end.clamp(r.end_min.max(1), r.end_max.min(99));
        let gap = RESUME_GAP.max(r.min_gap);
        let start = end.saturating_sub(gap).clamp(r.start_min, r.start_max);
        Thresholds { start, end }
    }

    pub fn rules(&self) -> &ThresholdRules {
        &self.profile.thresholds
    }

    /// The profile's fixed hold, for `pbcctl hold`.
    pub fn hold_default(&self) -> Thresholds {
        let d = &self.profile.defaults;
        Thresholds {
            start: d.hold_start,
            end: d.hold_end,
        }
    }

    /// Thresholds shared by every controllable battery, or None if they differ (or there are none).
    pub fn current(&self) -> Option<Thresholds> {
        let all: Vec<Option<Thresholds>> =
            self.batteries().iter().map(|b| b.thresholds()).collect();
        let first = (*all.first()?)?;
        all.iter().all(|t| *t == Some(first)).then_some(first)
    }

    /// Per-battery thresholds, for status display.
    pub fn per_battery(&self) -> Vec<(String, Option<Thresholds>)> {
        self.batteries()
            .into_iter()
            .map(|b| (b.name.clone(), b.thresholds()))
            .collect()
    }

    /// Bring every battery in line with `target`. `now` is used for the resume check.
    /// Returns the first change made, or Unchanged. Writes to every battery even
    /// if one fails, then reports the first error.
    pub fn apply(&mut self, target: Target, now: i64) -> anyhow::Result<Outcome> {
        let bats = self.batteries();
        let combined = self.combined(&bats);
        let mut want = self.thresholds_for(target);
        let ceiling = self.thresholds_for(Target { end: self.max_pct });
        let floored =
            combined.capacity_pct.is_some_and(|c| c < self.floor_pct) && want.end < ceiling.end;
        if floored {
            want = ceiling;
        }
        want.start = self.kick_start(want, bats.first().and_then(|b| b.thresholds()), &combined);
        self.write(&bats, combined, want, floored, now)
    }

    /// Firmware only resumes below the start threshold, so raising the end with the level
    /// already between start and end does nothing (85/90 at 86% sits idle). Lift the start
    /// just above the level until charging is seen; then drop back to the normal gap.
    fn kick_start(&self, want: Thresholds, cur: Option<Thresholds>, snap: &PowerSnapshot) -> u8 {
        let Some(cap) = snap.capacity_pct else {
            return want.start;
        };
        if want.start == 0
            || cap < want.start
            || cap >= want.end
            || snap.status == BatteryStatus::Charging
        {
            return want.start;
        }
        let raising = cur.is_none_or(|c| c.end < want.end);
        let pending = cur.is_some_and(|c| c.end == want.end && c.start > want.start);
        if !(raising || pending) {
            return want.start; // holding; let it drift down to the normal start
        }
        let r = &self.profile.thresholds;
        (cap + 1)
            .min(want.end.saturating_sub(r.min_gap))
            .clamp(r.start_min, r.start_max)
            .max(want.start)
    }

    /// As `apply`, for a limit set by hand: these exact thresholds, unless below the
    /// floor with charging not due, where the start is raised to the floor.
    pub fn apply_manual(&mut self, t: Thresholds, now: i64) -> anyhow::Result<Outcome> {
        let bats = self.batteries();
        let combined = self.combined(&bats);
        let r = &self.profile.thresholds;
        let mut want = t;
        let floored = combined
            .capacity_pct
            .is_some_and(|c| c < self.floor_pct && c >= t.start);
        if floored {
            want = if t.end > self.floor_pct {
                let start = self
                    .floor_pct
                    .min(t.end.saturating_sub(r.min_gap))
                    .clamp(r.start_min, r.start_max);
                Thresholds { start, end: t.end }
            } else {
                self.thresholds_for(Target { end: self.max_pct })
            };
        }
        self.write(&bats, combined, want, floored, now)
    }

    fn write(
        &mut self,
        bats: &[Battery],
        combined: PowerSnapshot,
        want: Thresholds,
        floored: bool,
        now: i64,
    ) -> anyhow::Result<Outcome> {
        // Forget batteries that are gone (BAT1 removed).
        self.last_written
            .retain(|name, _| bats.iter().any(|b| &b.name == name));
        // The end threshold before this write, to tell a raise (charging should start) from a lowering.
        let cur_end = bats.first().and_then(|b| b.thresholds()).map(|t| t.end);
        let mut outcome = Outcome::Unchanged;
        let mut first_err: Option<anyhow::Error> = None;
        for b in bats {
            let cur = b.thresholds();
            if cur == Some(want) {
                self.last_written.insert(b.name.clone(), want);
                continue;
            }
            match apply(&self.root, b, want, &self.profile.thresholds) {
                Ok(got) if got == want => {
                    self.last_written.insert(b.name.clone(), want);
                    match &mut outcome {
                        Outcome::Wrote { batteries, .. } => batteries.push(b.name.clone()),
                        Outcome::Floor { .. } => {}
                        Outcome::Unchanged => {
                            outcome = if floored {
                                Outcome::Floor { to: want }
                            } else {
                                Outcome::Wrote {
                                    batteries: vec![b.name.clone()],
                                    from: cur,
                                    to: want,
                                }
                            };
                        }
                    }
                }
                Ok(got) => {
                    first_err.get_or_insert_with(|| {
                        anyhow::anyhow!("{}: firmware reports {got} after writing {want}", b.name)
                    });
                }
                Err(e) => {
                    first_err.get_or_insert_with(|| anyhow::anyhow!("{}: {e}", b.name));
                }
            }
        }
        if outcome != Outcome::Unchanged {
            // Only when the firmware should start now: below the end, and below the start (0 = always).
            let due = combined
                .capacity_pct
                .filter(|&c| c < want.end && (want.start == 0 || c < want.start));
            if want == FULL || cur_end.is_some_and(|c| want.end > c) {
                if let Some(c) = due {
                    self.resume_check = Some((now, c));
                } else {
                    self.resume_check = None;
                }
            } else if cur_end.is_some_and(|c| want.end < c) {
                self.resume_check = None;
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(outcome),
        }
    }

    /// Periodic health checks. Returns alerts for the caller to log and expose.
    pub fn check(&mut self, now: i64, online: bool, grace_s: i64) -> Vec<Alert> {
        let mut alerts = Vec::new();
        let bats = self.batteries();
        for b in &bats {
            if let (Some(exp), Some(found)) = (self.last_written.get(&b.name), b.thresholds()) {
                if *exp != found {
                    alerts.push(Alert::Overwritten {
                        battery: b.name.clone(),
                        expected: *exp,
                        found,
                    });
                }
            }
        }
        if let Some((since, cap0)) = self.resume_check {
            let snap = self.combined(&bats);
            let cap = snap.capacity_pct.unwrap_or(0);
            // Any battery charging counts. Firmware usually charges them one at a time.
            let charging = matches!(snap.status, BatteryStatus::Charging | BatteryStatus::Full)
                || cap > cap0
                || cap >= 99;
            if !online || charging {
                self.resume_check = None;
            } else if now - since >= grace_s {
                alerts.push(Alert::NotCharging {
                    since,
                    capacity: cap,
                });
                self.resume_check = None; // alert once
            }
        }
        alerts
    }

    /// Restore firmware defaults (charge to 100%) on every battery. Used on disable and uninstall.
    pub fn release(&mut self) -> anyhow::Result<()> {
        let mut first_err = None;
        for b in self.batteries() {
            if b.thresholds() != Some(FULL) {
                if let Err(e) = apply(&self.root, &b, FULL, &self.profile.thresholds) {
                    first_err.get_or_insert(anyhow::anyhow!("{}: {e}", b.name));
                }
            }
        }
        self.last_written.clear();
        first_err.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const H80: Target = Target { end: 80 };
    use std::fs;
    use std::path::Path;

    fn put(root: &Path, rel: &str, v: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, v).unwrap();
    }

    fn fake(dir: &Path, cap: u8, status: &str) {
        put(dir, "class/dmi/id/sys_vendor", "LENOVO");
        put(
            dir,
            "class/dmi/id/product_version",
            "ThinkPad X1 Carbon Gen 13",
        );
        fs::create_dir_all(dir.join("module/thinkpad_acpi")).unwrap();
        put(dir, "class/power_supply/AC/type", "Mains");
        put(dir, "class/power_supply/AC/online", "1");
        let b = "class/power_supply/BAT0";
        put(dir, &format!("{b}/type"), "Battery");
        put(dir, &format!("{b}/status"), status);
        put(dir, &format!("{b}/capacity"), &cap.to_string());
        put(dir, &format!("{b}/charge_control_start_threshold"), "0");
        put(dir, &format!("{b}/charge_control_end_threshold"), "100");
    }

    fn act(dir: &Path) -> Actuator {
        let root = SysRoot::new(dir);
        Actuator::new(root, Profile::builtin().remove(0), 25, 100).unwrap()
    }

    #[test]
    fn hold_then_full() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 60, "Charging");
        let mut a = act(t.path());
        assert!(matches!(a.apply(H80, 0).unwrap(), Outcome::Wrote { .. }));
        assert_eq!(a.current(), Some(Thresholds { start: 75, end: 80 }));
        assert_eq!(a.apply(H80, 1).unwrap(), Outcome::Unchanged);
        assert!(matches!(
            a.apply(Target::FULL, 2).unwrap(),
            Outcome::Wrote { .. }
        ));
        assert_eq!(a.current(), Some(FULL));
    }

    #[test]
    fn floor_forces_full() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 10, "Discharging");
        let mut a = act(t.path());
        put(
            t.path(),
            "class/power_supply/BAT0/charge_control_start_threshold",
            "75",
        );
        put(
            t.path(),
            "class/power_supply/BAT0/charge_control_end_threshold",
            "80",
        );
        assert_eq!(a.apply(H80, 0).unwrap(), Outcome::Floor { to: FULL });
        assert_eq!(a.current(), Some(FULL));
    }

    #[test]
    fn manual_is_exact_and_floor_raises_start() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 60, "Charging");
        let mut a = act(t.path());
        let m = Thresholds { start: 10, end: 60 };
        assert!(matches!(
            a.apply_manual(m, 0).unwrap(),
            Outcome::Wrote { .. }
        ));
        assert_eq!(a.current(), Some(m));
        put(t.path(), "class/power_supply/BAT0/capacity", "15");
        let floored = Thresholds { start: 25, end: 60 };
        assert_eq!(
            a.apply_manual(m, 1).unwrap(),
            Outcome::Floor { to: floored }
        );
        assert_eq!(a.current(), Some(floored));
        // Below its start already: charging is due, so written as set.
        put(t.path(), "class/power_supply/BAT0/capacity", "5");
        a.apply_manual(m, 2).unwrap();
        assert_eq!(a.current(), Some(m));
    }

    #[test]
    fn detects_overwrite() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 60, "Charging");
        let mut a = act(t.path());
        a.apply(H80, 0).unwrap();
        // UPower writes its own end threshold
        put(
            t.path(),
            "class/power_supply/BAT0/charge_control_end_threshold",
            "60",
        );
        let al = a.check(10, true, 300);
        assert!(
            matches!(&al[0], Alert::Overwritten { battery, .. } if battery == "BAT0"),
            "{al:?}"
        );
    }

    #[test]
    fn alerts_if_charging_never_resumes() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 79, "Not charging");
        put(
            t.path(),
            "class/power_supply/BAT0/charge_control_start_threshold",
            "75",
        );
        put(
            t.path(),
            "class/power_supply/BAT0/charge_control_end_threshold",
            "80",
        );
        let mut a = act(t.path());
        a.apply(Target::FULL, 0).unwrap();
        assert!(a.check(100, true, 300).is_empty(), "still in grace");
        let al = a.check(301, true, 300);
        assert!(matches!(al[0], Alert::NotCharging { .. }), "{al:?}");
        assert!(a.check(400, true, 300).is_empty(), "alert only once");
    }

    #[test]
    fn raise_above_level_lifts_start() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 86, "Not charging");
        let bat = "class/power_supply/BAT0";
        put(
            t.path(),
            &format!("{bat}/charge_control_start_threshold"),
            "75",
        );
        put(
            t.path(),
            &format!("{bat}/charge_control_end_threshold"),
            "80",
        );
        let mut a = act(t.path());
        let h90 = Target { end: 90 };
        a.apply(h90, 0).unwrap();
        assert_eq!(a.current(), Some(Thresholds { start: 87, end: 90 }));
        a.apply(h90, 60).unwrap();
        assert_eq!(
            a.current(),
            Some(Thresholds { start: 87, end: 90 }),
            "kept until charging"
        );
        put(t.path(), &format!("{bat}/status"), "Charging");
        a.apply(h90, 120).unwrap();
        assert_eq!(
            a.current(),
            Some(Thresholds { start: 85, end: 90 }),
            "normal gap once charging"
        );
        // Holding at 90 and drifting to 88: no top-up.
        put(t.path(), &format!("{bat}/status"), "Not charging");
        put(t.path(), &format!("{bat}/capacity"), "88");
        assert_eq!(a.apply(h90, 180).unwrap(), Outcome::Unchanged);
    }

    #[test]
    fn no_alert_when_firmware_should_not_start() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 89, "Not charging");
        let bat = "class/power_supply/BAT0";
        put(
            t.path(),
            &format!("{bat}/charge_control_start_threshold"),
            "75",
        );
        put(
            t.path(),
            &format!("{bat}/charge_control_end_threshold"),
            "80",
        );
        let mut a = act(t.path());
        a.apply_manual(Thresholds { start: 85, end: 90 }, 0)
            .unwrap();
        assert!(a.check(301, true, 300).is_empty());
    }

    #[test]
    fn no_alert_when_charging_resumes() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 79, "Not charging");
        let mut a = act(t.path());
        put(
            t.path(),
            "class/power_supply/BAT0/charge_control_end_threshold",
            "80",
        );
        put(
            t.path(),
            "class/power_supply/BAT0/charge_control_start_threshold",
            "75",
        );
        a.apply(Target::FULL, 0).unwrap();
        put(t.path(), "class/power_supply/BAT0/status", "Charging");
        assert!(a.check(301, true, 300).is_empty());
    }

    /// Adds a removable second battery, energy-weighted with BAT0.
    fn add_bat1(dir: &Path, cap: u8, present: bool) {
        let b = "class/power_supply/BAT1";
        put(dir, &format!("{b}/type"), "Battery");
        put(
            dir,
            &format!("{b}/present"),
            if present { "1" } else { "0" },
        );
        put(dir, &format!("{b}/status"), "Not charging");
        put(dir, &format!("{b}/capacity"), &cap.to_string());
        put(dir, &format!("{b}/charge_control_start_threshold"), "0");
        put(dir, &format!("{b}/charge_control_end_threshold"), "100");
    }

    fn thr(dir: &Path, bat: &str) -> String {
        let r = |a: &str| {
            fs::read_to_string(dir.join(format!("class/power_supply/{bat}/{a}"))).unwrap()
        };
        format!(
            "{}/{}",
            r("charge_control_start_threshold"),
            r("charge_control_end_threshold")
        )
    }

    #[test]
    fn writes_every_battery() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 60, "Charging");
        add_bat1(t.path(), 60, true);
        let mut a = act(t.path());
        a.apply(H80, 0).unwrap();
        assert_eq!(thr(t.path(), "BAT0"), "75/80");
        assert_eq!(thr(t.path(), "BAT1"), "75/80");
        assert_eq!(a.current(), Some(Thresholds { start: 75, end: 80 }));
        a.apply(Target::FULL, 1).unwrap();
        assert_eq!(thr(t.path(), "BAT1"), "0/100");
        a.release().unwrap();
    }

    #[test]
    fn hot_inserted_battery_gets_the_target() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 60, "Charging");
        add_bat1(t.path(), 30, false); // slot empty
        let mut a = act(t.path());
        a.apply(H80, 0).unwrap();
        assert_eq!(thr(t.path(), "BAT1"), "0/100", "absent battery untouched");
        // Second battery inserted. The next sample writes the hold to it as well.
        put(t.path(), "class/power_supply/BAT1/present", "1");
        assert!(
            matches!(a.apply(H80, 60).unwrap(), Outcome::Wrote { ref batteries, .. } if batteries == &["BAT1"])
        );
        assert_eq!(thr(t.path(), "BAT1"), "75/80");
        // Removed again. No stale overwrite alert for it.
        put(t.path(), "class/power_supply/BAT1/present", "0");
        assert!(a.check(120, true, 300).is_empty());
    }

    #[test]
    fn overwrite_on_one_battery_is_reported_by_name() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 60, "Charging");
        add_bat1(t.path(), 60, true);
        let mut a = act(t.path());
        a.apply(H80, 0).unwrap();
        put(
            t.path(),
            "class/power_supply/BAT1/charge_control_end_threshold",
            "90",
        );
        let al = a.check(10, true, 300);
        assert_eq!(al.len(), 1);
        assert!(
            matches!(&al[0], Alert::Overwritten { battery, .. } if battery == "BAT1"),
            "{al:?}"
        );
        assert_eq!(a.current(), None, "batteries now disagree");
    }

    #[test]
    fn floor_uses_combined_charge() {
        let t = tempfile::tempdir().unwrap();
        // BAT0 at 60% and nearly empty BAT1: combined charge is well above the 25% floor, so hold.
        fake(t.path(), 60, "Discharging");
        put(t.path(), "class/power_supply/BAT0/energy_now", "34000000");
        put(t.path(), "class/power_supply/BAT0/energy_full", "57000000");
        add_bat1(t.path(), 5, true);
        put(t.path(), "class/power_supply/BAT1/energy_now", "1100000");
        put(t.path(), "class/power_supply/BAT1/energy_full", "23000000");
        let mut a = act(t.path());
        assert!(
            matches!(a.apply(H80, 0).unwrap(), Outcome::Wrote { .. }),
            "combined 44%: no floor"
        );
        // Both low: combined below the floor, so charge.
        put(t.path(), "class/power_supply/BAT0/energy_now", "10000000");
        assert_eq!(a.apply(H80, 1).unwrap(), Outcome::Floor { to: FULL });
        assert_eq!(thr(t.path(), "BAT0"), "0/100");
        assert_eq!(thr(t.path(), "BAT1"), "0/100");
    }

    #[test]
    fn graded_thresholds() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 60, "Charging");
        let mut a = act(t.path());
        assert_eq!(
            a.thresholds_for(Target { end: 80 }),
            Thresholds { start: 75, end: 80 }
        );
        assert_eq!(
            a.thresholds_for(Target { end: 70 }),
            Thresholds { start: 65, end: 70 }
        );
        assert_eq!(
            a.thresholds_for(Target { end: 99 }),
            Thresholds { start: 94, end: 99 }
        );
        assert_eq!(a.thresholds_for(Target::FULL), FULL);
        a.apply(Target { end: 90 }, 0).unwrap();
        assert_eq!(a.current(), Some(Thresholds { start: 85, end: 90 }));
    }

    #[test]
    fn release_restores_defaults() {
        let t = tempfile::tempdir().unwrap();
        fake(t.path(), 60, "Charging");
        let mut a = act(t.path());
        a.apply(H80, 0).unwrap();
        a.release().unwrap();
        assert_eq!(a.current(), Some(FULL));
    }
}
