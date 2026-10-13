//! Passive self-checks: watch normal use and record what this machine can do.
//!
//! Failures switch the matching feature off for this machine:
//! - charging doesn't resume after a hold: stop limiting, restore 0/100
//! - a quiet wake misbehaves: stop arming quiet wakes, top off before sleep instead
//!
//! `pbcctl selftest` writes confirmed results into the same records.

use pbc_model::selfcheck::{
    classify_wake, judge_enforce, judge_resume, Capability, ChargeObs, Record, Verdict, WakeOutcome,
};
use pbc_model::Ts;
use pbc_store::{Sample, Store};

/// KV key for the thresholds written just before shutdown.
pub const KV_SHUTDOWN_WRITE: &str = "selfcheck_shutdown_write";

fn obs(s: &Sample) -> ChargeObs {
    ChargeObs {
        ts: s.ts,
        online: s.online,
        energy_uwh: s.energy_uwh,
        capacity: s.capacity,
    }
}

fn update(store: &Store, c: Capability, f: impl FnOnce(&mut Record)) -> anyhow::Result<Record> {
    let mut r = store.capability(c)?;
    let before = r.verdict();
    f(&mut r);
    store.set_capability(&r)?;
    if r.verdict() != before {
        tracing::info!(capability = c.key(), verdict = ?r.verdict(), passes = r.passes, failures = r.failures, "self-check");
    }
    Ok(r)
}

/// Judge a finished hold: did charging stop at `end`?
pub fn hold_ended(store: &Store, from: Ts, to: Ts, end: u8) -> anyhow::Result<()> {
    let samples: Vec<ChargeObs> = store.samples_between(from, to)?.iter().map(obs).collect();
    match judge_enforce(&samples, end, 15 * 60) {
        Some(true) => {
            update(store, Capability::Enforce, |r| r.pass(to))?;
        }
        Some(false) => {
            update(store, Capability::Enforce, |r| {
                r.fail(to, format!("kept charging past {end}%"))
            })?;
        }
        None => {}
    }
    Ok(())
}

/// Judge a release: did charging start again? Returns false if it clearly didn't.
pub fn release_checked(
    store: &Store,
    raised_at: Ts,
    now: Ts,
    grace_s: i64,
) -> anyhow::Result<Option<bool>> {
    let samples: Vec<ChargeObs> = store
        // From just before the raise, for the baseline reading.
        .samples_between(raised_at - 120, now)?
        .iter()
        .map(obs)
        .collect();
    let v = judge_resume(&samples, raised_at, grace_s);
    match v {
        Some(true) => {
            update(store, Capability::Resume, |r| r.pass(now))?;
        }
        Some(false) => {
            update(store, Capability::Resume, |r| {
                r.fail(now, "charging did not resume after the limit was raised")
            })?;
        }
        None => {}
    }
    Ok(v)
}

/// Record the thresholds written just before shutdown, for `boot_check`.
pub fn shutdown_written(store: &Store, start: u8, end: u8) -> anyhow::Result<()> {
    store.set(KV_SHUTDOWN_WRITE, &format!("{start}/{end}"))
}

/// At startup, compare what the firmware kept with what was written before
/// shutdown. Call this before anything else writes the thresholds.
pub fn boot_check(store: &Store, now: Ts, found: Option<(u8, u8)>) -> anyhow::Result<()> {
    let Some(written) = store.get(KV_SHUTDOWN_WRITE)? else {
        return Ok(());
    };
    store.set(KV_SHUTDOWN_WRITE, "")?;
    if written.is_empty() {
        return Ok(()); // Daemon restart, not a power cycle.
    }
    let Some((s, e)) = found else { return Ok(()) };
    // 0/100 was written: the firmware default tells us nothing.
    if written == "0/100" {
        return Ok(());
    }
    if written == format!("{s}/{e}") {
        update(store, Capability::Persist, |r| r.pass(now))?;
    } else {
        update(store, Capability::Persist, |r| {
            r.fail(
                now,
                format!("wrote {written} before shutdown, found {s}/{e} at boot"),
            )
        })?;
    }
    Ok(())
}

