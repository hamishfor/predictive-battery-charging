//! Helpers shared by the unplug predictors.

use crate::session::Session;
use crate::Ts;
use jiff::civil::Weekday;
use jiff::tz::TimeZone;
use jiff::Timestamp;

pub const HOUR: i64 = 3600;
pub const DAY: i64 = 86400;

/// Local minute of day, and whether it's a weekend.
pub fn local_minute(ts: Ts, tz: &TimeZone) -> Option<(i64, bool)> {
    let z = Timestamp::from_second(ts).ok()?.to_zoned(tz.clone());
    let weekend = matches!(z.weekday(), Weekday::Saturday | Weekday::Sunday);
    Some((z.hour() as i64 * 60 + z.minute() as i64, weekend))
}

/// Usable as training data: plug-in known to within an hour (not during sleep),
/// and within `max_age_days` of `now`.
pub fn placeable(s: &Session, now: Ts, max_age_days: i64) -> bool {
    s.plugged.hi - s.plugged.lo <= HOUR && now - s.plugged.lo <= max_age_days * DAY
}

/// Recency weight: halves every `half_life_days`.
pub fn decay(age_s: i64, half_life_days: f64) -> f64 {
    0.5f64.powf(age_s as f64 / DAY as f64 / half_life_days)
}

/// Unplug-time spread → confidence: ≤1 h → 1.0, ≥3 h → 0.
pub fn consistency(spread_h: f64) -> f64 {
    (1.0 - (spread_h - 1.0) / 2.0).clamp(0.0, 1.0)
}
