//! The charge controller: a pure state machine.
//!
//! `decide(prev, input, policy)` returns the next state and the charge limit.
//! The limit is graded by confidence: a confident prediction of a long session
//! holds low (down to 70%), a weak one holds high (up to 99%), and no prediction
//! charges to the maximum (100% unless `max_level` is set lower). As the
//! predicted unplug nears, the limit steps up to a ready level (90%), then to
//! the maximum. Only the user's override goes past `max_level`, to 100%.
//!
//! It has no I/O, so every transition is unit-tested. The daemon calls it on
//! every tick and event, and writes thresholds to match.

use crate::predict::Prediction;
use crate::Ts;
use serde::{Deserialize, Serialize};

/// The charge limit to apply: charge up to `end`, resume below `start`.
///
/// `end == 100` is "full": the firmware default, no limit. Every other value is
/// a hold, somewhere in `Policy::min_hold..=Policy::max_hold`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub end: u8,
}

impl Target {
    pub const FULL: Target = Target { end: 100 };

    pub fn is_full(self) -> bool {
        self.end >= 100
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    /// On battery. The current hold level stays set for the next plug-in.
    Unplugged,
    /// Plugged in, with no prediction worth acting on. Charges to the level
    /// confidence allows, which is 100% when there's no prediction at all.
    Normal,
    /// Holding below the maximum ahead of a predicted unplug at `full_by`.
    /// The level steps up as the unplug gets closer, then `topoff_at` starts the final charge to `max_level`.
    Holding { full_by: Ts, topoff_at: Ts },
    /// Charging to `max_level` for `full_by`. Latched until unplug, or until
    /// `full_by + release_after_s`, so it doesn't flap as capacity rises.
    ToppingOff { full_by: Ts },
    /// The user asked for a full charge, to 100% whatever `max_level` says. Lasts until the end of this plug-in
    /// session (requested on battery: until the end of the next one).
    Override,
}

impl State {
    pub fn name(&self) -> &'static str {
        match self {
            State::Unplugged => "unplugged",
            State::Normal => "normal",
            State::Holding { .. } => "holding",
            State::ToppingOff { .. } => "topping_off",
            State::Override => "override",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    /// Below this confidence a prediction has no effect: charge to `max_level`.
    pub min_confidence: f64,
    /// Extra time before the estimated top-off start (seconds).
    pub margin_s: i64,
    /// Once a top-off's deadline is this far in the past, re-evaluate.
    pub release_after_s: i64,
    /// The lowest hold, used when an unplug is far off and the prediction is certain.
    pub min_hold: u8,
    /// The highest hold below full. A hold above this is pointless.
    pub max_hold: u8,
    /// The intermediate level for when an unplug may come soon (see `ready_window_s`).
    pub ready_level: u8,
    /// Step up to `ready_level` once the predicted unplug is this close.
    pub ready_window_s: i64,
    /// The most any automatic decision charges to. 100 is no limit.
    pub max_level: u8,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            min_confidence: 0.3,
            margin_s: 15 * 60,
            release_after_s: 3600,
            min_hold: 70,
            max_hold: 99,
            ready_level: 90,
            ready_window_s: 3 * 3600,
            max_level: 100,
        }
    }
}

impl Policy {
    /// The automatic top-off target: `max_level`, or full when that's 100.
    pub fn top(&self) -> Target {
        Target {
            end: self.max_level.min(100),
        }
    }

    /// The hold level for a confidence. Linear from `max_hold` at `min_confidence`
    /// down to `min_hold` at confidence 1.0, rounded to whole percent.
    pub fn hold_for(&self, confidence: f64) -> u8 {
        let span = (1.0 - self.min_confidence).max(f64::EPSILON);
        let t = ((confidence - self.min_confidence) / span).clamp(0.0, 1.0);
        let lvl = self.max_hold as f64 - t * (self.max_hold as f64 - self.min_hold as f64);
        (lvl.round() as u8).clamp(self.min_hold, self.max_hold)
    }
}

#[derive(Debug, Clone)]
pub struct Input<'a> {
    pub now: Ts,
    pub online: bool,
    pub prediction: Option<&'a Prediction>,
    /// The user asked for a full charge. If plugged in, this session charges to
    /// 100%. If on battery, the next plug-in does. The caller clears it when
    /// that plug-in session ends.
    pub override_requested: bool,
    /// Seconds from the current capacity to `max_level`.
    pub secs_to_full: i64,
    /// Seconds from `ready_level` to `max_level`, for planning the final step.
    pub secs_ready_to_full: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub state: State,
    pub target: Target,
    pub reason: String,
}

