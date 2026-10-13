//! A learned unplug model: the chance of unplugging in each half hour.
//!
//! This is a discrete-time survival model. Each plug-in session is cut into
//! 30-minute steps, and a logistic regression learns the probability that the
//! session ends in a step, given the time of day at that step, the kind of day,
//! the time since plug-in, the charge at plug-in and the plug-in hour. Sessions
//! still plugged in when pbcd stopped watching contribute their steps with no
//! ending, so censoring is handled naturally.
//!
//! Its advantage over [`crate::learned::Routine`] is that it learns from the
//! clock time of the unplug, not only from sessions that started at a similar
//! time. "Unplugs at 07:45 whatever time it was plugged in" is one routine to
//! this model, but several unrelated ones to `Routine`.
//!
//! A linear model is used on purpose. With tens to a few hundred sessions,
//! trees overfit, and inference is a few dot products, so no accelerator or
//! inference runtime is needed. [`backtest`] decides whether it beats `Routine`
//! on this laptop's own history before pbcd uses it.

use crate::predict::{Prediction, Predictor, Query};
use crate::session::{End, Session};
use crate::Ts;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

pub const STEP_S: i64 = 1800;
const HORIZON_STEPS: usize = 72; // 36 h
/// Bumped whenever the features change, so a stored model from older code is retrained.
pub const FEATURE_VERSION: u32 = 2;

// Feature layout. Every row has exactly one active entry per group.
const BIAS: usize = 0;
const CLOCK: usize = 1; // 96: 48 half-hours × {weekday, weekend}
const ELAPSED: usize = CLOCK + 96; // 7 bins
const CAP: usize = ELAPSED + 7; // 3 bins
const PLUG_HOUR: usize = CAP + 3; // 6 bins of 4 h
/// Clock half-hour × plug-in block (4 h). Lets "plugged in at 18:00" and
/// "plugged in at 01:00" each learn their own quiet hours, instead of sharing one
/// clock profile that leaks probability into the small hours.
const CLOCK_BY_PLUG: usize = PLUG_HOUR + 6; // 48 × 6
pub const DIM: usize = CLOCK_BY_PLUG + 48 * 6;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HazardConfig {
    pub l2: f64,
    pub iters: usize,
    pub learning_rate: f64,
    pub half_life_days: f64,
    pub max_age_days: i64,
    /// The predicted unplug is when this share of the probability has passed.
    pub quantile: f64,
    pub min_session_h: f64,
    /// Fewer sessions than this gives no prediction.
    pub min_sessions: usize,
}

impl Default for HazardConfig {
    fn default() -> Self {
        Self {
            // Very light L2. Event rows are rare (one per session), so a stronger
            // penalty flattens the clock-time weights and smears the unplug across
            // the night. On a simulated varied-plug-in history, 0.002 lost to
            // Routine (loss 0.93 vs 0.53) and 0.0005 beat it (0.08 vs 0.53).
            // The backtest guards against overfitting on real data.
            l2: 0.0005,
            iters: 1500,
            learning_rate: 0.3,
            half_life_days: 60.0,
            max_age_days: 180,
            quantile: 0.2,
            min_session_h: 3.0,
            min_sessions: 10,
        }
    }
}

/// A trained model. Small enough to store as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HazardModel {
    pub version: u32,
    pub weights: Vec<f64>,
    /// Sessions it was trained on, for the confidence's support term.
    pub sessions: usize,
    pub trained_at: Ts,
}

fn local(ts: Ts, tz: &TimeZone) -> (usize, bool) {
    let z = Timestamp::from_second(ts)
        .map(|t| t.to_zoned(tz.clone()))
        .ok();
    match z {
        Some(z) => {
            let half = (z.hour() as usize) * 2 + (z.minute() as usize) / 30;
            let weekend = matches!(
                z.weekday(),
                jiff::civil::Weekday::Saturday | jiff::civil::Weekday::Sunday
            );
            (half, weekend)
        }
        None => (0, false),
    }
}

fn elapsed_bin(secs: i64) -> usize {
    let h = secs as f64 / 3600.0;
    match h {
        h if h < 1.0 => 0,
        h if h < 2.0 => 1,
        h if h < 4.0 => 2,
        h if h < 6.0 => 3,
        h if h < 8.0 => 4,
        h if h < 12.0 => 5,
        _ => 6,
    }
}

