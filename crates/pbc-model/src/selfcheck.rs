//! What this machine has shown it can do, learned from normal use.
//!
//! Each capability is judged from what the daemon sees anyway: holds,
//! releases, shutdowns, and maintenance wakes. A guided `pbcctl selftest` run
//! can confirm a capability straight away. That's the only way to confirm
//! that a quiet wake is invisible, because nothing on the machine can see
//! the screen or hear the fan.

use crate::Ts;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Charging stops at the end threshold.
    Enforce,
    /// Charging resumes when the limit is raised, without a replug.
    Resume,
    /// The firmware keeps the thresholds through a power-off.
    Persist,
    /// A maintenance wake during sleep stays invisible to the desktop.
    QuietWake,
}

impl Capability {
    pub const ALL: [Capability; 4] = [
        Capability::Enforce,
        Capability::Resume,
        Capability::Persist,
        Capability::QuietWake,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Capability::Enforce => "enforce",
            Capability::Resume => "resume",
            Capability::Persist => "persist",
            Capability::QuietWake => "quiet_wake",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Capability::Enforce => "Charging stops at the limit",
            Capability::Resume => "Charging resumes when the limit is lifted",
            Capability::Persist => "Limits are kept while powered off",
            Capability::QuietWake => "Wakes during sleep stay invisible",
        }
    }

    /// Clean observations needed before a passive result counts as working.
    fn needed(self) -> u32 {
        match self {
            Capability::Enforce | Capability::Resume => 3,
            Capability::Persist => 2,
            Capability::QuietWake => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Not enough evidence yet.
    Unknown,
    /// Enough clean observations, and no failures.
    Working,
    /// Failed at least once. Failures win, because they're the costly case.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Seen by pbcd during normal use.
    Observed,
    /// Confirmed by the user in a guided `pbcctl selftest` run.
    Confirmed,
}

/// The running record for one capability on this machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub capability: Capability,
    pub passes: u32,
    pub failures: u32,
    pub last_ts: Option<Ts>,
    /// Short reason for the last failure, shown in reports.
    pub last_failure: Option<String>,
    /// Set by a guided selftest. It overrides the passive counts until reset.
    pub confirmed: Option<bool>,
}

impl Record {
    pub fn new(capability: Capability) -> Self {
        Self {
            capability,
            passes: 0,
            failures: 0,
            last_ts: None,
            last_failure: None,
            confirmed: None,
        }
    }

    pub fn pass(&mut self, ts: Ts) {
        self.passes += 1;
        self.last_ts = Some(ts);
    }

    pub fn fail(&mut self, ts: Ts, why: impl Into<String>) {
        self.failures += 1;
        self.last_ts = Some(ts);
        self.last_failure = Some(why.into());
    }

    pub fn verdict(&self) -> Verdict {
        match self.confirmed {
            Some(true) => Verdict::Working,
            Some(false) => Verdict::Failed,
            None if self.failures > 0 => Verdict::Failed,
            None if self.passes >= self.capability.needed() => Verdict::Working,
            None => Verdict::Unknown,
        }
    }

    pub fn source(&self) -> Option<Source> {
        match (self.confirmed, self.passes + self.failures) {
            (Some(_), _) => Some(Source::Confirmed),
            (None, 0) => None,
            (None, _) => Some(Source::Observed),
        }
    }
}

/// How a maintenance wake ended, from the sleep hook's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    /// Thresholds written, then the kernel slept again with the desktop frozen.
    Clean,
    /// Something went wrong that the user might have seen or heard.
    Bad(String),
    /// Not a test of the quiet wake: the user woke the machine, or it wasn't ours.
    NotTested,
}

/// Classify one night from the hook's decisions, in order.
///
/// `decisions` pairs a decision name with its detail. Only wakes the hook
/// treated as ours say anything about the quiet wake. A normal resume because
/// the lid opened is the hook doing its job, not a failure.
pub fn classify_wake(decisions: &[(String, String)]) -> WakeOutcome {
    let ours = decisions
        .iter()
        .any(|(d, _)| d == "wrote" || d == "skip_write");
    if !ours {
        return WakeOutcome::NotTested;
    }
    for (d, detail) in decisions {
        if d == "post" && detail.contains("frozen=Some(false)") {
            return WakeOutcome::Bad("desktop was awake during a maintenance wake".into());
        }
        if d == "normal_resume" {
            let user = [
                "lid opened",
                "lid not closed",
                "other wake sources",
                "not at scheduled time",
                "already done",
            ];
            if user.iter().any(|u| detail.contains(u)) {
                continue;
            }
            return WakeOutcome::Bad(format!("woke the desktop: {detail}"));
        }
    }
    let resuspended = decisions.iter().any(|(d, _)| d == "resuspend");
    if resuspended {
        WakeOutcome::Clean
    } else {
        // Ours, but the user opened the lid before the re-sleep. It worked, but proves nothing about silence.
        WakeOutcome::NotTested
    }
}

/// Charging behaviour over one hold or release, from consecutive samples.
#[derive(Debug, Clone, Copy)]
pub struct ChargeObs {
    pub ts: Ts,
    pub online: bool,
    pub energy_uwh: Option<u64>,
    pub capacity: Option<u8>,
}

/// Did energy keep rising during a hold, past `end`? Uses energy, not the
/// status field, which some firmware flips between charging and pending every
/// few seconds. Returns None when the samples can't decide it.
pub fn judge_enforce(samples: &[ChargeObs], end: u8, min_span_s: i64) -> Option<bool> {
    let above: Vec<&ChargeObs> = samples
        .iter()
        .filter(|s| s.online && s.capacity.is_some_and(|c| c >= end))
        .collect();
    let (first, last) = (above.first()?, above.last()?);
    if last.ts - first.ts < min_span_s {
        return None;
    }
    let (e0, e1) = (first.energy_uwh?, last.energy_uwh?);
    // More than 2% of capacity gained while at the limit means it isn't holding.
    let gained_pct = last.capacity? as i64 - first.capacity? as i64;
    Some(!(e1 > e0 && gained_pct >= 2))
}

