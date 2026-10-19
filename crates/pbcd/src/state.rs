//! The daemon core: owns the store, the session builder and the predictor.
//! It is driven by plain method calls, so tests can run it without D-Bus or sysfs.

use anyhow::Result;
use pbc_model::charge::{ChargeModel, ChargeSample};
use pbc_model::control::{decide, Decision, Input, Policy, State, Target};
use pbc_model::predict::{Prediction, Predictor, Query, Schedule, WithSchedule};
use pbc_model::session::{Config as SessionConfig, PowerEvent, Session, SessionBuilder};
use pbc_model::Ts;
use pbc_store::{Sample, Store};
use serde::Serialize;

/// History the predictors see (matches `max_age_days`).
const HISTORY_S: Ts = 180 * 86400;

pub const KV_LAST_SEEN: &str = "last_seen";
pub const KV_CONTROL: &str = "control_state";
pub const KV_OVERRIDE: &str = "override";
pub const KV_CHARGE_MODEL: &str = "charge_model";
pub const KV_ACCURACY: &str = "prediction_accuracy";
pub const KV_HAZARD_MODEL: &str = "hazard_model";
pub const KV_BACKTEST: &str = "predictor_backtest";
/// The open session's target, so its outcome can be judged at unplug: (charge_to, was holding for a prediction).
pub const KV_SESSION_TARGET: &str = "session_target";

pub struct Core {
    pub store: Store,
    builder: SessionBuilder,
    schedule: Schedule,
    tz: jiff::tz::TimeZone,
    last_online: Option<bool>,
    last_capacity: Option<u8>,
    pub policy: Policy,
    pub control: State,
    pub last_decision: Option<Decision>,
    pub override_requested: bool,
    /// True once the requested full charge has been plugged in. It ends at the next unplug.
    override_used: bool,
    pub charge: ChargeModel,
    pub alerts: Vec<String>,
    /// Learned unplug-time predictor tunables.
    pub routine: pbc_model::learned::RoutineConfig,
    pub accuracy: pbc_model::learned::Accuracy,
    /// Trained unplug model, used only while `backtest.use_model` says it beats `Routine` here.
    pub hazard: Option<pbc_model::hazard::HazardModel>,
    pub backtest: Option<pbc_model::hazard::Backtest>,
}

/// Snapshot of current state for the D-Bus API and the CLI.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub plugged: bool,
    pub plugged_since: Option<Ts>,
    pub capacity: Option<u8>,
    pub sessions: i64,
    pub prediction: Option<Prediction>,
    pub mode: &'static str,
    pub profile: Option<String>,
    pub state: String,
    pub target: Option<Target>,
    pub reason: Option<String>,
    pub override_active: bool,
    pub secs_to_full: Option<i64>,
    pub thresholds: Option<String>,
    pub alerts: Vec<String>,
    /// Why Predictive Battery Charging isn't acting (switched off in GNOME Settings, or another tool owns charging).
    pub paused: Option<String>,
    /// True when GNOME Settings → Power → Battery Charging is the on/off switch.
    pub gnome_switch: bool,
    /// One entry per battery. `capacity` above is their energy-weighted combination.
    pub batteries: Vec<BatteryStatusJson>,
    /// What this machine has shown it can do (see pbc_model::selfcheck).
    pub capabilities: Vec<pbc_model::selfcheck::Record>,
    /// The charge limit right now: `max_pct`, a hold level below it, or 100 for "charge to full".
    pub charge_to: Option<u8>,
    /// The most automatic charging goes to (`max_pct` in the config). 100 is no limit.
    pub max_pct: u8,
    /// Where the current prediction's deadline came from: "learned", or "schedule" when
    /// the schedule brought it forward. None while still learning.
    pub prediction_source: Option<String>,
    /// Which learned predictor is in use, "model" or "routine", and the last comparison.
    pub learned_predictor: &'static str,
    pub backtest: Option<pbc_model::hazard::Backtest>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatteryStatusJson {
    pub name: String,
    /// Friendly name from the hardware profile, if it labels every present battery.
    pub label: Option<String>,
    pub capacity: Option<u8>,
    pub status: String,
    pub thresholds: Option<String>,
    pub energy_full_wh: Option<f64>,
}

