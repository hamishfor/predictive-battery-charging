//! Predictions learned from this laptop's own plug-in history.
//!
//! - [`Routine`] predicts when the current plug-in session will end, from past
//!   sessions that started at a similar time on a similar kind of day.
//! - [`Accuracy`] tracks whether past predictions were right, and lowers
//!   confidence when they weren't.
//!
//! Everything here is cheap to recompute, so it runs on every decision with no
//! training step. With little history confidence is low, and the controller
//! holds high or not at all.

use crate::predict::{Prediction, Predictor, Query};
use crate::session::End;
use crate::stats::{consistency, decay, local_minute, placeable, HOUR};
use crate::Ts;
use serde::{Deserialize, Serialize};

/// Tunables. The defaults are deliberately cautious.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoutineConfig {
    /// Past sessions starting within this many minutes of the current one's
    /// start time (on a similar day) count as the same routine.
    pub window_min: i64,
    /// Weight halves for every this many days of age.
    pub half_life_days: f64,
    /// Fewer similar sessions than this gives no prediction.
    pub min_similar: usize,
    /// The predicted unplug time is the time by which only this share of
    /// similar sessions had ended. Low values are cautious: full early rather than late.
    pub quantile: f64,
    /// Sessions shorter than this are top-ups, not routines worth holding for.
    pub min_session_h: f64,
    /// Ignore history older than this.
    pub max_age_days: i64,
}

impl Default for RoutineConfig {
    /// About six months of history. A 90-day half-life keeps last season's
    /// routines in play, while a new job or term takes over within about a month.
    /// A reset re-learns at once.
    fn default() -> Self {
        Self {
            window_min: 90,
            half_life_days: 90.0,
            min_similar: 5,
            quantile: 0.2,
            min_session_h: 3.0,
            max_age_days: 180,
        }
    }
}

/// Circular distance between two minute-of-day values.
fn minute_gap(a: i64, b: i64) -> i64 {
    let d = (a - b).rem_euclid(1440);
    d.min(1440 - d)
}

/// One past session, reduced to what the predictor needs.
#[derive(Debug, Clone, Copy)]
struct Past {
    /// Duration in seconds, as a best estimate (midpoints for imprecise times).
    dur: i64,
    /// Ended by an observed unplug. Censored sessions only give a lower bound.
    observed: bool,
    weight: f64,
}

/// Unplug-time predictor learned from history.
#[derive(Debug, Clone, Default)]
pub struct Routine {
    pub cfg: RoutineConfig,
    /// Multiplies confidence. 1.0 with no track record. Lower after misses.
    pub accuracy_factor: f64,
}

impl Routine {
    pub fn new(cfg: RoutineConfig, accuracy_factor: f64) -> Self {
        Self {
            cfg,
            accuracy_factor,
        }
    }

    fn similar(&self, q: &Query<'_>, start: Ts) -> Vec<Past> {
        let Some((m0, k0)) = local_minute(start, q.tz) else {
            return Vec::new();
        };
        q.history
            .iter()
            .filter(|s| placeable(s, q.now, self.cfg.max_age_days))
            .filter_map(|s| {
                let st = s.plugged.mid();
                let (m, k) = local_minute(st, q.tz)?;
                (k == k0 && minute_gap(m, m0) <= self.cfg.window_min).then(|| Past {
                    dur: s.duration(),
                    observed: matches!(s.end, End::Unplugged(_)),
                    weight: decay(q.now - st, self.cfg.half_life_days),
                })
            })
            .collect()
    }
}

/// Weighted product-limit (Kaplan-Meier) estimate of how long sessions last.
/// Censored sessions (still plugged in when we stopped watching) count as
/// "lasted at least this long" instead of being thrown away.
/// Returns (duration, survival) steps, survival falling from 1.0.
fn survival(mut p: Vec<Past>) -> Vec<(i64, f64)> {
    p.sort_by_key(|x| x.dur);
    let mut at_risk: f64 = p.iter().map(|x| x.weight).sum();
    let mut s = 1.0;
    let mut steps = Vec::new();
    for x in &p {
        if x.observed && at_risk > 0.0 {
            s *= 1.0 - x.weight / at_risk;
            steps.push((x.dur, s));
        }
        at_risk -= x.weight;
    }
    steps
}

