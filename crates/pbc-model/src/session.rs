use crate::Ts;
use serde::{Deserialize, Serialize};

/// Something the collector observed. The daemon produces these from udev,
/// logind and its own lifecycle. Replay tests produce them from a list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PowerEvent {
    /// The charger state was sampled or changed while we were awake.
    Ac {
        ts: Ts,
        online: bool,
        capacity: Option<u8>,
    },
    /// System is about to sleep.
    Suspend { ts: Ts, capacity: Option<u8> },
    /// System resumed. `online` is the AC state read right after resume.
    Resume {
        ts: Ts,
        online: bool,
        capacity: Option<u8>,
    },
    /// Daemon is stopping (shutdown, reboot, or service stop).
    Stop {
        ts: Ts,
        online: bool,
        capacity: Option<u8>,
    },
    /// Daemon started. `last_seen` is the last moment the previous run was
    /// known to be alive. The machine may have been off in between.
    Start {
        ts: Ts,
        online: bool,
        capacity: Option<u8>,
        last_seen: Option<Ts>,
    },
}

impl PowerEvent {
    pub fn ts(&self) -> Ts {
        match self {
            Self::Ac { ts, .. }
            | Self::Suspend { ts, .. }
            | Self::Resume { ts, .. }
            | Self::Stop { ts, .. }
            | Self::Start { ts, .. } => *ts,
        }
    }

    /// AC state and capacity, for events that report AC (all but Suspend).
    pub fn online_capacity(&self) -> Option<(bool, Option<u8>)> {
        match *self {
            Self::Ac {
                online, capacity, ..
            }
            | Self::Resume {
                online, capacity, ..
            }
            | Self::Stop {
                online, capacity, ..
            }
            | Self::Start {
                online, capacity, ..
            } => Some((online, capacity)),
            Self::Suspend { .. } => None,
        }
    }
}

/// When something happened. Usually known exactly. If it happened while we
/// couldn't watch (asleep or powered off), we only know the interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct When {
    /// Earliest possible time.
    pub lo: Ts,
    /// Latest possible time. Equal to `lo` when the time is exact.
    pub hi: Ts,
}

impl When {
    pub fn exact(ts: Ts) -> Self {
        Self { lo: ts, hi: ts }
    }
    pub fn between(lo: Ts, hi: Ts) -> Self {
        Self { lo, hi }
    }
    pub fn is_exact(&self) -> bool {
        self.lo == self.hi
    }
    /// Midpoint. Used where a single number is needed (merging, display).
    pub fn mid(&self) -> Ts {
        self.lo + (self.hi - self.lo) / 2
    }
}

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum End {
    /// Unplug observed (exactly or within an interval).
    Unplugged(When),
    /// We stopped watching while still plugged in (shutdown on AC, daemon
    /// stopped). The session lasted *at least* until this time. A survival
    /// model treats it as right-censored.
    Censored(Ts),
}

/// One plug-in session: one training example for the predictor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub plugged: When,
    pub end: End,
    pub capacity_at_plug: Option<u8>,
    pub capacity_at_end: Option<u8>,
    /// Number of brief interruptions merged into this session.
    pub merged_gaps: u32,
}

impl Session {
    /// Best-estimate duration in seconds (midpoints). For censored sessions,
    /// a lower bound.
    pub fn duration(&self) -> i64 {
        let end = match self.end {
            End::Unplugged(w) => w.mid(),
            End::Censored(t) => t,
        };
        end - self.plugged.mid()
    }
    pub fn is_censored(&self) -> bool {
        matches!(self.end, End::Censored(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// An unplug followed by a replug within this many seconds is merged
    /// into one session (moving rooms, cable knocks). Only applies when both
    /// edges are exact. An imprecise unplug during sleep is not merged.
    pub merge_gap_s: i64,
}

impl Default for Config {
    fn default() -> Self {
        Self { merge_gap_s: 300 }
    }
}

/// What the builder knows about the present moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// Unknown yet (before the first event).
    Unknown,
    /// Awake and observing. `online` is the last known AC state.
    Awake { online: bool },
    /// Asleep or off since `since`. `online` is the AC state when we stopped watching,
    /// and `capacity` the charge then, if known.
    Blind {
        since: Ts,
        online: bool,
        capacity: Option<u8>,
    },
}

/// An open session (currently plugged in).
#[derive(Debug, Clone, PartialEq)]
struct Open {
    plugged: When,
    capacity_at_plug: Option<u8>,
    merged_gaps: u32,
}

/// A session that just ended. It is held back until the merge window
/// passes, in case the charger comes straight back.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    session: Session,
    unplugged_at: Ts,
}

/// Charge gained across a sleep spent on battery that counts as a hidden plug-in.
/// Batteries don't gain charge on their own, and 2 points rules out rounding and
/// the capacity jitter some batteries show at resume.
pub const HIDDEN_CHARGE_PCT: u8 = 2;

/// Incremental state machine: feed events in time order, collect sessions.
#[derive(Debug, Clone)]
pub struct SessionBuilder {
    cfg: Config,
    watch: Watch,
    open: Option<Open>,
    pending: Option<Pending>,
    last_capacity: Option<u8>,
    last_ts: Option<Ts>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BuildError {
    #[error("event at {got} is earlier than previous event at {prev}")]
    OutOfOrder { prev: Ts, got: Ts },
}

impl SessionBuilder {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            watch: Watch::Unknown,
            open: None,
            pending: None,
            last_capacity: None,
            last_ts: None,
        }
    }