/// After the limit is raised, did energy start rising within the grace period?
///
/// The baseline is the last sample at or before `raised_at`. Samples are
/// taken once a minute, so the first one after the raise may already show the rise.
pub fn judge_resume(samples: &[ChargeObs], raised_at: Ts, grace_s: i64) -> Option<bool> {
    let base = samples.iter().rfind(|s| s.ts <= raised_at && s.online)?;
    if base.capacity.is_some_and(|c| c >= 99) {
        return None; // Already full: nothing to resume.
    }
    let e0 = base.energy_uwh?;
    let after: Vec<&ChargeObs> = samples
        .iter()
        .filter(|s| s.ts > raised_at && s.online)
        .collect();
    let rose = after
        .iter()
        .any(|s| s.ts - raised_at <= grace_s && s.energy_uwh.is_some_and(|e| e > e0));
    if rose {
        Some(true)
    } else if after.last().is_some_and(|l| l.ts - raised_at >= grace_s) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(a: &str, b: &str) -> (String, String) {
        (a.into(), b.into())
    }

    #[test]
    fn verdict_rules() {
        let mut r = Record::new(Capability::Enforce);
        assert_eq!(r.verdict(), Verdict::Unknown);
        assert_eq!(r.source(), None);
        r.pass(1);
        r.pass(2);
        assert_eq!(r.verdict(), Verdict::Unknown);
        r.pass(3);
        assert_eq!(r.verdict(), Verdict::Working);
        assert_eq!(r.source(), Some(Source::Observed));
        r.fail(4, "kept charging");
        assert_eq!(r.verdict(), Verdict::Failed, "one failure outweighs passes");
        r.confirmed = Some(true);
        assert_eq!(r.verdict(), Verdict::Working, "a guided run overrides");
        assert_eq!(r.source(), Some(Source::Confirmed));
    }

    #[test]
    fn clean_wake() {
        let n = [
            d("armed", ""),
            d("post", "lid_closed=true frozen=Some(true)"),
            d("wrote", "0/100 on 1 battery"),
            d("resuspend", "nested=1"),
            d("normal_resume", "maintenance already done this sleep"),
        ];
        assert_eq!(classify_wake(&n), WakeOutcome::Clean);
    }

    #[test]
    fn on_battery_wake_counts_as_a_clean_quiet_wake() {
        // Slept on battery and stayed unplugged: the hook skipped the write and re-slept.
        let n = [
            d("armed", ""),
            d("post", "lid_closed=true frozen=Some(true) ac=false"),
            d("skip_write", "ac offline"),
            d("resuspend", "nested=1"),
            d("normal_resume", "maintenance already done this sleep"),
        ];
        assert_eq!(classify_wake(&n), WakeOutcome::Clean);
    }

    #[test]
    fn user_opening_lid_is_not_a_test() {
        let n = [
            d("armed", ""),
            d("post", "lid_closed=false frozen=Some(true)"),
            d("normal_resume", "not at scheduled time"),
        ];
        assert_eq!(classify_wake(&n), WakeOutcome::NotTested);
        let n = [
            d("post", ""),
            d("wrote", "0/100"),
            d("normal_resume", "lid opened during settle window"),
        ];
        assert_eq!(classify_wake(&n), WakeOutcome::NotTested);
    }

    #[test]
    fn failed_write_or_thawed_desktop_is_bad() {
        let n = [
            d("post", "frozen=Some(true)"),
            d("wrote", "0/100"),
            d("normal_resume", "kernel refused suspend (wakeup pending)"),
        ];
        assert!(matches!(classify_wake(&n), WakeOutcome::Bad(_)));
        let n = [
            d("post", "lid_closed=true frozen=Some(false)"),
            d("wrote", "0/100"),
            d("resuspend", ""),
        ];
        assert!(
            matches!(classify_wake(&n), WakeOutcome::Bad(m) if m.contains("desktop was awake"))
        );
    }

    fn obs(ts: Ts, cap: u8, e: u64) -> ChargeObs {
        ChargeObs {
            ts,
            online: true,
            energy_uwh: Some(e),
            capacity: Some(cap),
        }
    }

    #[test]
    fn enforce_holds_and_fails() {
        // Held at 80 for 20 min, energy flat.
        let held: Vec<_> = (0..20).map(|i| obs(i * 60, 80, 41_800_000)).collect();
        assert_eq!(judge_enforce(&held, 80, 600), Some(true));
        // Kept charging past 80.
        let leaky: Vec<_> = (0..20)
            .map(|i| obs(i * 60, 80 + (i / 4) as u8, 41_800_000 + i as u64 * 200_000))
            .collect();
        assert_eq!(judge_enforce(&leaky, 80, 600), Some(false));
        // Too short to tell.
        assert_eq!(judge_enforce(&held[..3], 80, 600), None);
    }

    #[test]
    fn resume_rules() {
        let ok = [obs(0, 80, 41_800_000), obs(60, 80, 42_000_000)];
        assert_eq!(judge_resume(&ok, 0, 300), Some(true));
        let stuck: Vec<_> = (0..7).map(|i| obs(i * 60, 80, 41_800_000)).collect();
        assert_eq!(judge_resume(&stuck, 0, 300), Some(false));
        assert_eq!(
            judge_resume(&stuck[..3], 0, 300),
            None,
            "still within grace"
        );
        let full = [obs(0, 100, 52_000_000)];
        assert_eq!(judge_resume(&full, 0, 300), None);
    }
}