/// Shortest duration by which at least `q` of sessions had ended. None if the
/// estimate never gets that far (too many sessions still running).
fn quantile(steps: &[(i64, f64)], q: f64) -> Option<i64> {
    steps.iter().find(|(_, s)| 1.0 - s >= q).map(|(d, _)| *d)
}

impl Predictor for Routine {
    fn predict(&self, q: &Query<'_>) -> Option<Prediction> {
        let since = q.plugged_since?;
        // A plug-in during sleep has no known start. Use the latest possible one: cautious.
        let start = since.hi;
        let sim = self.similar(q, start);
        if sim.len() < self.cfg.min_similar {
            return None;
        }
        let long: Vec<Past> = sim
            .iter()
            .copied()
            .filter(|p| p.dur as f64 >= self.cfg.min_session_h * HOUR as f64)
            .collect();
        let steps = survival(sim.clone());
        let dur = quantile(&steps, self.cfg.quantile)?;
        if (dur as f64) < self.cfg.min_session_h * HOUR as f64 {
            return None; // This routine is short top-ups: nothing to gain by holding.
        }
        let full_by = start + dur;
        if full_by <= q.now {
            return None; // Already past the cautious unplug time: charge.
        }
        // Confidence: enough history, and a consistent end time.
        let weight: f64 = sim.iter().map(|p| p.weight).sum();
        let support = (weight / (self.cfg.min_similar as f64 * 1.5)).min(1.0);
        let ends: Vec<i64> = long.iter().filter(|p| p.observed).map(|p| p.dur).collect();
        let spread_h = if ends.len() >= 2 {
            let mean = ends.iter().sum::<i64>() as f64 / ends.len() as f64;
            (ends.iter().map(|d| (*d as f64 - mean).powi(2)).sum::<f64>() / ends.len() as f64)
                .sqrt()
                / HOUR as f64
        } else {
            f64::INFINITY
        };
        let share_long = long.len() as f64 / sim.len() as f64;
        let confidence =
            (support * consistency(spread_h) * share_long * self.accuracy_factor).clamp(0.0, 1.0);
        Some(Prediction {
            full_by,
            confidence,
            reason: format!("learned: {} similar sessions", sim.len()),
            source: "learned",
        })
    }
}

/// Prefer the first predictor that has an opinion.
pub struct Chain<'a>(pub Vec<&'a dyn Predictor>);

impl Predictor for Chain<'_> {
    fn predict(&self, q: &Query<'_>) -> Option<Prediction> {
        self.0.iter().find_map(|p| p.predict(q))
    }
}

/// Running record of how good past predictions were, weighted towards recent sessions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Accuracy {
    /// Sessions that held for a prediction and were at their target when unplugged.
    pub hits: f64,
    /// Sessions that held for a prediction, were unplugged short of their target,
    /// and then ran on battery before the next plug-in.
    pub misses: f64,
}

/// Each judged session scales the earlier record by this: a memory of about the last 18.
/// A changed routine then shows within a week of sessions, however long the old record.
const ACCURACY_KEEP: f64 = 0.945;

impl Accuracy {
    pub fn record(&mut self, reached_target: bool) {
        self.hits *= ACCURACY_KEEP;
        self.misses *= ACCURACY_KEEP;
        // Plain counts stored by older versions: shrink to the steady-state total, same ratio.
        let n = self.hits + self.misses;
        let cap = 1.0 / (1.0 - ACCURACY_KEEP) - 1.0;
        if n > cap {
            self.hits *= cap / n;
            self.misses *= cap / n;
        }
        if reached_target {
            self.hits += 1.0;
        } else {
            self.misses += 1.0;
        }
    }