pub fn decide(prev: &State, i: &Input<'_>, p: &Policy) -> Decision {
    let max = p.max_level.min(100);
    let d = |state: State, end: u8, reason: String| Decision {
        state,
        target: Target { end: end.min(max) },
        reason,
    };

    if i.override_requested {
        // The only decision allowed past `max_level`. On battery: open the limit now, so
        // the next plug-in charges straight to 100%, even if pbcd misses the plug event.
        let (state, reason) = if i.online {
            (State::Override, "charge to full requested")
        } else {
            (State::Unplugged, "on battery: next plug-in charges to 100%")
        };
        return Decision {
            state,
            target: Target::FULL,
            reason: reason.into(),
        };
    }

    let pred = i
        .prediction
        .filter(|pr| pr.confidence >= p.min_confidence && pr.full_by > i.now);

    if !i.online {
        // Leave the limit the next plug-in would start with, so a plug-in pbcd doesn't see is held too.
        let end = pred.map(|pr| p.hold_for(pr.confidence)).unwrap_or(max);
        return d(State::Unplugged, end, "on battery".into());
    }
    if let State::ToppingOff { full_by } = *prev {
        if i.now < full_by + p.release_after_s {
            return d(
                State::ToppingOff { full_by },
                max,
                format!("charging to {max}%, ready by {}", hm(full_by)),
            );
        }
    }
    let Some(pr) = pred else {
        return d(
            State::Normal,
            max,
            format!("no confident prediction: charging to {max}%"),
        );
    };

    // Final step: `max` in time for the predicted unplug.
    let topoff_at = pr.full_by - i.secs_to_full.max(0) - p.margin_s;
    if i.now >= topoff_at {
        return d(
            State::ToppingOff {
                full_by: pr.full_by,
            },
            max,
            format!("charging to {max}% ({})", pr.reason),
        );
    }
    // Middle step: once the unplug may be close, stand at the ready level, so an
    // early unplug still leaves a good charge.
    let hold = p.hold_for(pr.confidence).min(max);
    let ready = p.ready_level.min(max);
    let ready_at = pr.full_by - p.ready_window_s;
    let (end, why) = if i.now >= ready_at && hold < ready {
        (ready, format!("{ready}% while an unplug may be near"))
    } else {
        (hold, format!("holding at {hold}%"))
    };
    d(
        State::Holding {
            full_by: pr.full_by,
            topoff_at,
        },
        end,
        format!(
            "{why}, {max}% by {} ({}, confidence {:.0}%)",
            hm(pr.full_by),
            pr.reason,
            pr.confidence * 100.0
        ),
    )
}

/// What to do as the system goes to sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SleepAction {
    /// Nothing to change.
    Nothing,
    /// Start the top-off to `max_level` now, before sleeping.
    StartNow,
    /// Hold through sleep, and do a quiet maintenance wake at `at` to start the top-off to `max_level`.
    Wake { at: Ts },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SleepFallback {
    /// No quiet wake available: start the top-off before sleeping (readiness first).
    Full,
    /// No quiet wake available: keep holding (battery health first; may not be full in time).
    Hold,
}

/// Plan the coming sleep.
///
/// `unplugged_topoff` is the top-off time for the next deadline, if the laptop
/// is going to sleep on battery. Many laptops don't wake when plugged in, so a
/// charger connected after the lid closes would otherwise stop at the hold
/// limit. A wake at that time costs a few seconds, and the hook skips the write
/// if the laptop is still on battery.
pub fn plan_sleep(
    d: &Decision,
    now: Ts,
    quiet_wake: bool,
    presleep_window_s: i64,
    fallback: SleepFallback,
    unplugged_topoff: Option<Ts>,
) -> SleepAction {
    if d.state == State::Unplugged {
        return match unplugged_topoff {
            Some(at) if quiet_wake && at > now => SleepAction::Wake { at },
            _ => SleepAction::Nothing,
        };
    }
    match d.state {
        State::Holding { topoff_at, .. } => {
            if topoff_at - now <= presleep_window_s {
                SleepAction::StartNow
            } else if quiet_wake {
                SleepAction::Wake { at: topoff_at }
            } else {
                match fallback {
                    SleepFallback::Full => SleepAction::StartNow,
                    SleepFallback::Hold => SleepAction::Nothing,
                }
            }
        }
        _ => SleepAction::Nothing,
    }
}

fn hm(ts: Ts) -> String {
    jiff::Timestamp::from_second(ts)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%a %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| ts.to_string())
}