impl Core {
    /// Open the core and rebuild the in-memory builder by replaying stored events.
    pub fn new(
        store: Store,
        scfg: SessionConfig,
        schedule: Schedule,
        tz: jiff::tz::TimeZone,
    ) -> Result<Self> {
        let mut builder = SessionBuilder::new(scfg);
        // Replay events after the last stored session, to restore the open
        // session and any pending one. Sessions before that are already stored.
        let since = store
            .sessions_since(0)?
            .last()
            .map(|s| match s.end {
                pbc_model::session::End::Unplugged(w) => w.hi + 1,
                pbc_model::session::End::Censored(t) => t + 1,
            })
            .unwrap_or(0);
        let mut last_online = None;
        let mut last_capacity = None;
        for ev in store.events_since(since)? {
            // These sessions are already stored (or will be re-derived), so drop them.
            let _ = builder.push(&ev);
            if let Some((online, capacity)) = ev.online_capacity() {
                last_online = Some(online);
                if capacity.is_some() {
                    last_capacity = capacity;
                }
            }
        }
        let control = load_json(&store, KV_CONTROL)?.unwrap_or(State::Unplugged);
        let override_requested = store.get(KV_OVERRIDE)?.as_deref() == Some("1");
        let charge = load_json(&store, KV_CHARGE_MODEL)?.unwrap_or_default();
        let hazard = load_json(&store, KV_HAZARD_MODEL)?;
        let backtest = load_json(&store, KV_BACKTEST)?;
        let accuracy = load_json(&store, KV_ACCURACY)?.unwrap_or_default();
        Ok(Self {
            store,
            builder,
            schedule,
            tz,
            last_online,
            last_capacity,
            policy: Policy::default(),
            control,
            last_decision: None,
            override_requested,
            // A restart while plugged in with an override counts as already used.
            override_used: override_requested && last_online == Some(true),
            charge,
            alerts: Vec::new(),
            routine: Default::default(),
            accuracy,
            hazard,
            backtest,
        })
    }

    pub fn online(&self) -> bool {
        self.last_online.unwrap_or(false)
    }

    /// Run the controller. The caller applies `Decision::target` to the hardware.
    pub fn decide(&mut self, now: Ts) -> Result<Decision> {
        // The override lasts one plug-in session. Requested on battery, it waits for
        // the next plug-in. Requested while plugged in, it covers the current one.
        if self.override_requested {
            if self.online() {
                self.override_used = true;
            } else if self.override_used {
                self.set_override(false)?;
            }
        }
        let pred = if self.online() {
            self.predict(now)?
        } else {
            self.predict_if_plugged_now(now)?
        };
        let input = Input {
            now,
            online: self.online(),
            prediction: pred.as_ref(),
            override_requested: self.override_requested,
            secs_to_full: self.secs_to_top(self.last_capacity.unwrap_or(0)),
            secs_ready_to_full: self.secs_to_top(self.policy.ready_level),
        };
        let d = decide(&self.control, &input, &self.policy);
        self.track_accuracy(&d, pred.as_ref())?;
        if d.state != self.control {
            let same_reason = self
                .last_decision
                .as_ref()
                .is_some_and(|l| l.reason == d.reason);
            if d.state.name() != self.control.name() || !same_reason {
                tracing::info!(from = self.control.name(), to = d.state.name(), reason = %d.reason, "controller");
            }
            self.store
                .set(KV_CONTROL, &serde_json::to_string(&d.state)?)?;
            self.control = d.state.clone();
        }
        self.last_decision = Some(d.clone());
        Ok(d)
    }

    /// Remember what a learned prediction aimed for while plugged in, and judge it at unplug.
    /// Every prediction is learned. A schedule only moves its deadline earlier.
    fn track_accuracy(&mut self, d: &Decision, pred: Option<&Prediction>) -> Result<()> {
        let held = matches!(d.state, State::Holding { .. } | State::ToppingOff { .. });
        if held && pred.is_some() {
            if self
                .store
                .get(KV_SESSION_TARGET)?
                .as_deref()
                .unwrap_or("")
                .is_empty()
            {
                // The session should end at the top: that's what the prediction promised.
                self.store
                    .set(KV_SESSION_TARGET, &self.policy.top().end.to_string())?;
            }
        } else if d.state == State::Unplugged {
            if let Some(target) = self
                .store
                .get(KV_SESSION_TARGET)?
                .and_then(|v| v.parse::<u8>().ok())
            {
                // Within 3 points of the target counts as ready. Capacity readings jitter.
                let reached = self.last_capacity.is_some_and(|c| c + 3 >= target);
                self.accuracy.record(reached);
                tracing::info!(reached, target, capacity = ?self.last_capacity, factor = self.accuracy.factor(), "learned prediction judged");
                self.store
                    .set(KV_ACCURACY, &serde_json::to_string(&self.accuracy)?)?;
                self.store.set(KV_SESSION_TARGET, "")?;
            }
        } else if matches!(d.state, State::Override) {
            // The user overrode it. Not a prediction outcome.
            self.store.set(KV_SESSION_TARGET, "")?;
        }
        Ok(())
    }

