//! Hourly charge history for the extension's chart.

use pbc_model::charge::ChargeModel;
use pbc_model::Ts;
use pbc_store::Sample;
use serde::Serialize;

pub const HOUR: Ts = 3600;
/// The plug strip's resolution.
const QUARTER: Ts = HOUR / 4;

/// Resolution of the gap simulation. A step of charging is about 1%.
const STEP: Ts = 60;
/// Longer gaps are interpolated straight, with no plug state. Searching for a plug
/// change is quadratic in the gap's steps.
const MAX_SIM_S: Ts = 3 * 86400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Power {
    Charging,
    Held,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hour {
    /// Start of the hour.
    pub ts: Ts,
    /// Last capacity seen in the hour, or estimated for the hour's end (see `estimated`).
    /// None only before the first sample.
    pub capacity: Option<u8>,
    /// No samples this hour. `capacity` is from simulating the gap.
    pub estimated: bool,
    /// Per quarter hour: charging if it charged at all, else held if plugged in for most
    /// of it, else None (on battery). Simulated for quarters with no samples.
    pub power: [Option<Power>; 4],
}

/// What the gap simulation assumes about this machine.
pub struct Rates {
    /// While unobserved on battery (asleep or off), %/h.
    pub drain_pct_per_h: f64,
    pub charge: ChargeModel,
}

/// `hours` buckets ending with the one containing `now`, oldest first. `samples` sorted by ts.
/// `before` is the last sample with a capacity before the window, so a gap that starts
/// before it can be filled.
pub fn hourly(
    before: Option<&Sample>,
    samples: &[Sample],
    now: Ts,
    hours: u32,
    rates: &Rates,
) -> Vec<Hour> {
    let last = now.div_euclid(HOUR) * HOUR;
    let first = last - (hours as Ts - 1) * HOUR;
    let mut out: Vec<Hour> = (0..hours as Ts)
        .map(|i| Hour {
            ts: first + i * HOUR,
            capacity: None,
            estimated: false,
            power: [None; 4],
        })
        .collect();
    // [charging, held, battery] counts per quarter hour.
    let mut counts = vec![[0u32; 3]; out.len() * 4];
    for s in samples {
        if s.ts < first || s.ts >= last + HOUR {
            continue;
        }
        let i = ((s.ts - first) / HOUR) as usize;
        if s.capacity.is_some() {
            out[i].capacity = s.capacity;
        }
        let k = match (s.online, charging(s)) {
            (false, _) => 2,
            (true, true) => 0,
            (true, false) => 1,
        };
        counts[((s.ts - first) / QUARTER) as usize][k] += 1;
    }
    for (q, c) in counts.iter().enumerate() {
        out[q / 4].power[q % 4] = plug_state(c);
    }
    let mut points = before
        .into_iter()
        .chain(samples)
        .filter(|s| s.capacity.is_some());
    if let Some(mut prev) = points.next() {
        for s in points {
            fill(&mut out, &counts, first, prev, s, rates);
            prev = s;
        }
    }
    out
}

fn charging(s: &Sample) -> bool {
    s.status.as_deref() == Some("Charging")
}

/// From [charging, held, battery] counts. Any charging shows, so a short top-off from a
/// hold level doesn't vanish into the hold around it.
fn plug_state(c: &[u32; 3]) -> Option<Power> {
    if c[0] > 0 {
        Some(Power::Charging)
    } else {
        (c[1] > c[2]).then_some(Power::Held)
    }
}

/// Fill the empty hours and quarters strictly between consecutive samples `a` and `b`.
fn fill(out: &mut [Hour], counts: &[[u32; 3]], first: Ts, a: &Sample, b: &Sample, rates: &Rates) {
    let (Some(ca), Some(cb)) = (a.capacity, b.capacity) else {
        return;
    };
    let index = |t: Ts, len: Ts| (t - first).div_euclid(len);
    let hours = (index(a.ts, HOUR) + 1).max(0)..index(b.ts, HOUR).min(out.len() as Ts);
    let quarters = (index(a.ts, QUARTER) + 1).max(0)..index(b.ts, QUARTER).min(counts.len() as Ts);
    if quarters.is_empty() {
        return;
    }
    let span = (b.ts - a.ts) as f64;
    let path = (b.ts - a.ts <= MAX_SIM_S).then(|| Path::best(a, b, rates));
    for i in hours {
        let h = &mut out[i as usize];
        let end = h.ts + HOUR;
        let f = (end - a.ts) as f64 / span;
        let c = match &path {
            // Spread the miss at `b` across the gap, so both ends meet the samples.
            Some(p) => p.at(a.ts, end) - p.miss * f,
            None => ca as f64 + (cb as f64 - ca as f64) * f,
        };
        h.capacity = Some(c.round().clamp(0.0, 100.0) as u8);
        h.estimated = true;
    }
    for q in quarters {
        // Samples without a capacity can still say what the plug was doing.
        if counts[q as usize].iter().sum::<u32>() > 0 {
            continue;
        }
        let q0 = first + q * QUARTER;
        out[q as usize / 4].power[q as usize % 4] =
            path.as_ref().and_then(|p| p.power(a.ts, q0, q0 + QUARTER));
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Battery,
    Charging,
    Held,
}

/// Simulated charge across a gap, one entry per `STEP`.
struct Path {
    /// `levels[k]` at `a.ts + k * STEP`, from `a`'s capacity.
    levels: Vec<f64>,
    /// `states[k]` during step k.
    states: Vec<State>,
    /// Simulated minus actual capacity at `b`.
    miss: f64,
}

impl Path {
    /// The plug state at each edge is known. A change in between happened at an unknown
    /// time: take the one whose simulation best matches `b`.
    fn best(a: &Sample, b: &Sample, rates: &Rates) -> Self {
        let n = ((b.ts - a.ts + STEP - 1) / STEP) as usize;
        let (ca, cb) = (
            a.capacity.unwrap_or(0) as f64,
            b.capacity.unwrap_or(0) as f64,
        );
        let limit = |s: &Sample| s.th_end.unwrap_or(100) as f64;
        // Where charging stopped at `b`: its level if held there, else it was still going.
        let last = if b.online && charging(b) {
            limit(b)
        } else {
            cb
        };
        let run = |plugged: &dyn Fn(usize) -> bool, cap: &dyn Fn(usize) -> f64| {
            Self::simulate(a, n, cb, plugged, cap, rates)
        };
        // Over switch times 0..=n. `late` breaks ties toward the latest.
        let search = |sim: &dyn Fn(usize) -> Self, late: bool| {
            (0..=n)
                .map(sim)
                .reduce(|best, p| {
                    let (d, bd) = (p.miss.abs(), best.miss.abs());
                    if d < bd - 1e-9 || (late && d <= bd + 1e-9) {
                        p
                    } else {
                        best
                    }
                })
                .expect("at least one switch time")
        };
        match (a.online, b.online) {
            (false, false) if cb <= ca => run(&|_| false, &|_| 0.0),
            // On the charger throughout, or on battery at both ends but charged in between
            // (plugged in and out while asleep or off): taken as plugged in for the whole gap.
            // Held at `a`'s limit, then topped off to finish at `b`, as pbcd does before a
            // usual unplug.
            (false, false) | (true, true) => {
                let hold = limit(a).min(last);
                if hold >= last {
                    run(&|_| true, &|_| last)
                } else {
                    search(
                        &|s| run(&|_| true, &|k| if k < s { hold } else { last }),
                        true,
                    )
                }
            }
            // Plugged in at bedtime is likelier than just before waking, so on a tie (held
            // at the limit by `b`) the earliest.
            (false, true) => search(&|s| run(&|k| k >= s, &|_| last), false),
            (true, false) => {
                let cap = limit(a).max(cb);
                search(&|s| run(&|k| k < s, &|_| cap), false)
            }
        }
    }

    fn simulate(
        a: &Sample,
        n: usize,
        cb: f64,
        plugged: &dyn Fn(usize) -> bool,
        cap: &dyn Fn(usize) -> f64,
        rates: &Rates,
    ) -> Self {
        let mut c = a.capacity.unwrap_or(0) as f64;
        let mut levels = Vec::with_capacity(n + 1);
        let mut states = Vec::with_capacity(n);
        levels.push(c);
        for k in 0..n {
            let st = if !plugged(k) {
                c = (c - rates.drain_pct_per_h * STEP as f64 / HOUR as f64).max(0.0);
                State::Battery
            } else if c < cap(k) {
                let l = (c as u8).min(99);
                let secs = rates.charge.secs_to(l, l + 1).max(1) as f64;
                c = (c + STEP as f64 / secs).min(cap(k));
                State::Charging
            } else {
                State::Held
            };
            levels.push(c);
            states.push(st);
        }
        Self {
            levels,
            states,
            miss: c - cb,
        }
    }

    fn at(&self, from: Ts, t: Ts) -> f64 {
        let k = ((t - from) / STEP) as usize;
        self.levels[k.min(self.levels.len() - 1)]
    }

    /// Plug state over the steps that start in `[h0, h1)`.
    fn power(&self, from: Ts, h0: Ts, h1: Ts) -> Option<Power> {
        let n = self.states.len();
        let k0 = (((h0 - from).max(0) / STEP) as usize).min(n);
        let k1 = ((((h1 - from) + STEP - 1) / STEP) as usize).min(n);
        let mut c = [0u32; 3];
        for st in &self.states[k0..k1] {
            c[match st {
                State::Charging => 0,
                State::Held => 1,
                State::Battery => 2,
            }] += 1;
        }
        plug_state(&c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(ts: Ts, online: bool, status: &str, cap: u8) -> Sample {
        Sample {
            ts,
            online,
            status: Some(status.into()),
            capacity: Some(cap),
            energy_uwh: None,
            power_uw: None,
            th_start: None,
            th_end: None,
        }
    }

    /// 1%/h drain and the default curve: 66 s/% below 85%.
    fn rates() -> Rates {
        Rates {
            drain_pct_per_h: 1.0,
            charge: ChargeModel::default(),
        }
    }

    fn hours(samples: &[Sample], now: Ts, n: u32) -> Vec<Hour> {
        hourly(None, samples, now, n, &rates())
    }

    fn caps(h: &[Hour]) -> Vec<Option<u8>> {
        h.iter().map(|h| h.capacity).collect()
    }

    use Power::{Charging as C, Held as H};

    /// Per-minute samples for one hour from 0, plugged state by minute.
    fn minutes(state: impl Fn(Ts) -> Option<Power>) -> Vec<Sample> {
        (0..60)
            .map(|m| match state(m) {
                None => s(m * 60, false, "Discharging", 50),
                Some(C) => s(m * 60, true, "Charging", 50),
                Some(H) => s(m * 60, true, "NotCharging", 50),
            })
            .collect()
    }

    #[test]
    fn plug_state_per_quarter() {
        let samples = minutes(|m| match m {
            0..15 | 45.. => None,
            30 | 31 => Some(C), // any charging at all
            _ => Some(H),
        });
        let h = hours(&samples, 0, 1);
        assert_eq!(h[0].power, [None, Some(H), Some(C), None]);
        assert_eq!(h[0].capacity, Some(50));
    }

    #[test]
    fn a_short_top_off_shows_as_charging() {
        // As seen: 26 min charging 76% -> 95%, then held.
        let samples = minutes(|m| Some(if m < 26 { C } else { H }));
        assert_eq!(
            hours(&samples, 0, 1)[0].power,
            [Some(C), Some(C), Some(H), Some(H)]
        );
    }

    #[test]
    fn plugged_for_a_minority_shows_battery() {
        let samples = minutes(|m| (m % 3 == 0).then_some(H));
        assert_eq!(hours(&samples, 0, 1)[0].power, [None; 4]);
    }

    #[test]
    fn sleep_on_battery_meets_both_ends() {
        // Asleep from 1h to 5h, 80% -> 76%. Estimates are for each hour's end.
        let samples = vec![
            s(HOUR, false, "Discharging", 80),
            s(5 * HOUR, false, "Discharging", 76),
        ];
        let h = hours(&samples, 5 * HOUR, 6);
        assert_eq!(
            caps(&h),
            [None, Some(80), Some(78), Some(77), Some(76), Some(76)]
        );
        assert!(h[2].estimated && !h[5].estimated);
        assert!(h.iter().all(|h| h.power == [None; 4]));
    }

    #[test]
    fn fills_from_before_the_window() {
        let before = s(0, false, "Discharging", 90);
        let samples = vec![s(10 * HOUR, false, "Discharging", 80)];
        let h = hourly(Some(&before), &samples, 10 * HOUR, 3, &rates());
        assert_eq!(caps(&h), [Some(81), Some(80), Some(80)]);
    }

    #[test]
    fn plugged_in_asleep_then_held() {
        // Slept on battery at 60%, woke on the charger held at 80%: plugged in at the
        // start (the earliest that fits), 22 min to 80%, then held.
        let samples = vec![
            s(0, false, "Discharging", 60),
            s(6 * HOUR, true, "NotCharging", 80),
        ];
        let h = hours(&samples, 6 * HOUR, 7);
        // The first quarter has the sample, on battery. The charging shows in the next.
        assert_eq!(h[0].power, [None, Some(C), Some(H), Some(H)]);
        for x in &h[1..6] {
            assert_eq!((x.capacity, x.power), (Some(80), [Some(H); 4]), "{x:?}");
        }
    }

    #[test]
    fn plugged_in_asleep_still_charging() {
        // On battery at 1%/h, then ~18% -> 80% takes 68 min: plugged in just before 3h.
        let samples = vec![
            s(0, false, "Discharging", 21),
            s(4 * HOUR, true, "Charging", 80),
        ];
        let h = hours(&samples, 4 * HOUR, 5);
        assert_eq!((h[1].capacity, h[1].power), (Some(19), [None; 4]));
        assert_eq!(h[2].power, [None, None, None, Some(C)]);
        assert_eq!(h[3].power, [Some(C); 4]);
        assert!(h[3].capacity.unwrap() > 70, "{:?}", h[3]);
    }

    #[test]
    fn unplugged_asleep() {
        // Held at 80%, woke on battery at 77%: at 1%/h, unplugged about 3h before.
        let mut a = s(0, true, "NotCharging", 80);
        a.th_end = Some(80);
        let samples = vec![a, s(8 * HOUR, false, "Discharging", 77)];
        let h = hours(&samples, 8 * HOUR, 9);
        assert_eq!((h[2].capacity, h[2].power), (Some(80), [Some(H); 4]));
        assert_eq!((h[6].capacity, h[6].power), (Some(78), [None; 4]));
    }

    #[test]
    fn charged_while_unobserved_holds_then_tops_off() {
        // Asleep on battery at 60% with an 80% limit, woke on battery at 94%: plugged in
        // overnight, held at 80%, topped off to finish at the end.
        let mut a = s(0, false, "Discharging", 60);
        a.th_end = Some(80);
        let samples = vec![a, s(12 * HOUR, false, "Discharging", 94)];
        let h = hours(&samples, 12 * HOUR, 13);
        assert_eq!((h[1].capacity, h[1].power), (Some(80), [Some(H); 4]));
        assert_eq!((h[10].capacity, h[10].power), (Some(80), [Some(H); 4]));
        // The 22 min top-off, from about 11:38.
        assert_eq!(
            (h[11].capacity, h[11].power),
            (Some(94), [Some(H), Some(H), Some(C), Some(C)])
        );
    }

    #[test]
    fn nothing_before_the_first_sample() {
        let h = hours(&[s(2 * HOUR, false, "Discharging", 50)], 2 * HOUR, 3);
        assert_eq!(caps(&h)[..2], [None, None]);
    }

    #[test]
    fn long_gaps_are_straight() {
        let samples = vec![
            s(0, true, "Charging", 50),
            s(10 * 86400, false, "Discharging", 40),
        ];
        let h = hours(&samples, 10 * 86400, 3);
        assert_eq!(
            (h[1].capacity, h[1].power, h[1].estimated),
            (Some(40), [None; 4], true)
        );
    }
}
