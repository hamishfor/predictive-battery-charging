//! Predictor interface, plus manual schedules ("full by 08:00 on weekdays").
//!
//! The controller asks "when must the battery be full?" Every predictor
//! answers with the same type. A schedule isn't a predictor on its own: it
//! never makes pbcd hold. [`WithSchedule`] lets it bring a learned
//! prediction's deadline forward, so the battery is full by the scheduled time.

use crate::session::{Session, When};
use crate::Ts;
use jiff::civil::{Time, Weekday};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, Zoned};
use serde::{Deserialize, Serialize};

/// What the predictor needs to know about the present.
#[derive(Debug, Clone)]
pub struct Query<'a> {
    pub now: Ts,
    /// Plug-in time of the current session, if plugged in.
    pub plugged_since: Option<When>,
    pub capacity: Option<u8>,
    /// Finished sessions, oldest first. The schedule ignores this. A learned predictor uses it.
    pub history: &'a [Session],
    pub tz: &'a TimeZone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prediction {
    /// The battery should be full by this time.
    pub full_by: Ts,
    /// 0..=1. The controller only holds charge above a threshold.
    pub confidence: f64,
    /// Human-readable reason, shown in the UI and CLI.
    pub reason: String,
    pub source: &'static str,
}

pub trait Predictor {
    /// `None` means "no opinion": the controller falls back to charging normally.
    fn predict(&self, q: &Query<'_>) -> Option<Prediction>;
}

/// One rule: be full by `time` on these weekdays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRule {
    /// Weekdays this rule applies to, e.g. ["mon","tue","wed","thu","fri"].
    pub days: Vec<Day>,
    /// Local time, "HH:MM".
    pub full_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Day {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Day {
    fn weekday(self) -> Weekday {
        match self {
            Day::Mon => Weekday::Monday,
            Day::Tue => Weekday::Tuesday,
            Day::Wed => Weekday::Wednesday,
            Day::Thu => Weekday::Thursday,
            Day::Fri => Weekday::Friday,
            Day::Sat => Weekday::Saturday,
            Day::Sun => Weekday::Sunday,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("bad time {0:?}, expected HH:MM")]
    BadTime(String),
    #[error("rule has no days")]
    NoDays,
}

/// Manual schedule: the next "full by" time from a list of weekly rules.
#[derive(Debug, Clone, Default)]
pub struct Schedule {
    rules: Vec<(Vec<Weekday>, Time)>,
}

impl Schedule {
    pub fn new(rules: &[ScheduleRule]) -> Result<Self, ScheduleError> {
        let mut out = Vec::new();
        for r in rules {
            if r.days.is_empty() {
                return Err(ScheduleError::NoDays);
            }
            let t: Time = r
                .full_by
                .parse()
                .map_err(|_| ScheduleError::BadTime(r.full_by.clone()))?;
            out.push((r.days.iter().map(|d| d.weekday()).collect(), t));
        }
        Ok(Self { rules: out })
    }

    /// The next deadline strictly after `now`, however far ahead.
    ///
    /// Weekly rules always have a next occurrence within 7 days, so this is
    /// `Some` whenever there's at least one rule. A shorter lookahead would miss
    /// Monday's deadline over the weekend and charge to 100% until then.
    pub fn next_deadline(&self, now: Ts, tz: &TimeZone) -> Option<Ts> {
        let now_z: Zoned = Timestamp::from_second(now).ok()?.to_zoned(tz.clone());
        let mut best: Option<Ts> = None;
        // 0..=7: today's rule may have passed, so the same weekday next week counts too.
        for day_offset in 0..=7i64 {
            let date = now_z.date().checked_add(day_offset.days()).ok()?;
            for (days, t) in &self.rules {
                if !days.contains(&date.weekday()) {
                    continue;
                }
                // DST gaps and folds use jiff's default "compatible" strategy.
                let Ok(z) = date.to_datetime(*t).to_zoned(tz.clone()) else {
                    continue;
                };
                let ts = z.timestamp().as_second();
                if ts > now && best.is_none_or(|b| ts < b) {
                    best = Some(ts);
                }
            }
        }
        best
    }
}

/// A learned prediction, with the schedule as a deadline it must not miss.
///
/// Learning decides whether to hold, and how low: the confidence is the learned
/// one. The schedule only moves the top-off earlier, when its next deadline
/// comes before the learned unplug. With no learned prediction there's no hold,
/// so the battery charges to 100% and is full long before any deadline.
pub struct WithSchedule<'a> {
    pub learned: &'a dyn Predictor,
    pub schedule: &'a Schedule,
}

impl Predictor for WithSchedule<'_> {
    fn predict(&self, q: &Query<'_>) -> Option<Prediction> {
        let mut p = self.learned.predict(q)?;
        if let Some(deadline) = self.schedule.next_deadline(q.now, q.tz) {
            if deadline < p.full_by {
                let local = Timestamp::from_second(deadline)
                    .ok()?
                    .to_zoned(q.tz.clone());
                p.full_by = deadline;
                p.reason = format!(
                    "{}, schedule: full by {}",
                    p.reason,
                    local.strftime("%a %H:%M")
                );
                p.source = "schedule";
            }
        }
        Some(p)
    }
}