    pub fn set_override(&mut self, on: bool) -> Result<()> {
        self.override_requested = on;
        // A new request starts unused. Plugged in, the next decide() marks it used.
        self.override_used = false;
        self.store.set(KV_OVERRIDE, if on { "1" } else { "0" })?;
        Ok(())
    }

    /// Retrain the unplug model and compare it with `Routine` on this laptop's
    /// history. The model is used only if it wins. Returns the backtest.
    pub fn retrain(&mut self, now: Ts) -> Result<Option<pbc_model::hazard::Backtest>> {
        let history = self.store.sessions_since(now - HISTORY_S)?;
        let cfg = pbc_model::hazard::HazardConfig::default();
        let Some(model) = pbc_model::hazard::train(&history, now, &self.tz, &cfg) else {
            return Ok(None);
        };
        let b = pbc_model::hazard::backtest(&history, now, &self.tz, self.routine, cfg);
        self.store
            .set(KV_HAZARD_MODEL, &serde_json::to_string(&model)?)?;
        self.store.set(KV_BACKTEST, &serde_json::to_string(&b)?)?;
        self.hazard = Some(model);
        self.backtest = Some(b.clone());
        Ok(Some(b))
    }

    /// Refine the charge model from the last 90 days of samples.
    pub fn learn_charge(&mut self, now: Ts) -> Result<usize> {
        // Rebuild from the default prior over all retained samples, rather than
        // refining the stored model. A learning fix then flushes old values, and
        // levels with no recent data fall back to the prior, not a stale estimate.
        let samples: Vec<ChargeSample> = self
            .store
            .samples_between(now - 90 * 86400, now)?
            .into_iter()
            .filter_map(|s| {
                Some(ChargeSample {
                    ts: s.ts,
                    capacity: s.capacity?,
                    charging: s.status.as_deref() == Some("Charging"),
                })
            })
            .collect();
        let mut m = ChargeModel::default();
        let n = m.learn(&samples, 600, 3);
        if m != self.charge {
            self.store
                .set(KV_CHARGE_MODEL, &serde_json::to_string(&m)?)?;
            self.charge = m;
        }
        Ok(n)
    }

    /// Record an event: append it to the log, update the builder, and store finished sessions.
    pub fn event(&mut self, ev: PowerEvent) -> Result<Vec<Session>> {
        self.store.append_event(&ev)?;
        let done = match self.builder.push(&ev) {
            Ok(d) => d,
            Err(e) => {
                // Clock went backwards (NTP step, RTC reset). Log the event but
                // don't feed it out of order. The rebuild tool can re-derive later.
                tracing::warn!("ignoring out-of-order event: {e}");
                Vec::new()
            }
        };
        for s in &done {
            self.store.insert_session(s)?;
        }
        if let Some((online, capacity)) = ev.online_capacity() {
            self.last_online = Some(online);
            if capacity.is_some() {
                self.last_capacity = capacity;
            }
        }
        self.store.set(KV_LAST_SEEN, &ev.ts().to_string())?;
        Ok(done)
    }

    /// A periodic sample. Emits an `Ac` event only when the AC state changed,
    /// so a missed udev event is caught within one sample interval.
    pub fn sample(&mut self, s: Sample) -> Result<Vec<Session>> {
        self.store.append_sample(&s)?;
        self.store.set(KV_LAST_SEEN, &s.ts.to_string())?;
        if s.capacity.is_some() {
            self.last_capacity = s.capacity;
        }
        if self.last_online != Some(s.online) {
            return self.event(PowerEvent::Ac {
                ts: s.ts,
                online: s.online,
                capacity: s.capacity,
            });
        }
        let done = self.builder.tick(s.ts);
        for d in &done {
            self.store.insert_session(d)?;
        }
        Ok(done)
    }

    pub fn last_seen(&self) -> Result<Option<Ts>> {
        Ok(self.store.get(KV_LAST_SEEN)?.and_then(|v| v.parse().ok()))
    }

    pub fn predict(&self, now: Ts) -> Result<Option<Prediction>> {
        self.predict_from(now, self.builder.open_since())
    }

    fn predict_from(
        &self,
        now: Ts,
        plugged_since: Option<pbc_model::session::When>,
    ) -> Result<Option<Prediction>> {
        let history = self.store.sessions_since(now - HISTORY_S)?;
        let q = Query {
            now,
            plugged_since,
            capacity: self.last_capacity,
            history: &history,
            tz: &self.tz,
        };
        let factor = self.accuracy.factor();
        let routine = pbc_model::learned::Routine::new(self.routine, factor);
        let with = |learned: &dyn Predictor| {
            WithSchedule {
                learned,
                schedule: &self.schedule,
            }
            .predict(&q)
        };
        let p = match self.hazard.as_ref().filter(|_| self.uses_model()) {
            Some(m) => {
                let hz = pbc_model::hazard::Hazard::new(m, Default::default(), factor);
                with(&pbc_model::learned::Chain(vec![&hz, &routine]))
            }
            None => with(&routine),
        };
        Ok(p)
    }