    /// Confidence multiplier. 1.0 with no record. Each miss weighs three hits,
    /// because ending short is the failure users notice. From a clean record,
    /// misses in a row give 0.89, 0.79, 0.69, 0.59, then 0.51 at five: a week of
    /// misses moves a certain prediction's hold from 70% to 90%.
    pub fn factor(&self) -> f64 {
        let n = self.hits + self.misses;
        if n <= 0.0 {
            return 1.0;
        }
        let score = (self.hits - 3.0 * self.misses) / n;
        (0.5 + 0.5 * score).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, When};
    use jiff::civil::date;
    use jiff::tz::TimeZone;

    const H: i64 = 3600;
    fn tz() -> TimeZone {
        TimeZone::get("Pacific/Auckland").unwrap()
    }
    fn at(d: i8, h: i8, m: i8) -> Ts {
        date(2026, 10, d)
            .at(h, m, 0, 0)
            .to_zoned(tz())
            .unwrap()
            .timestamp()
            .as_second()
    }
    fn sess(start: Ts, end: Ts, cp: u8, ce: u8) -> Session {
        Session {
            plugged: When::exact(start),
            end: End::Unplugged(When::exact(end)),
            capacity_at_plug: Some(cp),
            capacity_at_end: Some(ce),
            merged_gaps: 0,
        }
    }
    fn query<'a>(now: Ts, since: Ts, h: &'a [Session], tz: &'a TimeZone) -> Query<'a> {
        Query {
            now,
            plugged_since: Some(When::exact(since)),
            capacity: Some(60),
            history: h,
            tz,
        }
    }

    /// Weeknights: plugged in about 22:00, unplugged about 07:45.
    fn nights(days: &[i8]) -> Vec<Session> {
        days.iter()
            .enumerate()
            .map(|(i, &d)| {
                sess(
                    at(d, 22, (i as i8 * 7) % 20),
                    at(d + 1, 7, 35 + (i as i8 * 3) % 20),
                    50,
                    100,
                )
            })
            .collect()
    }

    #[test]
    fn learns_a_regular_overnight_routine() {
        let tz = tz();
        // Mon 5 to Fri 9, then Mon 12 to Thu 15 (weekday nights).
        let h = nights(&[5, 6, 7, 8, 12, 13, 14, 15]);
        let start = at(19, 22, 5); // Mon 19 Oct
        let p = Routine::new(RoutineConfig::default(), 1.0)
            .predict(&query(start + 600, start, &h, &tz))
            .expect("prediction");
        let (hh, mm) = local_minute(p.full_by, &tz)
            .map(|(m, _)| (m / 60, m % 60))
            .unwrap();
        assert!((7..=8).contains(&hh), "predicted unplug {hh}:{mm:02}");
        assert!(p.confidence >= 0.6, "confident: {}", p.confidence);
        assert_eq!(p.source, "learned");
    }

    #[test]
    fn too_little_history_gives_no_prediction() {
        let tz = tz();
        let h = nights(&[5, 6, 7]);
        let start = at(12, 22, 0);
        assert!(Routine::new(RoutineConfig::default(), 1.0)
            .predict(&query(start, start, &h, &tz))
            .is_none());
    }

    #[test]
    fn short_top_ups_are_not_held() {
        let tz = tz();
        // Weekday lunchtime top-ups of 30 to 50 min.
        let h: Vec<Session> = [5, 6, 7, 8, 9, 12, 13]
            .iter()
            .map(|&d| sess(at(d, 12, 30), at(d, 13, 10), 40, 80))
            .collect();
        let start = at(14, 12, 35);
        assert!(Routine::new(RoutineConfig::default(), 1.0)
            .predict(&query(start, start, &h, &tz))
            .is_none());
    }

    #[test]
    fn irregular_routine_has_low_confidence() {
        let tz = tz();
        // Same plug-in time, but unplug anywhere from 01:00 to 11:00.
        let ends = [
            (1, 0),
            (11, 0),
            (4, 0),
            (9, 30),
            (2, 0),
            (10, 30),
            (6, 0),
            (3, 0),
        ];
        let days = [5, 6, 7, 8, 12, 13, 14, 15];
        let h: Vec<Session> = days
            .iter()
            .zip(ends)
            .map(|(&d, (eh, em))| sess(at(d, 22, 0), at(d + 1, eh, em), 50, 100))
            .collect();
        let start = at(19, 22, 0);
        let c = Routine::new(RoutineConfig::default(), 1.0)
            .predict(&query(start, start, &h, &tz))
            .map(|p| p.confidence)
            .unwrap_or(0.0);
        assert!(c < 0.6, "should not be confident: {c}");
    }