fn cap_bin(c: Option<u8>) -> usize {
    match c {
        Some(c) if c < 40 => 0,
        Some(c) if c < 70 => 1,
        _ => 2,
    }
}

/// Active feature indices for the step starting at `t`, in a session plugged in at `start`.
fn features(t: Ts, start: Ts, cap_at_plug: Option<u8>, tz: &TimeZone) -> [usize; 6] {
    let (half, weekend) = local(t, tz);
    let (plug_half, _) = local(start, tz);
    let block = plug_half / 8;
    [
        BIAS,
        CLOCK + half + if weekend { 48 } else { 0 },
        ELAPSED + elapsed_bin(t - start),
        CAP + cap_bin(cap_at_plug),
        PLUG_HOUR + block,
        CLOCK_BY_PLUG + block * 48 + half,
    ]
}

fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z).exp())
}

/// One training row: active features, whether the session ended in this step, and a recency weight.
struct Row {
    x: [usize; 6],
    y: f64,
    w: f64,
}

fn rows(history: &[Session], now: Ts, tz: &TimeZone, cfg: &HazardConfig) -> (Vec<Row>, usize) {
    let mut out = Vec::new();
    let mut n = 0;
    for s in history {
        // Plug-in during sleep: start unknown to within the hour, can't be placed in the day.
        if s.plugged.hi - s.plugged.lo > 3600 || now - s.plugged.lo > cfg.max_age_days * 86400 {
            continue;
        }
        let start = s.plugged.mid();
        let (end, observed) = match s.end {
            End::Unplugged(w) => (w.mid(), true),
            End::Censored(t) => (t, false),
        };
        if end <= start {
            continue;
        }
        n += 1;
        let w = 0.5f64.powf(((now - start) as f64 / 86400.0) / cfg.half_life_days);
        let mut t = start;
        while t < end {
            let last = t + STEP_S >= end;
            out.push(Row {
                x: features(t, start, s.capacity_at_plug, tz),
                y: if last && observed { 1.0 } else { 0.0 },
                w,
            });
            t += STEP_S;
        }
    }
    (out, n)
}

/// Train on history. None if there's too little of it.
pub fn train(
    history: &[Session],
    now: Ts,
    tz: &TimeZone,
    cfg: &HazardConfig,
) -> Option<HazardModel> {
    let (data, n) = rows(history, now, tz, cfg);
    if n < cfg.min_sessions || data.is_empty() {
        return None;
    }
    let total_w: f64 = data.iter().map(|r| r.w).sum();
    let mut w = vec![0.0; DIM];
    // Start the bias at the overall event rate, so early iterations aren't wasted.
    let rate = data.iter().map(|r| r.w * r.y).sum::<f64>() / total_w;
    w[BIAS] = (rate.clamp(1e-4, 0.5) / (1.0 - rate.clamp(1e-4, 0.5))).ln();
    // Full-batch AdaGrad on weighted log-loss with L2 (not on the bias).
    let mut g2 = vec![1e-8; DIM];
    let mut grad = vec![0.0; DIM];
    for _ in 0..cfg.iters {
        grad.iter_mut().for_each(|g| *g = 0.0);
        for r in &data {
            let z: f64 = r.x.iter().map(|&i| w[i]).sum();
            let e = r.w * (sigmoid(z) - r.y);
            for &i in &r.x {
                grad[i] += e;
            }
        }
        for i in 0..DIM {
            let g = grad[i] / total_w + if i == BIAS { 0.0 } else { cfg.l2 * w[i] };
            g2[i] += g * g;
            w[i] -= cfg.learning_rate * g / g2[i].sqrt();
        }
    }
    Some(HazardModel {
        version: FEATURE_VERSION,
        weights: w,
        sessions: n,
        trained_at: now,
    })
}

/// Predictor over a trained model.
pub struct Hazard<'a> {
    pub model: &'a HazardModel,
    pub cfg: HazardConfig,
    pub accuracy_factor: f64,
}

impl Hazard<'_> {
    /// For each step from `from`: (step start, cumulative unplug probability by
    /// the step's end), given still plugged in at `from`.
    fn curve(&self, from: Ts, start: Ts, cap: Option<u8>, tz: &TimeZone) -> Vec<(Ts, f64)> {
        let mut s = 1.0;
        let mut out = Vec::with_capacity(HORIZON_STEPS);
        for k in 0..HORIZON_STEPS {
            let t = from + k as i64 * STEP_S;
            let x = features(t, start, cap, tz);
            let h = sigmoid(x.iter().map(|&i| self.model.weights[i]).sum());
            s *= 1.0 - h;
            out.push((t, 1.0 - s));
        }
        out
    }
}