    /// As if the charger went in now: the limit an unseen plug-in starts at.
    fn predict_if_plugged_now(&self, now: Ts) -> Result<Option<Prediction>> {
        self.predict_from(now, Some(pbc_model::session::When::exact(now)))
    }

    /// The backtest picked the hazard model over `Routine`.
    fn uses_model(&self) -> bool {
        self.hazard.is_some() && self.backtest.as_ref().is_some_and(|b| b.use_model)
    }

    /// While on battery: when a top-off would have to start for the next confident
    /// deadline, if the charger were connected now. The sleep planner uses this to
    /// arm a wake in case the charger goes in after the lid closes.
    pub fn unplugged_topoff(&self, now: Ts) -> Result<Option<Ts>> {
        if self.online() {
            return Ok(None);
        }
        let Some(p) = self.predict_if_plugged_now(now)? else {
            return Ok(None);
        };
        if p.confidence < self.policy.min_confidence || p.full_by <= now {
            return Ok(None);
        }
        // From the current charge, the most a sleep on battery could need.
        let secs = self.secs_to_top(self.last_capacity.unwrap_or(0));
        Ok(Some(p.full_by - secs - self.policy.margin_s))
    }

    /// Seconds from `from` to the automatic top-off level.
    fn secs_to_top(&self, from: u8) -> i64 {
        self.charge.secs_to(from, self.policy.top().end)
    }

    pub fn status(&self, now: Ts, profile: Option<String>) -> Result<Status> {
        let prediction = self.predict(now)?;
        let prediction_source = prediction.as_ref().map(|p| p.source.to_string());
        Ok(Status {
            plugged: self.builder.plugged(),
            plugged_since: self.builder.open_since().map(|w| w.mid()),
            capacity: self.last_capacity,
            sessions: self.store.session_count()?,
            prediction,
            mode: "collect",
            profile,
            state: self.control.name().into(),
            target: self.last_decision.as_ref().map(|d| d.target),
            reason: self.last_decision.as_ref().map(|d| d.reason.clone()),
            override_active: self.override_requested,
            secs_to_full: self.last_capacity.map(|c| self.secs_to_top(c)),
            max_pct: self.policy.top().end,
            thresholds: None,
            alerts: self.alerts.clone(),
            paused: None,
            gnome_switch: false,
            batteries: Vec::new(),
            capabilities: self.store.capabilities().unwrap_or_default(),
            charge_to: self.last_decision.as_ref().map(|d| d.target.end),
            prediction_source,
            learned_predictor: if self.uses_model() {
                "model"
            } else {
                "routine"
            },
            backtest: self.backtest.clone(),
        })
    }

    /// Rebuild all sessions from the raw event log, for example after a builder fix.
    pub fn rebuild(&mut self, scfg: SessionConfig, _now: Ts) -> Result<usize> {
        let events = self.store.events_since(0)?;
        let mut b = SessionBuilder::new(scfg);
        let mut out = Vec::new();
        for e in &events {
            if let Ok(d) = b.push(e) {
                out.extend(d);
            }
        }
        // Keep the open session open, and store only finished ones.
        self.store.replace_sessions(&out)?;
        self.builder = b;
        Ok(out.len())
    }
}

