//! How fast this machine's battery drains while suspended.

use crate::session::PowerEvent;
use crate::Ts;
use serde::{Deserialize, Serialize};

/// Shorter sleeps lose 0–1% and are mostly rounding.
const MIN_SLEEP_S: Ts = 2 * 3600;
const MIN_SLEEPS: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SleepDrain {
    /// Median over the sleeps measured.
    pub pct_per_h: f64,
    pub sleeps: usize,
}

impl SleepDrain {
    /// From suspend/resume pairs on battery throughout, `MIN_SLEEP_S` or longer.
    /// None until `MIN_SLEEPS` have been seen. `events` sorted by ts.
    pub fn learn(events: &[PowerEvent]) -> Option<Self> {
        let mut online = None;
        let mut asleep: Option<(Ts, u8)> = None;
        let mut rates = Vec::new();
        for ev in events {
            match *ev {
                PowerEvent::Suspend { ts, capacity } => {
                    asleep = capacity.filter(|_| online == Some(false)).map(|c| (ts, c));
                }
                PowerEvent::Resume {
                    ts,
                    online: false,
                    capacity: Some(c),
                } => {
                    if let Some((t0, c0)) =
                        asleep.filter(|&(t0, c0)| ts - t0 >= MIN_SLEEP_S && c <= c0)
                    {
                        rates.push((c0 - c) as f64 / ((ts - t0) as f64 / 3600.0));
                    }
                    asleep = None;
                }
                _ => asleep = None,
            }
            if let Some((o, _)) = ev.online_capacity() {
                online = Some(o);
            }
        }
        if rates.len() < MIN_SLEEPS {
            return None;
        }
        rates.sort_by(f64::total_cmp);
        let n = rates.len();
        let median = if n % 2 == 1 {
            rates[n / 2]
        } else {
            (rates[n / 2 - 1] + rates[n / 2]) / 2.0
        };
        Some(Self {
            pct_per_h: median,
            sleeps: n,
        })
    }

    /// %/h: the measured median, or a typical s2idle drain until enough sleeps are seen.
    pub fn rate(d: Option<&Self>) -> f64 {
        d.map_or(0.5, |d| d.pct_per_h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: Ts = 3600;

    fn ac(ts: Ts, online: bool) -> PowerEvent {
        PowerEvent::Ac {
            ts,
            online,
            capacity: Some(80),
        }
    }
    fn sleep(ts: Ts, cap: u8) -> PowerEvent {
        PowerEvent::Suspend {
            ts,
            capacity: Some(cap),
        }
    }
    fn wake(ts: Ts, online: bool, cap: u8) -> PowerEvent {
        PowerEvent::Resume {
            ts,
            online,
            capacity: Some(cap),
        }
    }

    #[test]
    fn median_of_sleeps_on_battery() {
        let evs = vec![
            ac(0, false),
            sleep(0, 80),
            wake(10 * H, false, 76), // 0.4%/h
            sleep(11 * H, 76),
            wake(21 * H, false, 71), // 0.5%/h
            sleep(22 * H, 71),
            wake(23 * H, false, 71), // too short
            sleep(24 * H, 71),
            wake(34 * H, false, 61), // 1.0%/h
            sleep(35 * H, 61),
            wake(45 * H, true, 70), // woke on the charger
            ac(46 * H, false),
            sleep(46 * H, 70),
            wake(56 * H, false, 75), // rose: charged in between
        ];
        let d = SleepDrain::learn(&evs).unwrap();
        assert_eq!(d.sleeps, 3);
        assert!((d.pct_per_h - 0.5).abs() < 1e-9, "{d:?}");
    }

    #[test]
    fn plugged_in_sleeps_and_too_few_are_ignored() {
        let evs = vec![
            ac(0, true),
            sleep(0, 80),
            wake(10 * H, false, 70), // went to sleep on the charger
            sleep(11 * H, 70),
            wake(21 * H, false, 66),
        ];
        assert_eq!(SleepDrain::learn(&evs), None);
        assert_eq!(SleepDrain::rate(None), 0.5);
    }
}