/// Judge the night's maintenance wake from the hook's log.
pub fn wake_logged(store: &Store, now: Ts, decisions: &[(String, String)]) -> anyhow::Result<()> {
    match classify_wake(decisions) {
        WakeOutcome::Clean => {
            update(store, Capability::QuietWake, |r| r.pass(now))?;
        }
        WakeOutcome::Bad(why) => {
            update(store, Capability::QuietWake, |r| r.fail(now, why))?;
        }
        WakeOutcome::NotTested => {}
    }
    Ok(())
}

/// Should pbcd arm quiet wakes on this machine?
///
/// A failure always turns it off. A user confirmation, or the profile listing
/// this model, turns it on. Otherwise `try_untested` decides.
pub fn quiet_wake_allowed(store: &Store, model_validated: bool, try_untested: bool) -> bool {
    let r = store
        .capability(Capability::QuietWake)
        .unwrap_or_else(|_| Record::new(Capability::QuietWake));
    match r.verdict() {
        Verdict::Failed => false,
        Verdict::Working => true,
        Verdict::Unknown => model_validated || r.confirmed == Some(true) || try_untested,
    }
}

/// Should pbcd limit charging at all? Only a resume failure stops it, because
/// that's the one case where limiting could leave the battery unable to charge.
pub fn limiting_allowed(store: &Store) -> bool {
    store
        .capability(Capability::Resume)
        .map(|r| r.verdict() != Verdict::Failed)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: Ts, cap: u8, e: u64) -> Sample {
        Sample {
            ts,
            online: true,
            status: None,
            capacity: Some(cap),
            energy_uwh: Some(e),
            power_uw: None,
            th_start: None,
            th_end: None,
        }
    }

    #[test]
    fn hold_and_release_are_recorded() {
        let s = Store::open_in_memory().unwrap();
        for t in 0..30 {
            s.append_sample(&sample(t * 60, 80, 41_800_000)).unwrap();
        }
        hold_ended(&s, 0, 30 * 60, 80).unwrap();
        assert_eq!(s.capability(Capability::Enforce).unwrap().passes, 1);
        // Released at 30 min, energy rises a minute later.
        s.append_sample(&sample(31 * 60, 80, 42_100_000)).unwrap();
        assert_eq!(
            release_checked(&s, 30 * 60, 31 * 60, 300).unwrap(),
            Some(true)
        );
        assert!(limiting_allowed(&s));
    }

    #[test]
    fn resume_failure_stops_limiting() {
        let s = Store::open_in_memory().unwrap();
        for t in 0..8 {
            s.append_sample(&sample(t * 60, 79, 41_500_000)).unwrap();
        }
        assert_eq!(release_checked(&s, 0, 7 * 60, 300).unwrap(), Some(false));
        assert!(!limiting_allowed(&s));
    }

    #[test]
    fn persistence_across_power_off() {
        let s = Store::open_in_memory().unwrap();
        shutdown_written(&s, 75, 80).unwrap();
        boot_check(&s, 100, Some((75, 80))).unwrap();
        assert_eq!(s.capability(Capability::Persist).unwrap().passes, 1);
        // A daemon restart without a shutdown write isn't judged.
        boot_check(&s, 200, Some((0, 100))).unwrap();
        assert_eq!(s.capability(Capability::Persist).unwrap().failures, 0);
        // Firmware forgot.
        shutdown_written(&s, 75, 80).unwrap();
        boot_check(&s, 300, Some((0, 100))).unwrap();
        assert_eq!(s.capability(Capability::Persist).unwrap().failures, 1);
    }

    #[test]
    fn quiet_wake_gate() {
        let s = Store::open_in_memory().unwrap();
        assert!(
            !quiet_wake_allowed(&s, false, false),
            "untested, not trying"
        );
        assert!(quiet_wake_allowed(&s, true, false), "validated model");
        assert!(
            quiet_wake_allowed(&s, false, true),
            "trying an untested model"
        );
        let bad = [
            ("post".into(), "frozen=Some(false)".into()),
            ("wrote".into(), "0/100".into()),
        ];
        wake_logged(&s, 1, &bad).unwrap();
        assert!(
            !quiet_wake_allowed(&s, true, true),
            "one bad night turns it off, even on a validated model"
        );
    }
}
