//! How long charging takes, per percent of capacity. It starts from a curve
//! measured on an X1 Carbon Gen 13 and is refined from this machine's own
//! charging samples.

use crate::Ts;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChargeModel {
    /// `secs[c]` = seconds to go from c% to (c+1)%, for c in 0..100.
    secs: Vec<f64>,
    /// `obs[c]` = observations behind `secs[c]`. 0 means it's still the prior.
    #[serde(default)]
    obs: Vec<u32>,
}

/// One battery sample, as used for learning.
#[derive(Debug, Clone, Copy)]
pub struct ChargeSample {
    pub ts: Ts,
    pub capacity: u8,
    pub charging: bool,
}

impl Default for ChargeModel {
    /// X1 Carbon Gen 13 on USB-C at about 32 W: about 66 s/% below 85%, then a constant-voltage taper.
    fn default() -> Self {
        let mut secs = vec![66.0; 100];
        for (c, s) in secs.iter_mut().enumerate() {
            *s = match c {
                0..=84 => 66.0,
                85..=89 => 90.0,
                90..=94 => 138.0,
                _ => 180.0,
            };
        }
        Self {
            secs,
            obs: vec![0; 100],
        }
    }
}

impl ChargeModel {
    pub fn secs_to_full(&self, from: u8) -> i64 {
        let from = (from as usize).min(100);
        self.secs[from..].iter().sum::<f64>().round() as i64
    }

    /// Minutes to charge across each `width`-point band, keyed by the band's
    /// lower percent. Only bands where every level was learned are included,
    /// so the built-in prior is never reported as this machine's data.
    pub fn learned_bands(&self, width: u8) -> Vec<(u8, f64)> {
        let w = width.max(1) as usize;
        (0..100)
            .step_by(w)
            .filter(|&lo| (lo..(lo + w).min(100)).all(|c| self.obs.get(c).is_some_and(|&n| n > 0)))
            .map(|lo| {
                let s: f64 = self.secs[lo..(lo + w).min(100)].iter().sum();
                (lo as u8, (s / 6.0).round() / 10.0)
            })
            .collect()
    }

    /// Refine from samples (oldest first). Returns how many percent levels were updated.
    ///
    /// Capacity is reported in whole percents, so most samples show no change.
    /// The time spent at level `c` is the time between capacity *first reaching*
    /// `c` and first reaching `c + 1`, inside one continuous charging run. A run
    /// ends at any non-charging sample, or at a gap longer than `max_gap_s`.
    ///
    /// The level a run starts at is skipped, because we don't know when the
    /// battery reached it. If capacity jumps several points between two
    /// samples, the time is shared evenly between those levels. Each level
    /// takes the interquartile mean of its observations, once there are at least `min_obs`.
    ///
    /// Timing only the sample pairs where capacity rises would drop the flat
    /// minutes in between, hide the taper near full, and make 80 to 100% look
    /// like 10 min instead of about 40.
    pub fn learn(&mut self, samples: &[ChargeSample], max_gap_s: i64, min_obs: usize) -> usize {
        let mut obs: Vec<Vec<f64>> = vec![Vec::new(); 100];
        // (time capacity first reached `level`, level). None outside a run or before its first change.
        let mut reached: Option<(Ts, u8)> = None;
        let mut prev: Option<ChargeSample> = None;
        for &s in samples {
            let continues = prev.is_some_and(|p| {
                p.charging && s.charging && s.ts > p.ts && s.ts - p.ts <= max_gap_s
            });
            if !continues {
                // New run, or not charging. We don't know when the current level was reached.
                reached = None;
                prev = Some(s);
                continue;
            }
            let p = prev.unwrap();
            if s.capacity > p.capacity {
                // Capacity rose between p and s. Attribute the crossing to s's timestamp.
                if let Some((t0, from)) = reached {
                    let levels = s.capacity.saturating_sub(from) as f64;
                    if levels > 0.0 && from < p.capacity.saturating_add(1) {
                        let per = (s.ts - t0) as f64 / levels;
                        for c in from..s.capacity {
                            if (c as usize) < 100 {
                                obs[c as usize].push(per);
                            }
                        }
                    }
                }
                reached = Some((s.ts, s.capacity));
            } else if s.capacity < p.capacity {
                // Capacity fell while "charging" (heavy load, or recalibration). Restart.
                reached = None;
            }
            prev = Some(s);
        }
        let mut changed = 0;
        for (c, o) in obs.iter_mut().enumerate() {
            if o.len() >= min_obs {
                self.secs[c] = interquartile_mean(o);
                self.obs.resize(100, 0);
                self.obs[c] = o.len() as u32;
                changed += 1;
            }
        }
        changed
    }
}

/// Mean of the middle half.
///
/// Whole-percent rounding makes single observations jump between about 0.5x
/// and 1x the true time, and a median would pick one of those modes. Trimming
/// the quartiles drops one-off outliers such as heavy CPU load.
fn interquartile_mean(o: &mut [f64]) -> f64 {
    o.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let n = o.len();
    let (lo, hi) = if n >= 4 { (n / 4, n - n / 4) } else { (0, n) };
    let mid = &o[lo..hi];
    mid.iter().sum::<f64>() / mid.len() as f64
}