    /// True if a session is currently open (plugged in).
    pub fn plugged(&self) -> bool {
        self.open.is_some()
    }

    /// Plug-in time of the open session, if any.
    pub fn open_since(&self) -> Option<When> {
        self.open.as_ref().map(|o| o.plugged)
    }

    /// Feed one event. Returns any sessions that are now final.
    pub fn push(&mut self, ev: &PowerEvent) -> Result<Vec<Session>, BuildError> {
        let ts = ev.ts();
        if let Some(prev) = self.last_ts {
            if ts < prev {
                return Err(BuildError::OutOfOrder { prev, got: ts });
            }
        }
        self.last_ts = Some(ts);
        let mut out = Vec::new();
        self.flush_pending(ts, &mut out);

        match *ev {
            PowerEvent::Ac {
                ts,
                online,
                capacity,
            } => {
                self.note_capacity(capacity);
                self.observe(When::exact(ts), online, capacity, &mut out);
                self.watch = Watch::Awake { online };
            }
            PowerEvent::Suspend { ts, capacity } => {
                self.note_capacity(capacity);
                let online = self.awake_online().unwrap_or(self.open.is_some());
                self.blind(ts, online, capacity);
            }
            PowerEvent::Resume {
                ts,
                online,
                capacity,
            } => {
                self.wake(ts, online, capacity, &mut out);
            }
            PowerEvent::Stop {
                ts,
                online,
                capacity,
            } => {
                self.note_capacity(capacity);
                self.observe(When::exact(ts), online, capacity, &mut out);
                self.blind(ts, online, capacity);
            }
            PowerEvent::Start {
                ts,
                online,
                capacity,
                last_seen,
            } => {
                // A fresh process starts from the persisted state. If the
                // previous run died without a Stop event, `last_seen` gives
                // the start of the blind interval.
                if let (Watch::Unknown | Watch::Awake { .. }, Some(seen)) = (self.watch, last_seen)
                {
                    let was_online = self.open.is_some();
                    self.watch = Watch::Blind {
                        since: seen,
                        online: was_online,
                        capacity: self.last_capacity,
                    };
                }
                self.wake(ts, online, capacity, &mut out);
            }
        }
        Ok(out)
    }

    /// Time has passed with no state change. Releases a session whose merge
    /// window has expired. Call this on every periodic sample.
    pub fn tick(&mut self, now: Ts) -> Vec<Session> {
        let mut out = Vec::new();
        if self.last_ts.is_none_or(|t| now >= t) {
            self.flush_pending(now, &mut out);
        }
        out
    }

    /// Close everything at `ts` (for example, end of a replay). An open
    /// session becomes censored, and a pending one is released.
    pub fn finish(&mut self, ts: Ts) -> Vec<Session> {
        let mut out = Vec::new();
        if let Some(p) = self.pending.take() {
            out.push(p.session);
        }
        if let Some(o) = self.open.take() {
            out.push(Session {
                plugged: o.plugged,
                end: End::Censored(ts),
                capacity_at_plug: o.capacity_at_plug,
                capacity_at_end: self.last_capacity,
                merged_gaps: o.merged_gaps,
            });
        }
        out
    }

    fn blind(&mut self, since: Ts, online: bool, capacity: Option<u8>) {
        self.watch = Watch::Blind {
            since,
            online,
            capacity: capacity.or(self.last_capacity),
        };
    }