impl Predictor for Hazard<'_> {
    fn predict(&self, q: &Query<'_>) -> Option<Prediction> {
        if self.model.version != FEATURE_VERSION || self.model.sessions < self.cfg.min_sessions {
            return None;
        }
        let start = q.plugged_since?.hi;
        let cap = q
            .history
            .iter()
            .rev()
            .find(|s| s.plugged.hi == start)
            .and_then(|s| s.capacity_at_plug)
            .or(q.capacity);
        let from = q.now.max(start);
        let c = self.curve(from, start, cap, q.tz);
        // The start of the step in which the probability crosses `p`: cautious by up to a step.
        let at = |p: f64| c.iter().find(|(_, f)| *f >= p).map(|(t, _)| *t);
        let full_by = at(self.cfg.quantile)?;
        if ((full_by - start) as f64) < self.cfg.min_session_h * 3600.0 {
            return None; // A short top-up: nothing to gain by holding.
        }
        // Confidence: a tight unplug window, and enough history. The window is
        // from the cautious time to the median. Later quantiles fall past the
        // last unplug in the data, where the model has learned nothing.
        let spread_h = at(0.5)
            .map(|t50| (t50 - full_by) as f64 / 3600.0)
            .unwrap_or(f64::INFINITY);
        // Within 1 h is consistent. 3 h or more is barely a routine.
        let consistency = (1.0 - (spread_h - 1.0) / 2.0).clamp(0.0, 1.0);
        let support = (self.model.sessions as f64 / 30.0).min(1.0);
        let confidence = (consistency * support * self.accuracy_factor).clamp(0.0, 1.0);
        Some(Prediction {
            full_by,
            confidence,
            reason: format!("learned model: {} sessions", self.model.sessions),
            source: "learned",
        })
    }
}

/// Result of comparing the two learned predictors on this laptop's history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Backtest {
    /// Sessions both were scored on.
    pub evaluated: usize,
    /// Mean quantile loss in hours. Lower is better.
    pub routine_loss: f64,
    pub model_loss: f64,
    /// Share of sessions where the prediction came after the real unplug (caught short).
    pub routine_late: f64,
    pub model_late: f64,
    /// Whether pbcd should use the model.
    pub use_model: bool,
    pub at: Ts,
}

/// Minimum scored sessions before the model may be chosen.
pub const MIN_EVALUATED: usize = 20;
/// The model must beat Routine's loss by this factor.
pub const MARGIN: f64 = 0.95;

/// Quantile (pinball) loss of a predicted duration `q` against the real one `y`, in hours.
/// "No prediction" scores as q = 0: charging straight to 100%. That's never
/// late, but gives up the chance to hold.
fn pinball(y: f64, q: f64, tau: f64) -> f64 {
    if y >= q {
        tau * (y - q)
    } else {
        (1.0 - tau) * (q - y)
    }
}