fn load_json<T: serde::de::DeserializeOwned>(store: &Store, key: &str) -> Result<Option<T>> {
    Ok(store.get(key)?.and_then(|v| serde_json::from_str(&v).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbc_model::predict::{Day, ScheduleRule};

    /// A daily 08:00 schedule, and two weeks of nights (see [`learned_nights`]).
    fn core() -> Core {
        let c = bare_core();
        learned_nights(&c.store);
        c
    }

    /// A daily 08:00 schedule, and no history.
    fn bare_core() -> Core {
        let sched = Schedule::new(&[ScheduleRule {
            days: vec![
                Day::Mon,
                Day::Tue,
                Day::Wed,
                Day::Thu,
                Day::Fri,
                Day::Sat,
                Day::Sun,
            ],
            full_by: "08:00".into(),
        }])
        .unwrap();
        Core::new(
            Store::open_in_memory().unwrap(),
            SessionConfig::default(),
            sched,
            jiff::tz::TimeZone::UTC,
        )
        .unwrap()
    }

    /// Two weeks of nights before t = 0 (00:00 UTC), plugged in 22:00 to 09:00.
    /// The learned unplug is about 09:00, so the 08:00 schedule sets the deadline.
    /// Tests that hold plug in at `NIGHT` (22:00 the day before t = 0).
    const NIGHT: Ts = -2 * 3600;

    fn learned_nights(store: &Store) {
        use pbc_model::session::{End, Session, When};
        for d in 1..=14i64 {
            let start = -d * 86400 - 2 * 3600;
            store
                .insert_session(&Session {
                    plugged: When::exact(start),
                    end: End::Unplugged(When::exact(start + 11 * 3600)),
                    capacity_at_plug: Some(50),
                    capacity_at_end: Some(100),
                    merged_gaps: 0,
                })
                .unwrap();
        }
    }

    fn sample(ts: Ts, online: bool) -> Sample {
        Sample {
            ts,
            online,
            status: None,
            capacity: Some(50),
            energy_uwh: None,
            power_uw: None,
            th_start: None,
            th_end: None,
        }
    }

    #[test]
    fn samples_drive_sessions() {
        let mut c = bare_core();
        // Plugged 600..3000, sampled every 60 s up to 3300 (exactly 300 s after unplug).
        for t in (0..=3300).step_by(60) {
            c.sample(sample(t, (600..3000).contains(&t))).unwrap();
        }
        // Unplug at 3000 is held until the merge window (300 s) has passed.
        assert_eq!(c.store.session_count().unwrap(), 0);
        c.sample(sample(3360, false)).unwrap();
        assert_eq!(c.store.session_count().unwrap(), 1);
        let s = &c.store.sessions_since(0).unwrap()[0];
        assert_eq!(s.duration(), 2400);
        // Only state changes are logged as events, not every sample.
        assert_eq!(c.store.events_since(0).unwrap().len(), 3);
    }

    #[test]
    fn restart_restores_open_session() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db");
        learned_nights(&Store::open(&p).unwrap());
        {
            let mut c = Core::new(
                Store::open(&p).unwrap(),
                SessionConfig::default(),
                Schedule::default(),
                jiff::tz::TimeZone::UTC,
            )
            .unwrap();
            c.event(PowerEvent::Ac {
                ts: 100,
                online: true,
                capacity: Some(40),
            })
            .unwrap();
            assert!(c.status(200, None).unwrap().plugged);
        }
        let c = Core::new(
            Store::open(&p).unwrap(),
            SessionConfig::default(),
            Schedule::default(),
            jiff::tz::TimeZone::UTC,
        )
        .unwrap();
        let st = c.status(300, None).unwrap();
        assert!(st.plugged);
        assert_eq!(st.plugged_since, Some(100));
        assert_eq!(c.last_seen().unwrap(), Some(100));
    }

    #[test]
    fn schedule_brings_the_learned_deadline_forward() {
        let mut c = core();
        c.event(PowerEvent::Ac {
            ts: -2 * 3600,
            online: true,
            capacity: Some(60),
        })
        .unwrap();
        let p = c.predict(0).unwrap().unwrap();
        assert_eq!(
            p.full_by,
            8 * 3600,
            "schedule's 08:00, not the learned 09:00"
        );
        assert_eq!(p.source, "schedule");
        assert!(p.reason.starts_with("learned:"), "{}", p.reason);
    }

    /// Unplugged overnight, then a quick charge each morning, with a weekday 08:00
    /// schedule. Every step should charge to 100%: the schedule mustn't hold the
    /// morning charge, and nothing should be held overnight either.
    #[test]
    fn morning_quick_charge_with_a_schedule() {
        use pbc_model::control::{plan_sleep, SleepAction, SleepFallback};
        use pbc_model::session::{End, Session, When};
        const DAY: Ts = 86400;
        let store = Store::open_in_memory().unwrap();
        // Two weeks of mornings before t = 0: plugged in 07:00 to 07:45, 40% to 85%.
        for d in 1..=14 {
            let start = -d * DAY + 7 * 3600;
            store
                .insert_session(&Session {
                    plugged: When::exact(start),
                    end: End::Unplugged(When::exact(start + 45 * 60)),
                    capacity_at_plug: Some(40),
                    capacity_at_end: Some(85),
                    merged_gaps: 0,
                })
                .unwrap();
        }
        // Weekday 08:00 rule. t = 0 is Thu 1 Jan 1970 00:00 UTC, so t = 0 and the day after are weekdays.
        let sched = Schedule::new(&[ScheduleRule {
            days: vec![Day::Mon, Day::Tue, Day::Wed, Day::Thu, Day::Fri],
            full_by: "08:00".into(),
        }])
        .unwrap();
        let mut c = Core::new(
            store,
            SessionConfig::default(),
            sched,
            jiff::tz::TimeZone::UTC,
        )
        .unwrap();

        // 23:00 the night before, on battery at 60%, going to sleep.
        let bed = -DAY + 23 * 3600;
        c.event(PowerEvent::Ac {
            ts: bed - 3600,
            online: false,
            capacity: Some(60),
        })
        .unwrap();
        let d = c.decide(bed).unwrap();
        assert!(d.target.is_full(), "no hold left set overnight: {d:?}");
        // No deadline for an unseen plug-in, so no quiet wake.
        assert_eq!(c.unplugged_topoff(bed).unwrap(), None);
        let action = plan_sleep(&d, bed, true, 30 * 60, SleepFallback::Full, None);
        assert_eq!(action, SleepAction::Nothing);

        // 07:00, plugged in at 40%: the schedule's 08:00 is an hour away, but the routine is a short charge.
        let morning = 7 * 3600;
        c.event(PowerEvent::Ac {
            ts: morning,
            online: true,
            capacity: Some(40),
        })
        .unwrap();
        assert_eq!(c.predict(morning + 60).unwrap(), None);
        for t in (morning + 60..morning + 45 * 60).step_by(5 * 60) {
            let d = c.decide(t).unwrap();
            assert!(d.target.is_full(), "charging to 100% at {t}: {d:?}");
            assert_eq!(d.state, State::Normal);
        }
        // 07:45, unplugged. Off to work at 85%, the most a 45-minute charge allows.
        c.event(PowerEvent::Ac {
            ts: morning + 45 * 60,
            online: false,
            capacity: Some(85),
        })
        .unwrap();
        assert!(c.decide(morning + 45 * 60).unwrap().target.is_full());
    }

    /// Unplugged at 22:00, plugged in 08:15 to 09:15, after the schedule's 08:00. Replayed
    /// over a working week with a history of these mornings. The 08:15 plug-in must
    /// charge straight to 100%, and the night before mustn't leave a hold set.
    #[test]
    fn unplug_at_ten_plug_in_after_the_deadline() {
        use pbc_model::control::{plan_sleep, SleepAction, SleepFallback};
        use pbc_model::session::{End, Session, When};
        const DAY: Ts = 86400;
        const H: Ts = 3600;
        let store = Store::open_in_memory().unwrap();
        // Two weeks of history before t = 0: plugged in 08:15 to 09:15, then unplugged
        // from 22:00. Daytime use in between isn't plugged in.
        for d in 1..=14 {
            let start = -d * DAY + 8 * H + 15 * 60;
            store
                .insert_session(&Session {
                    plugged: When::exact(start),
                    end: End::Unplugged(When::exact(start + H)),
                    capacity_at_plug: Some(55),
                    capacity_at_end: Some(90),
                    merged_gaps: 0,
                })
                .unwrap();
        }
        let sched = Schedule::new(&[ScheduleRule {
            days: vec![Day::Mon, Day::Tue, Day::Wed, Day::Thu, Day::Fri],
            full_by: "08:00".into(),
        }])
        .unwrap();
        let mut c = Core::new(
            store,
            SessionConfig::default(),
            sched,
            jiff::tz::TimeZone::UTC,
        )
        .unwrap();

        // Three days from t = 0 (Thu, Fri, Sat 1 to 3 Jan 1970 UTC). Saturday has no rule.
        for day in 0..3 {
            let base = day * DAY;
            // 22:00 the evening before: unplugged at 90%, then sleep.
            let unplug = base - 2 * H;
            c.event(PowerEvent::Ac {
                ts: unplug,
                online: false,
                capacity: Some(90),
            })
            .unwrap();
            let d = c.decide(unplug + 60).unwrap();
            assert!(
                d.target.is_full(),
                "day {day}: no hold set overnight: {d:?}"
            );
            assert_eq!(
                c.unplugged_topoff(unplug + 60).unwrap(),
                None,
                "day {day}: no wake"
            );
            assert_eq!(
                plan_sleep(&d, unplug + 60, true, 30 * 60, SleepFallback::Full, None),
                SleepAction::Nothing
            );

            // 08:15, plugged in at 55%, 15 minutes after the 08:00 deadline.
            let plug = base + 8 * H + 15 * 60;
            c.event(PowerEvent::Ac {
                ts: plug,
                online: true,
                capacity: Some(55),
            })
            .unwrap();
            assert_eq!(c.predict(plug + 60).unwrap(), None, "day {day}");
            for t in (plug + 60..plug + H).step_by(5 * 60) {
                let d = c.decide(t).unwrap();
                assert!(
                    d.target.is_full(),
                    "day {day}, {t}: charging to 100%: {d:?}"
                );
            }
            // 09:15, unplugged for the day.
            c.event(PowerEvent::Ac {
                ts: plug + H,
                online: false,
                capacity: Some(90),
            })
            .unwrap();
            assert!(
                c.decide(plug + H).unwrap().target.is_full(),
                "day {day}: after the 09:15 unplug"
            );
            // Plugged back in at 21:00 for an hour, as the evening before 22:00.
            c.event(PowerEvent::Ac {
                ts: base + 21 * H,
                online: true,
                capacity: Some(60),
            })
            .unwrap();
            assert!(
                c.decide(base + 21 * H + 60).unwrap().target.is_full(),
                "day {day} evening"
            );
        }
        assert_eq!(
            c.accuracy,
            pbc_model::learned::Accuracy::default(),
            "nothing was held, so nothing judged"
        );
    }

    /// 2026-10-13, 17:00 replayed: a weekday 08:00 schedule, and a history of short
    /// evening top-ups. The schedule must not make pbcd hold until the next morning.
    #[test]
    fn schedule_alone_does_not_hold() {
        let store = Store::open_in_memory().unwrap();
        let sched = Schedule::new(&[ScheduleRule {
            days: vec![Day::Mon, Day::Tue, Day::Wed, Day::Thu, Day::Fri],
            full_by: "08:00".into(),
        }])
        .unwrap();
        let mut c = Core::new(
            store,
            SessionConfig::default(),
            sched,
            jiff::tz::TimeZone::UTC,
        )
        .unwrap();
        c.event(PowerEvent::Ac {
            ts: 17 * 3600,
            online: true,
            capacity: Some(70),
        })
        .unwrap();
        assert_eq!(c.predict(17 * 3600 + 60).unwrap(), None);
        assert!(c.decide(17 * 3600 + 60).unwrap().target.is_full());
    }

    #[test]
    fn controller_holds_then_override_persists() {
        let mut c = core();
        c.event(PowerEvent::Ac {
            ts: NIGHT,
            online: true,
            capacity: Some(60),
        })
        .unwrap();
        // 22:00, plugged in like every night: hold, full by the 08:00 schedule.
        let d = c.decide(NIGHT + 60).unwrap();
        assert!(!d.target.is_full(), "{d:?}");
        c.set_override(true).unwrap();
        assert!(c.decide(NIGHT + 120).unwrap().target.is_full());
        // Unplug clears the override.
        c.event(PowerEvent::Ac {
            ts: NIGHT + 180,
            online: false,
            capacity: Some(60),
        })
        .unwrap();
        c.decide(NIGHT + 180).unwrap();
        assert!(!c.override_requested);
    }

    #[test]
    fn override_on_battery_waits_for_next_plug_in() {
        let mut c = core();
        c.event(PowerEvent::Ac {
            ts: NIGHT - 120,
            online: false,
            capacity: Some(40),
        })
        .unwrap();
        c.decide(NIGHT - 120).unwrap();
        c.set_override(true).unwrap();
        // Still on battery: kept, and the limit is opened.
        let d = c.decide(NIGHT - 60).unwrap();
        assert!(d.target.is_full());
        assert!(c.override_requested, "armed for the next plug-in");
        // Plug in at 22:00: charges to full, even though the routine would hold.
        c.event(PowerEvent::Ac {
            ts: NIGHT,
            online: true,
            capacity: Some(40),
        })
        .unwrap();
        let d = c.decide(NIGHT).unwrap();
        assert_eq!(d.state, State::Override);
        assert!(d.target.is_full());
        // Unplug after that session: cleared. On battery the limit is set for an unseen plug-in,
        // which would be held, since 23:00 is when this laptop is plugged in for the night.
        c.event(PowerEvent::Ac {
            ts: NIGHT + 3600,
            online: false,
            capacity: Some(90),
        })
        .unwrap();
        let d = c.decide(NIGHT + 3600).unwrap();
        assert!(!c.override_requested);
        assert!(!d.target.is_full());
    }

    #[test]
    fn override_cancel_on_battery() {
        let mut c = core();
        c.event(PowerEvent::Ac {
            ts: NIGHT - 120,
            online: false,
            capacity: Some(40),
        })
        .unwrap();
        c.set_override(true).unwrap();
        c.set_override(false).unwrap();
        assert!(!c.decide(NIGHT).unwrap().target.is_full());
    }

    #[test]
    fn unplugged_topoff_for_next_deadline() {
        let mut c = core();
        c.event(PowerEvent::Ac {
            ts: NIGHT - 60,
            online: false,
            capacity: Some(66),
        })
        .unwrap();
        // 22:00 on battery: a plug-in now would be held until the 08:00 schedule. From 66%, the default curve needs about 61 min, plus 15 min margin.
        let at = c
            .unplugged_topoff(NIGHT)
            .unwrap()
            .expect("a deadline is coming");
        assert!(at > 6 * 3600 && at < 7 * 3600, "{at}");
        // Plugged in: the normal hold path handles it.
        c.event(PowerEvent::Ac {
            ts: NIGHT + 60,
            online: true,
            capacity: Some(66),
        })
        .unwrap();
        assert_eq!(c.unplugged_topoff(NIGHT + 60).unwrap(), None);
    }

    /// 2026-10-12 replayed: on battery at 66% going to sleep at 23:00, deadline 08:00.
    /// The planner must arm a wake, so a charger plugged in after the lid closed
    /// still reaches 100%.
    #[test]
    fn sleeping_on_battery_before_a_deadline_arms_a_wake() {
        use pbc_model::control::{plan_sleep, SleepAction, SleepFallback};
        let mut c = core();
        // 22:00, an hour before the replayed 23:00, so it matches this test history's plug-in time.
        let t23 = NIGHT;
        c.event(PowerEvent::Ac {
            ts: t23 - 60,
            online: false,
            capacity: Some(66),
        })
        .unwrap();
        let d = c.decide(t23).unwrap();
        assert_eq!(d.state, State::Unplugged);
        let at = c.unplugged_topoff(t23).unwrap().expect("deadline ahead");
        let action = plan_sleep(&d, t23, true, 30 * 60, SleepFallback::Full, Some(at));
        assert_eq!(action, SleepAction::Wake { at });
        assert!(
            at < 8 * 3600 && at > 6 * 3600,
            "wake before 08:00 with time to charge: {at}"
        );
    }

    #[test]
    fn controller_state_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db");
        let sched = || {
            Schedule::new(&[ScheduleRule {
                days: vec![
                    Day::Mon,
                    Day::Tue,
                    Day::Wed,
                    Day::Thu,
                    Day::Fri,
                    Day::Sat,
                    Day::Sun,
                ],
                full_by: "08:00".into(),
            }])
            .unwrap()
        };
        learned_nights(&Store::open(&p).unwrap());
        {
            let mut c = Core::new(
                Store::open(&p).unwrap(),
                SessionConfig::default(),
                sched(),
                jiff::tz::TimeZone::UTC,
            )
            .unwrap();
            c.event(PowerEvent::Ac {
                ts: NIGHT,
                online: true,
                capacity: Some(90),
            })
            .unwrap();
            // 07:30, inside the top-off window.
            assert!(c.decide(7 * 3600 + 1800).unwrap().target.is_full());
        }
        let mut c = Core::new(
            Store::open(&p).unwrap(),
            SessionConfig::default(),
            sched(),
            jiff::tz::TimeZone::UTC,
        )
        .unwrap();
        assert!(matches!(c.control, State::ToppingOff { .. }));
        // Still latched after the restart.
        assert!(c.decide(7 * 3600 + 1900).unwrap().target.is_full());
    }

    #[test]
    fn learn_charge_replaces_a_stale_model() {
        let mut c = core();
        // A stale model: 60 s for every level.
        let stale: ChargeModel =
            serde_json::from_str(&format!("{{\"secs\":{:?}}}", vec![60.0f64; 100])).unwrap();
        c.charge = stale.clone();
        // No samples: the rebuild falls back to the default prior.
        c.learn_charge(1_000_000).unwrap();
        assert_eq!(c.charge, ChargeModel::default());
        assert_ne!(c.charge, stale);
        let saved: ChargeModel =
            serde_json::from_str(&c.store.get(KV_CHARGE_MODEL).unwrap().unwrap()).unwrap();
        assert_eq!(saved, ChargeModel::default());
    }

    #[test]
    fn rebuild_matches_incremental() {
        let mut c = core();
        let evs = [
            (0, true),
            (1000, false),
            (1100, true),
            (5000, false),
            (9000, true),
            (9500, false),
        ];
        for (t, on) in evs {
            c.event(PowerEvent::Ac {
                ts: t,
                online: on,
                capacity: Some(50),
            })
            .unwrap();
        }
        c.event(PowerEvent::Ac {
            ts: 20000,
            online: false,
            capacity: Some(50),
        })
        .unwrap();
        let before = c.store.sessions_since(0).unwrap();
        assert_eq!(before.len(), 2);
        c.rebuild(SessionConfig::default(), 20000).unwrap();
        assert_eq!(c.store.sessions_since(0).unwrap(), before);
    }
}
