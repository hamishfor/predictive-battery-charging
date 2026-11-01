//! Hourly charge history for the extension's chart.

use pbc_model::charge::ChargeModel;
use pbc_model::Ts;
use pbc_store::Sample;
use serde::Serialize;

pub const HOUR: Ts = 3600;

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
    /// No samples this hour. `capacity` and `power` are from simulating the gap.
    pub estimated: bool,
    /// Plug state for most of the hour. None on battery.
    pub power: Option<Power>,
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
            power: None,
        })
        .collect();
    // [charging, held, battery] counts per hour.
    let mut counts = vec![[0u32; 3]; out.len()];
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
        counts[i][k] += 1;
    }
    for (h, c) in out.iter_mut().zip(&counts) {
        h.power = majority(c);
    }
    let mut points = before
        .into_iter()
        .chain(samples)
        .filter(|s| s.capacity.is_some());
    if let Some(mut prev) = points.next() {
        for s in points {
            fill(&mut out, first, prev, s, rates);
            prev = s;
        }
    }
    out
}

fn charging(s: &Sample) -> bool {
    s.status.as_deref() == Some("Charging")
}

/// From [charging, held, battery] counts.
fn majority(c: &[u32; 3]) -> Option<Power> {
    (c[0] + c[1] > c[2]).then_some(if c[0] >= c[1] {
        Power::Charging
    } else {
        Power::Held
    })
}

/// Fill the empty hours strictly between consecutive samples `a` and `b`.
fn fill(out: &mut [Hour], first: Ts, a: &Sample, b: &Sample, rates: &Rates) {
    let (Some(ca), Some(cb)) = (a.capacity, b.capacity) else {
        return;
    };
    let bucket = |t: Ts| (t - first).div_euclid(HOUR);
    let lo = (bucket(a.ts) + 1).max(0);
    let hi = bucket(b.ts).min(out.len() as Ts);
    if hi <= lo {
        return;
    }
    let span = (b.ts - a.ts) as f64;
    let path = (b.ts - a.ts <= MAX_SIM_S).then(|| Path::best(a, b, rates));
    for i in lo..hi {
        let h = &mut out[i as usize];
        let end = h.ts + HOUR;
        let f = (end - a.ts) as f64 / span;
        let (c, power) = match &path {
            // Spread the miss at `b` across the gap, so both ends meet the samples.
            Some(p) => (p.at(a.ts, end) - p.miss * f, p.power(a.ts, h.ts, end)),
            None => (ca as f64 + (cb as f64 - ca as f64) * f, None),
        };
        h.capacity = Some(c.round().clamp(0.0, 100.0) as u8);
        h.estimated = true;
        h.power = power;
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

    /// Majority plug state over the steps that start in `[h0, h1)`.
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
        majority(&c)
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

    fn caps(h: &[Hour]) -> Vec<(Option<u8>, Option<Power>)> {
        h.iter().map(|h| (h.capacity, h.power)).collect()
    }

    #[test]
    fn buckets_by_hour() {
        let now = 10 * HOUR + 120;
        let samples = vec![
            s(5 * HOUR, true, "Charging", 1), // before the window
            s(8 * HOUR, false, "Discharging", 60),
            s(8 * HOUR + 60, false, "Discharging", 59),
            s(9 * HOUR, true, "Charging", 50),
            s(9 * HOUR + 60, true, "NotCharging", 80),
            s(9 * HOUR + 120, true, "NotCharging", 80),
            s(10 * HOUR, true, "Charging", 81),
        ];
        let h = hours(&samples, now, 4);
        assert_eq!(h.len(), 4);
        // Filled from the sample before the window.
        assert!(h[0].estimated && h[0].capacity.is_some(), "{:?}", h[0]);
        assert_eq!((h[1].capacity, h[1].power), (Some(59), None));
        assert_eq!((h[2].capacity, h[2].power), (Some(80), Some(Power::Held)));
        assert_eq!(
            (h[3].capacity, h[3].power),
            (Some(81), Some(Power::Charging))
        );
    }

    #[test]
    fn plugged_for_a_minority_shows_battery() {
        let samples = vec![
            s(0, true, "Charging", 50),
            s(60, false, "Discharging", 50),
            s(120, false, "Discharging", 49),
        ];
        assert_eq!(hours(&samples, 0, 1)[0].power, None);
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
            vec![
                (None, None),
                (Some(80), None),
                (Some(78), None),
                (Some(77), None),
                (Some(76), None),
                (Some(76), None),
            ]
        );
        assert!(h[2].estimated && !h[5].estimated);
    }

    #[test]
    fn fills_from_before_the_window() {
        let before = s(0, false, "Discharging", 90);
        let samples = vec![s(10 * HOUR, false, "Discharging", 80)];
        let h = hourly(Some(&before), &samples, 10 * HOUR, 3, &rates());
        assert_eq!(
            caps(&h),
            vec![(Some(81), None), (Some(80), None), (Some(80), None)]
        );
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
        for x in &h[1..6] {
            assert_eq!(
                (x.capacity, x.power),
                (Some(80), Some(Power::Held)),
                "{x:?}"
            );
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
        assert_eq!((h[1].capacity, h[1].power), (Some(19), None));
        assert_eq!(h[2].power, None);
        assert_eq!(h[3].power, Some(Power::Charging));
        assert!(h[3].capacity.unwrap() > 70, "{:?}", h[3]);
    }

    #[test]
    fn unplugged_asleep() {
        // Held at 80%, woke on battery at 77%: at 1%/h, unplugged about 3h before.
        let mut a = s(0, true, "NotCharging", 80);
        a.th_end = Some(80);
        let samples = vec![a, s(8 * HOUR, false, "Discharging", 77)];
        let h = hours(&samples, 8 * HOUR, 9);
        assert_eq!((h[2].capacity, h[2].power), (Some(80), Some(Power::Held)));
        assert_eq!((h[6].capacity, h[6].power), (Some(78), None));
    }

    #[test]
    fn charged_while_unobserved_holds_then_tops_off() {
        // Asleep on battery at 60% with an 80% limit, woke on battery at 94%: plugged in
        // overnight, held at 80%, topped off to finish at the end.
        let mut a = s(0, false, "Discharging", 60);
        a.th_end = Some(80);
        let samples = vec![a, s(12 * HOUR, false, "Discharging", 94)];
        let h = hours(&samples, 12 * HOUR, 13);
        assert_eq!((h[1].capacity, h[1].power), (Some(80), Some(Power::Held)));
        assert_eq!((h[8].capacity, h[8].power), (Some(80), Some(Power::Held)));
        assert_eq!((h[10].capacity, h[10].power), (Some(80), Some(Power::Held)));
        // The 22 min top-off is a minority of the last hour.
        assert_eq!(h[11].capacity, Some(94));
    }

    #[test]
    fn nothing_before_the_first_sample() {
        let h = hours(&[s(2 * HOUR, false, "Discharging", 50)], 2 * HOUR, 3);
        assert_eq!(caps(&h)[..2], [(None, None), (None, None)]);
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
            (Some(40), None, true)
        );
    }
}