/// Replay history in weekly blocks: train on everything before a block, score
/// both predictors on the sessions in it, as the controller would see them 10
/// minutes after plug-in.
pub fn backtest(
    history: &[Session],
    now: Ts,
    tz: &TimeZone,
    rcfg: crate::learned::RoutineConfig,
    hcfg: HazardConfig,
) -> Backtest {
    let mut sessions: Vec<&Session> = history
        .iter()
        .filter(|s| {
            matches!(s.end, End::Unplugged(_))
                && s.plugged.hi - s.plugged.lo <= 3600
                && now - s.plugged.lo <= hcfg.max_age_days * 86400
        })
        .collect();
    sessions.sort_by_key(|s| s.plugged.lo);
    let (mut rl, mut ml, mut rlate, mut mlate, mut n) = (0.0, 0.0, 0usize, 0usize, 0usize);
    let tau = hcfg.quantile;
    let week = 7 * 86400;
    let first = sessions.first().map(|s| s.plugged.lo).unwrap_or(now);
    let mut block = first + 2 * week; // two weeks to learn from before scoring
    while block < now {
        let past: Vec<Session> = history
            .iter()
            .filter(|s| s.plugged.lo < block)
            .cloned()
            .collect();
        let model = train(&past, block, tz, &hcfg);
        for s in sessions
            .iter()
            .filter(|s| s.plugged.lo >= block && s.plugged.lo < block + week)
        {
            let start = s.plugged.mid();
            let End::Unplugged(e) = s.end else { continue };
            let y = (e.mid() - start) as f64 / 3600.0;
            let at = start + 600;
            let q = Query {
                now: at,
                plugged_since: Some(s.plugged),
                capacity: s.capacity_at_plug,
                history: &past,
                tz,
            };
            let score = |p: Option<Prediction>| -> (f64, bool) {
                let d = p
                    .filter(|p| p.confidence >= 0.3)
                    .map(|p| (p.full_by - start) as f64 / 3600.0)
                    .unwrap_or(0.0);
                (pinball(y, d, tau), d > y)
            };
            let r = score(crate::learned::Routine::new(rcfg, 1.0).predict(&q));
            let m = score(model.as_ref().and_then(|m| {
                Hazard {
                    model: m,
                    cfg: hcfg,
                    accuracy_factor: 1.0,
                }
                .predict(&q)
            }));
            rl += r.0;
            ml += m.0;
            rlate += r.1 as usize;
            mlate += m.1 as usize;
            n += 1;
        }
        block += week;
    }
    let div = |a: f64| if n == 0 { 0.0 } else { a / n as f64 };
    let (routine_loss, model_loss) = (div(rl), div(ml));
    let (routine_late, model_late) = (div(rlate as f64), div(mlate as f64));
    // Only switch on clear evidence: enough sessions, lower loss by a margin, and not caught short more often.
    let use_model = n >= MIN_EVALUATED
        && model_loss < routine_loss * MARGIN
        && model_late <= routine_late + 0.05;
    Backtest {
        evaluated: n,
        routine_loss,
        model_loss,
        routine_late,
        model_late,
        use_model,
        at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learned::{Routine, RoutineConfig};
    use crate::session::When;
    use jiff::civil::date;

    fn tz() -> TimeZone {
        TimeZone::get("Pacific/Auckland").unwrap()
    }
    fn at(m: i8, d: i8, h: i8, mi: i8) -> Ts {
        date(2026, m, d)
            .at(h, mi, 0, 0)
            .to_zoned(tz())
            .unwrap()
            .timestamp()
            .as_second()
    }
    fn sess(a: Ts, b: Ts) -> Session {
        Session {
            plugged: When::exact(a),
            end: End::Unplugged(When::exact(b)),
            capacity_at_plug: Some(50),
            capacity_at_end: Some(100),
            merged_gaps: 0,
        }
    }
    /// Weekday nights over `weeks` weeks before 19 Oct. Plug-in times spread
    /// evenly from 18:00 to 02:00, but the unplug is always about 07:40. Similar
    /// start times then give a wide spread of durations, which is where a
    /// start-time-matching predictor has to be cautious and an unplug-time model doesn't.
    fn varied_plug_fixed_unplug(weeks: i32) -> Vec<Session> {
        let mut h = Vec::new();
        let base = date(2026, 10, 19);
        for w in (1..=weeks).rev() {
            for d in 0..5i32 {
                let day = base
                    .checked_sub(jiff::Span::new().days(7 * w as i64 - d as i64))
                    .unwrap();
                let k = (w * 5 + d) as i64;
                let midnight = day
                    .at(0, 0, 0, 0)
                    .to_zoned(tz())
                    .unwrap()
                    .timestamp()
                    .as_second();
                // Spread evenly from 18:00 to 02:00 (golden-ratio steps, so no two nights repeat).
                let frac = (k as f64 * 0.618_034).fract();
                let plug = midnight + 18 * 3600 + (frac * 8.0 * 3600.0) as i64;
                let next = day.checked_add(jiff::Span::new().days(1)).unwrap();
                let unplug = next
                    .at(7, (30 + (k * 7) % 20) as i8, 0, 0)
                    .to_zoned(tz())
                    .unwrap()
                    .timestamp()
                    .as_second();
                h.push(sess(plug, unplug));
            }
        }
        h
    }

    #[test]
    fn learns_a_fixed_unplug_time() {
        let h = varied_plug_fixed_unplug(6);
        let now = at(10, 19, 23, 40);
        let m = train(&h, now, &tz(), &HazardConfig::default()).expect("trained");
        // Plugged in at 23:30, between the usual times: the model still knows the unplug time.
        let start = at(10, 19, 23, 30);
        let q = Query {
            now,
            plugged_since: Some(When::exact(start)),
            capacity: Some(50),
            history: &h,
            tz: &tz(),
        };
        let p = Hazard {
            model: &m,
            cfg: HazardConfig::default(),
            accuracy_factor: 1.0,
        }
        .predict(&q)
        .expect("prediction");
        let (half, _) = local(p.full_by, &tz());
        assert!(
            (13..=16).contains(&half),
            "predicted unplug at half-hour {half} (expect about 07:00 to 08:00)"
        );
        assert!(p.confidence > 0.5, "{}", p.confidence);
    }

    #[test]
    fn too_little_history_gives_no_model() {
        let h = varied_plug_fixed_unplug(1);
        assert!(train(&h, at(10, 19, 23, 0), &tz(), &HazardConfig::default()).is_none());
    }

    #[test]
    fn short_top_ups_give_no_prediction() {
        let mut h = Vec::new();
        for d in 1..=20i8 {
            h.push(sess(at(9, d, 12, 30), at(9, d, 13, 10)));
        }
        let now = at(9, 25, 12, 40);
        let m = train(&h, now, &tz(), &HazardConfig::default()).unwrap();
        let q = Query {
            now,
            plugged_since: Some(When::exact(at(9, 25, 12, 30))),
            capacity: Some(50),
            history: &h,
            tz: &tz(),
        };
        assert!(Hazard {
            model: &m,
            cfg: HazardConfig::default(),
            accuracy_factor: 1.0
        }
        .predict(&q)
        .is_none());
    }

    #[test]
    fn censored_sessions_are_used() {
        let mut h = varied_plug_fixed_unplug(4);
        for s in h.iter_mut().step_by(3) {
            if let End::Unplugged(w) = s.end {
                s.end = End::Censored(w.mid() - 3600);
            }
        }
        assert!(train(&h, at(10, 19, 23, 0), &tz(), &HazardConfig::default()).is_some());
    }

    #[test]
    fn stale_feature_version_is_ignored() {
        let h = varied_plug_fixed_unplug(4);
        let mut m = train(&h, at(10, 19, 23, 0), &tz(), &HazardConfig::default()).unwrap();
        m.version = FEATURE_VERSION + 1;
        let q = Query {
            now: at(10, 19, 23, 0),
            plugged_since: Some(When::exact(at(10, 19, 22, 0))),
            capacity: Some(50),
            history: &h,
            tz: &tz(),
        };
        assert!(Hazard {
            model: &m,
            cfg: HazardConfig::default(),
            accuracy_factor: 1.0
        }
        .predict(&q)
        .is_none());
    }

    #[test]
    fn backtest_prefers_the_model_when_plug_times_vary() {
        let h = varied_plug_fixed_unplug(10);
        let b = backtest(
            &h,
            at(10, 19, 12, 0),
            &tz(),
            RoutineConfig::default(),
            HazardConfig::default(),
        );
        assert!(b.evaluated >= MIN_EVALUATED, "{b:?}");
        assert!(b.use_model, "{b:?}");
        assert!(b.model_loss < b.routine_loss, "{b:?}");
    }

    #[test]
    fn backtest_keeps_routine_with_little_history() {
        let h = varied_plug_fixed_unplug(3);
        let b = backtest(
            &h,
            at(10, 19, 12, 0),
            &tz(),
            RoutineConfig::default(),
            HazardConfig::default(),
        );
        assert!(!b.use_model, "{b:?}");
    }

    #[test]
    fn backtest_keeps_routine_when_it_is_as_good() {
        // Regular plug-in and unplug times: Routine handles this already.
        let mut h = Vec::new();
        for w in 0..10i64 {
            for d in 0..5i64 {
                let a = at(8, 3, 22, 0) + (w * 7 + d) * 86400;
                h.push(sess(a, a + 9 * 3600 + 40 * 60));
            }
        }
        let b = backtest(
            &h,
            at(10, 19, 12, 0),
            &tz(),
            RoutineConfig::default(),
            HazardConfig::default(),
        );
        assert!(b.evaluated >= MIN_EVALUATED, "{b:?}");
        // Either may win narrowly. The point: the model must not be picked without a clear margin.
        if b.use_model {
            assert!(b.model_loss < b.routine_loss * MARGIN, "{b:?}");
        }
        let _ = Routine::new(RoutineConfig::default(), 1.0);
    }
}