    #[test]
    fn weekend_history_does_not_predict_weekdays() {
        let tz = tz();
        // Sat/Sun nights only.
        let h = nights(&[3, 4, 10, 11, 17, 18]);
        let start = at(19, 22, 0); // Monday
        assert!(Routine::new(RoutineConfig::default(), 1.0)
            .predict(&query(start, start, &h, &tz))
            .is_none());
    }

    #[test]
    fn misses_reduce_confidence() {
        let tz = tz();
        let h = nights(&[5, 6, 7, 8, 12, 13, 14, 15]);
        let start = at(19, 22, 5);
        let mut acc = Accuracy::default();
        acc.record(true);
        acc.record(false);
        acc.record(false);
        let p = Routine::new(RoutineConfig::default(), acc.factor())
            .predict(&query(start, start, &h, &tz));
        assert!(p.as_ref().is_none_or(|p| p.confidence < 0.6), "{p:?}");
    }

    #[test]
    fn censored_sessions_count_as_lower_bounds() {
        // Ten nights, five of them still plugged in when the daemon stopped (shutdown on AC).
        let steps = survival(
            (0..10)
                .map(|i| Past {
                    dur: (8 + (i % 3)) * H,
                    observed: i % 2 == 0,
                    weight: 1.0,
                })
                .collect(),
        );
        let q20 = quantile(&steps, 0.2).unwrap();
        assert!((8 * H..=10 * H).contains(&q20), "{q20}");
    }

    #[test]
    fn chain_prefers_the_first_with_an_opinion() {
        struct Fixed(i64, &'static str);
        impl Predictor for Fixed {
            fn predict(&self, _: &Query<'_>) -> Option<Prediction> {
                Some(Prediction {
                    full_by: self.0,
                    confidence: 1.0,
                    reason: String::new(),
                    source: self.1,
                })
            }
        }
        struct Never;
        impl Predictor for Never {
            fn predict(&self, _: &Query<'_>) -> Option<Prediction> {
                None
            }
        }
        let tz = tz();
        let q = query(0, 0, &[], &tz);
        assert_eq!(
            Chain(vec![&Fixed(1, "model"), &Fixed(2, "learned")])
                .predict(&q)
                .unwrap()
                .source,
            "model"
        );
        assert_eq!(
            Chain(vec![&Never, &Fixed(2, "learned")])
                .predict(&q)
                .unwrap()
                .source,
            "learned"
        );
    }

    #[test]
    fn accuracy_factor() {
        let mut a = Accuracy::default();
        assert_eq!(a.factor(), 1.0);
        for _ in 0..9 {
            a.record(true);
        }
        a.record(false);
        assert!(a.factor() > 0.7 && a.factor() < 0.8, "{}", a.factor());
        for _ in 0..3 {
            a.record(false);
        }
        assert!(a.factor() < 0.5, "{}", a.factor());
    }

    #[test]
    fn a_long_record_does_not_hide_new_misses() {
        let mut a = Accuracy::default();
        for _ in 0..200 {
            a.record(true);
        }
        for _ in 0..3 {
            a.record(false);
        }
        assert!(a.factor() > 0.6, "3 misses: {}", a.factor());
        for _ in 0..2 {
            a.record(false);
        }
        let f = a.factor();
        assert!((0.45..0.55).contains(&f), "a week of misses: {f}");
        // And recovers once predictions hold up again.
        for _ in 0..15 {
            a.record(true);
        }
        assert!(a.factor() > 0.75, "{}", a.factor());
    }

    #[test]
    fn old_integer_counts_still_load() {
        let mut a: Accuracy = serde_json::from_str(r#"{"hits":100,"misses":0}"#).unwrap();
        assert_eq!(a.factor(), 1.0);
        for _ in 0..5 {
            a.record(false);
        }
        assert!(a.factor() < 0.55, "{}", a.factor());
    }
}