    /// Back from Blind (resume, restart): the transition happened somewhere in the gap.
    fn wake(&mut self, ts: Ts, online: bool, capacity: Option<u8>, out: &mut Vec<Session>) {
        let when = match self.watch {
            Watch::Blind { since, .. } => When::between(since, ts),
            _ => When::exact(ts),
        };
        self.hidden_charge(ts, online, capacity, out);
        self.observe(when, online, capacity, out);
        self.note_capacity(capacity);
        self.watch = Watch::Awake { online };
    }

    fn awake_online(&self) -> Option<bool> {
        match self.watch {
            Watch::Awake { online } => Some(online),
            _ => None,
        }
    }

    fn note_capacity(&mut self, c: Option<u8>) {
        if c.is_some() {
            self.last_capacity = c;
        }
    }

    /// The AC state is `online`, as observed at `when`.
    fn observe(&mut self, when: When, online: bool, capacity: Option<u8>, out: &mut Vec<Session>) {
        match (self.open.is_some(), online) {
            (false, true) => self.plug(when, capacity, out),
            (true, false) => self.unplug(when, capacity, out),
            (true, true) => {
                // Still plugged. If we were blind and also off AC at the
                // start of that interval, an unplug/replug might have been
                // missed. We can't know, so treat it as one session.
            }
            (false, false) => {}
        }
    }

    fn plug(&mut self, when: When, capacity: Option<u8>, out: &mut Vec<Session>) {
        // Merge with a just-ended session if the gap is short and both edges are exact.
        if let Some(p) = &self.pending {
            if when.is_exact() && when.lo - p.unplugged_at <= self.cfg.merge_gap_s {
                let p = self.pending.take().unwrap();
                self.open = Some(Open {
                    plugged: p.session.plugged,
                    capacity_at_plug: p.session.capacity_at_plug,
                    merged_gaps: p.session.merged_gaps + 1,
                });
                return;
            }
        }
        // Not merging: the pending session is final.
        if let Some(p) = self.pending.take() {
            out.push(p.session);
        }
        self.open = Some(Open {
            plugged: when,
            capacity_at_plug: capacity.or(self.last_capacity),
            merged_gaps: 0,
        });
    }

    fn unplug(&mut self, when: When, capacity: Option<u8>, out: &mut Vec<Session>) {
        let Some(o) = self.open.take() else { return };
        let session = Session {
            plugged: o.plugged,
            end: End::Unplugged(when),
            capacity_at_plug: o.capacity_at_plug,
            capacity_at_end: capacity.or(self.last_capacity),
            merged_gaps: o.merged_gaps,
        };
        if when.is_exact() {
            // Hold it back for the merge window.
            self.pending = Some(Pending {
                session,
                unplugged_at: when.lo,
            });
        } else {
            // Imprecise unplug (during sleep): no merging, final straight away.
            out.push(session);
        }
    }

    /// Unplugged at both ends of a blind interval (asleep or off), but the
    /// battery gained charge: it was plugged in and out unseen. Plugging in
    /// doesn't wake every laptop, so this can be the only evidence.
    fn hidden_charge(
        &mut self,
        ts: Ts,
        online: bool,
        capacity: Option<u8>,
        out: &mut Vec<Session>,
    ) {
        let Watch::Blind {
            since,
            online: false,
            capacity: Some(before),
        } = self.watch
        else {
            return;
        };
        if online
            || self.open.is_some()
            || !capacity.is_some_and(|after| after >= before.saturating_add(HIDDEN_CHARGE_PCT))
        {
            return;
        }
        self.flush_all_pending(out);
        out.push(Session {
            plugged: When::between(since, ts),
            end: End::Unplugged(When::between(since, ts)),
            capacity_at_plug: Some(before),
            capacity_at_end: capacity,
            merged_gaps: 0,
        });
    }

    /// Release a pending session regardless of the merge window.
    fn flush_all_pending(&mut self, out: &mut Vec<Session>) {
        if let Some(p) = self.pending.take() {
            out.push(p.session);
        }
    }

    fn flush_pending(&mut self, now: Ts, out: &mut Vec<Session>) {
        if let Some(p) = &self.pending {
            if now - p.unplugged_at > self.cfg.merge_gap_s {
                out.push(self.pending.take().unwrap().session);
            }
        }
    }
}

/// Run a whole event list through a builder.
pub fn build(
    cfg: Config,
    events: &[PowerEvent],
    finish_at: Option<Ts>,
) -> Result<Vec<Session>, BuildError> {
    let mut b = SessionBuilder::new(cfg);
    let mut out = Vec::new();
    for e in events {
        out.extend(b.push(e)?);
    }
    if let Some(t) = finish_at {
        out.extend(b.finish(t));
    }
    Ok(out)
}
