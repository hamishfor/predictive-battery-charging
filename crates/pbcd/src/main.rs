mod actuator;
mod config;
mod dbus;
mod hw;
mod selfcheck;
mod state;
mod upower;

use anyhow::{Context, Result};
use clap::Parser;
use config::{Mode, QuietWake, ShutdownValue};
use futures_util::StreamExt;
use pbc_hw::sysfs::PSY;
use pbc_hw::wakeplan;
use pbc_hw::SysRoot;
use pbc_model::control::{plan_sleep, SleepAction, Target};
use pbc_model::predict::Schedule;
use pbc_model::session::{Config as SessionConfig, PowerEvent};
use pbc_store::Store;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

#[derive(Parser)]
#[command(name = "pbcd", version = env!("PBC_VERSION"), about = "Predictive battery charging daemon")]
struct Args {
    /// Config file
    #[arg(long, default_value = "/etc/pbcd/config.toml")]
    config: PathBuf,
    /// Override the database path (for running unprivileged in development)
    #[arg(long)]
    db: Option<PathBuf>,
    /// Use the session bus instead of the system bus (development)
    #[arg(long)]
    session_bus: bool,
    /// Don't connect to D-Bus at all: just sample and log (development)
    #[arg(long)]
    no_dbus: bool,
    /// Re-derive all sessions from the raw event log, then exit
    #[arg(long)]
    rebuild_sessions: bool,
    /// Restore firmware default thresholds (0/100), then exit. Used on uninstall.
    #[arg(long)]
    release: bool,
    /// Talk to a fake UPower on the session bus instead of the system bus (development)
    #[arg(long, requires = "session_bus")]
    session_upower: bool,
}

pub(crate) fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Login1 {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
    #[zbus(signal)]
    fn prepare_for_shutdown(&self, start: bool) -> zbus::Result<()>;
    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;
}

/// What the UI shows: (state, capacity, override active, paused). Changes emit `Changed`.
type UiSnapshot = (String, Option<u8>, bool, bool);

/// State shared by the control loop and the D-Bus API.
pub struct Ctl {
    pub core: Mutex<state::Core>,
    pub actuator: Mutex<Option<actuator::Actuator>>,
    /// Set when Predictive Battery Charging is switched off (GNOME "Maximize Charge"), or when
    /// another tool owns the thresholds. pbcd then writes nothing, apart from
    /// one release to 0/100 when the pause starts.
    pub paused: Mutex<Option<String>>,
    /// Woken by D-Bus methods (override), so the loop re-decides straight away.
    pub kick: Notify,
    pub profile: Option<String>,
    /// Friendly battery names from the hardware profile.
    pub battery_labels: std::collections::BTreeMap<String, String>,
    pub mode: Mode,
    /// GNOME Settings → Power → Battery Charging is the on/off switch (`upower_switch`).
    pub gnome_switch: bool,
    /// Sysfs root, for per-battery status.
    pub root: SysRoot,
    /// Set once D-Bus is up. Used to emit `Changed` signals for the GNOME extension.
    pub conn: std::sync::OnceLock<zbus::Connection>,
    /// The last (state, capacity, override, paused) signalled, so we only emit on change.
    pub last_signalled: Mutex<Option<UiSnapshot>>,
    /// Self-check tracking: when the current hold started (and its end %), and
    /// when the limit was last raised while plugged in.
    pub watch: Mutex<Watch>,
    pub resume_grace_s: i64,
}

#[derive(Default)]
pub struct Watch {
    hold_since: Option<(i64, u8)>,
    raised_at: Option<i64>,
}

impl Ctl {
    /// Emit `org.pbc.Daemon1.Changed` if anything the UI shows has changed.
    async fn notify_changed(&self) {
        let Some(conn) = self.conn.get() else { return };
        let snapshot = {
            let c = self.core.lock().await;
            let st = c.status(now(), None).ok();
            (
                c.control.name().to_string(),
                st.and_then(|s| s.capacity),
                c.override_requested,
                self.paused.lock().await.is_some(),
            )
        };
        let mut last = self.last_signalled.lock().await;
        if last.as_ref() == Some(&snapshot) {
            return;
        }
        *last = Some(snapshot);
        drop(last);
        if let Ok(iface) = conn
            .object_server()
            .interface::<_, dbus::Api>(dbus::PATH)
            .await
        {
            let _ = dbus::Api::changed(iface.signal_emitter()).await;
            let _ = iface
                .get()
                .await
                .state_changed(iface.signal_emitter())
                .await;
        }
    }
}

impl Ctl {
    /// Decide and apply. Returns the target that is now in force (if any).
    async fn step(&self) -> Result<Option<Target>> {
        let r = self.step_inner().await;
        self.notify_changed().await;
        r
    }

    async fn step_inner(&self) -> Result<Option<Target>> {
        let paused = self.paused.lock().await.is_some();
        let d = {
            let mut c = self.core.lock().await;
            c.in_control = self.mode == Mode::Control
                && !paused
                && self.actuator.lock().await.is_some()
                && selfcheck::limiting_allowed(&c.store);
            c.decide(now())?
        };
        if self.mode != Mode::Control {
            return Ok(None);
        }
        if let Some(why) = self.paused.lock().await.as_ref() {
            tracing::debug!("not writing: {why}");
            return Ok(None);
        }
        let mut target = d.target;
        if !selfcheck::limiting_allowed(&self.core.lock().await.store) {
            // Charging didn't resume after a hold on this machine. Never hold it again.
            // A fixed `max_pct` is never raised, so it stays.
            let top = self.core.lock().await.policy.top();
            if target != Target::FULL {
                target = top;
            }
        }
        let mut act = self.actuator.lock().await;
        let Some(a) = act.as_mut() else {
            return Ok(None);
        };
        let online = self.core.lock().await.online();
        let before = a.current().map(|t| t.end);
        match a.apply(target, now()) {
            Ok(actuator::Outcome::Unchanged) => {}
            Ok(o) => {
                tracing::info!(?o, reason = %d.reason, "thresholds");
                self.track(before, target, online).await;
            }
            Err(e) => {
                tracing::error!("threshold write failed: {e:#}");
                self.core
                    .lock()
                    .await
                    .alerts
                    .push(format!("write failed: {e}"));
            }
        }
        drop(act);
        self.judge_release(online).await;
        Ok(Some(target))
    }

    /// Note hold starts and limit raises, and judge a hold when it ends.
    /// `before` is the end threshold before this write.
    async fn track(&self, before: Option<u8>, target: Target, online: bool) {
        let t = now();
        let mut w = self.watch.lock().await;
        // A raise ends the current hold, judged at the level it held.
        let raised = before.is_none_or(|b| target.end > b);
        if raised {
            if let Some((from, end)) = w.hold_since.take() {
                if let Err(e) = selfcheck::hold_ended(&self.core.lock().await.store, from, t, end) {
                    tracing::warn!("hold self-check: {e:#}");
                }
            }
            w.raised_at = online.then_some(t);
        }
        if !target.is_full() {
            w.hold_since = online.then_some((t, target.end));
            if !raised {
                w.raised_at = None;
            }
        }
    }

    /// After the limit is raised, check that charging actually starts. If it
    /// doesn't, the self-check records it and limiting stops for good.
    async fn judge_release(&self, online: bool) {
        let mut w = self.watch.lock().await;
        let Some(at) = w.raised_at else { return };
        if !online {
            w.raised_at = None;
            return;
        }
        let store = &self.core.lock().await.store;
        match selfcheck::release_checked(store, at, now(), self.resume_grace_s) {
            Ok(Some(ok)) => {
                w.raised_at = None;
                if !ok {
                    tracing::error!("charging did not resume after the limit was raised: no longer limiting charging on this machine");
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("release self-check: {e:#}");
                w.raised_at = None;
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .without_time() // journald adds timestamps
        .init();
    let args = Args::parse();
    let mut cfg = config::Config::load(&args.config)
        .with_context(|| format!("loading {}", args.config.display()))?;
    if cfg.removed_quiet_wake_ignore.is_some() {
        tracing::warn!("quiet_wake_ignore is no longer used and can be removed from the config: chargers are now judged by the wake's timing");
    }
    if let Some(db) = args.db {
        cfg.db = db;
    }

    let hw = hw::Hw::probe(SysRoot::new(&cfg.sysfs), cfg.profiles_dir.as_deref())?;

    // Build the actuator (control mode only, and only with a matching profile).
    let actuator = match (
        &hw.profile,
        hw.batteries().iter().any(|b| b.has_thresholds()),
    ) {
        (Some(p), true) => Some(actuator::Actuator::new(
            hw.root.clone(),
            p.clone(),
            cfg.floor_pct,
            cfg.max_pct.clamp(50, 100),
        )?),
        _ => None,
    };
    if args.release {
        if let Some(mut a) = actuator {
            a.release()?;
            println!("thresholds restored to firmware defaults (0/100)");
        }
        wakeplan::clear(&cfg.runtime_dir);
        return Ok(());
    }
    let mode = if cfg.mode == Mode::Control && actuator.is_none() {
        tracing::warn!(
            "mode = control, but no hardware profile matches this machine: collect only"
        );
        Mode::Collect
    } else {
        cfg.mode
    };
    let dmi = pbc_hw::DmiInfo::read(&hw.root);
    let model_validated = hw
        .profile
        .as_ref()
        .is_some_and(|p| p.quiet_wake.validated.iter().any(|m| m == dmi.model()));
    // Re-evaluated before every sleep, so a self-check failure takes effect the same night.
    let quiet_wake_now = |store: &Store| match cfg.quiet_wake {
        QuietWake::On => true,
        QuietWake::Off => false,
        QuietWake::Auto => {
            hw.profile.is_some()
                && selfcheck::quiet_wake_allowed(
                    store,
                    model_validated,
                    cfg.quiet_wake_try_untested,
                )
        }
    };

    let schedule = Schedule::new(&cfg.schedule).context("invalid [[schedule]]")?;
    let tz = jiff::tz::TimeZone::system();
    let scfg = SessionConfig {
        merge_gap_s: cfg.merge_gap_s,
    };
    let store = Store::open(&cfg.db)?;
    let mut core = state::Core::new(store, scfg, schedule, tz)?;
    core.policy.min_confidence = cfg.min_confidence;
    core.policy.max_level = cfg.max_pct.clamp(50, 100);
    if cfg.max_pct < 50 {
        tracing::warn!(max_pct = cfg.max_pct, "max_pct below 50: using 50");
    } else if cfg.max_pct < cfg.hold_min {
        tracing::warn!(
            max_pct = cfg.max_pct,
            hold_min = cfg.hold_min,
            "max_pct below hold_min: holding at {}% throughout",
            cfg.max_pct
        );
    }
    let hold_cap = core.policy.max_level.min(99);
    core.policy.min_hold = cfg.hold_min.clamp(50, hold_cap);
    core.policy.max_hold = cfg.hold_max.clamp(core.policy.min_hold, hold_cap);
    core.policy.ready_level = cfg.ready_pct.clamp(core.policy.min_hold, hold_cap);
    core.policy.margin_s = cfg.margin_min * 60;
    if args.rebuild_sessions {
        let n = core.rebuild(scfg, now())?;
        println!("rebuilt {n} sessions from the event log");
        return Ok(());
    }
    if let Ok(Some(b)) = core.retrain(now()) {
        tracing::info!(
            use_model = b.use_model,
            evaluated = b.evaluated,
            "unplug model trained"
        );
    }
    if let Ok(n) = core.learn_charge(now()) {
        if n > 0 {
            tracing::info!(levels = n, "charge model refined from history");
        }
    }

    // Before anything writes thresholds: did the firmware keep the shutdown value?
    let found = actuator
        .as_ref()
        .and_then(|a| a.current())
        .map(|t| (t.start, t.end));
    if let Err(e) = selfcheck::boot_check(&core.store, now(), found) {
        tracing::warn!("boot self-check: {e:#}");
    }
    let quiet_wake = quiet_wake_now(&core.store);
    let last_seen = core.last_seen()?;
    core.event(PowerEvent::Start {
        ts: now(),
        online: hw.online(),
        capacity: hw.capacity(),
        last_seen,
    })?;
    absorb_wake_log(&core.store, &cfg.runtime_dir);
    tracing::info!(
        db = %cfg.db.display(),
        sessions = core.store.session_count()?,
        mode = ?mode,
        quiet_wake,
        "started"
    );

    let profile = hw.profile.as_ref().map(|p| p.id.clone());
    // UPower's charge limit is per battery, and GNOME Settings sets it on all of
    // them together. So the first battery's setting stands for the switch.
    let battery_name = hw.primary();
    let ctl = Arc::new(Ctl {
        core: Mutex::new(core),
        actuator: Mutex::new(actuator),
        paused: Mutex::new(None),
        kick: Notify::new(),
        profile: profile.clone(),
        battery_labels: hw
            .profile
            .as_ref()
            .map(|p| p.battery_labels.clone())
            .unwrap_or_default(),
        mode,
        gnome_switch: cfg.upower_switch,
        root: hw.root.clone(),
        conn: std::sync::OnceLock::new(),
        last_signalled: Mutex::new(None),
        watch: Mutex::new(Watch::default()),
        resume_grace_s: cfg.resume_grace_s,
    });

    // D-Bus: our API, plus logind sleep/shutdown signals and UPower.
    let (conn, system) = if args.no_dbus {
        (None, None)
    } else {
        let builder = if args.session_bus {
            zbus::connection::Builder::session()?
        } else {
            zbus::connection::Builder::system()?
        };
        let conn = builder
            .name(dbus::NAME)?
            .serve_at(
                dbus::PATH,
                dbus::Api {
                    ctl: ctl.clone(),
                    polkit: !args.session_bus,
                },
            )?
            .build()
            .await
            .context("connecting to D-Bus (system bus needs the dbus policy in dist/)")?;
        // logind and UPower live on the system bus even when our API is on the session bus.
        let system = if args.session_bus {
            zbus::Connection::system().await.ok()
        } else {
            Some(conn.clone())
        };
        (Some(conn), system)
    };
    if let Some(c) = &conn {
        let _ = ctl.conn.set(c.clone());
    }
    let _keep = conn;

    let login = match &system {
        Some(c) => Some(Login1Proxy::new(c).await?),
        None => None,
    };
    let upower_conn = if args.session_upower {
        ctl.conn.get().cloned()
    } else {
        system.clone()
    };
    let upower = match (&upower_conn, &battery_name) {
        (Some(c), Some(b)) if mode == Mode::Control => upower::device(c, b).await.ok(),
        _ => None,
    };
    let mut upower_changes = match &upower {
        Some(d) => Some(d.receive_charge_threshold_enabled_changed().await),
        None => None,
    };
    if let Some(d) = &upower {
        check_upower(&ctl, d, cfg.upower_switch).await;
    }

    // A delay inhibitor, so we can act before the system sleeps or shuts down.
    let mut inhibitor = match &login {
        Some(l) => inhibit(l).await,
        None => None,
    };
    let mut sleep_sig = match &login {
        Some(l) => Some(l.receive_prepare_for_sleep().await?),
        None => None,
    };
    let mut shut_sig = match &login {
        Some(l) => Some(l.receive_prepare_for_shutdown().await?),
        None => None,
    };

    ctl.step().await?;

    let mut tick = tokio::time::interval(Duration::from_secs(cfg.sample_interval_s));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut housekeeping = tokio::time::interval(Duration::from_secs(6 * 3600));
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let s = hw.sample(now());
                for done in ctl.core.lock().await.sample(s)? {
                    tracing::info!(duration_min = done.duration() / 60, censored = done.is_censored(), "session finished");
                }
                // Check before stepping, so an overwrite by another tool is seen before we correct it.
                let alerts = match ctl.actuator.lock().await.as_mut() {
                    Some(a) => a.check(now(), hw.online(), cfg.resume_grace_s),
                    None => Vec::new(),
                };
                for alert in alerts {
                    tracing::warn!(?alert, "actuator alert");
                    ctl.core.lock().await.alerts.push(format!("{alert:?}"));
                }
                ctl.step().await?;
            }
            _ = ctl.kick.notified() => {
                ctl.step().await?;
            }
            _ = housekeeping.tick() => {
                let mut c = ctl.core.lock().await;
                let n = c.store.prune_samples(now() - cfg.sample_retention_days * 86400)?;
                if n > 0 { tracing::info!(pruned = n, "old samples pruned"); }
                if let Ok(n) = c.learn_charge(now()) {
                    if n > 0 { tracing::info!(levels = n, "charge model refined"); }
                }
                match c.retrain(now()) {
                    Ok(Some(b)) => tracing::info!(use_model = b.use_model, evaluated = b.evaluated, model_loss = b.model_loss, routine_loss = b.routine_loss, "unplug model retrained"),
                    Ok(None) => {}
                    Err(e) => tracing::warn!("retraining the unplug model: {e:#}"),
                }
                c.alerts.truncate(20);
            }
            Some(_) = next(&mut upower_changes) => {
                if let Some(d) = &upower { check_upower(&ctl, d, cfg.upower_switch).await; }
                ctl.step().await?;
            }
            Some(sig) = next(&mut sleep_sig) => {
                let start = sig.args().map(|a| a.start).unwrap_or(false);
                if start {
                    ctl.core.lock().await.event(PowerEvent::Suspend { ts: now(), capacity: hw.capacity() })?;
                    let qw = quiet_wake_now(&ctl.core.lock().await.store);
                    before_sleep(&ctl, &cfg, qw, &hw).await;
                    tracing::info!("suspending");
                    inhibitor = None; // release so the system can sleep
                } else {
                    ctl.core.lock().await.event(PowerEvent::Resume { ts: now(), online: hw.online(), capacity: hw.capacity() })?;
                    absorb_wake_log(&ctl.core.lock().await.store, &cfg.runtime_dir);
                    tracing::info!(online = hw.online(), "resumed");
                    if let Some(l) = &login { inhibitor = inhibit(l).await; }
                    ctl.step().await?;
                    tick.reset();
                }
            }
            Some(sig) = next(&mut shut_sig) => {
                if sig.args().map(|a| a.start).unwrap_or(false) {
                    ctl.core.lock().await.event(PowerEvent::Stop { ts: now(), online: hw.online(), capacity: hw.capacity() })?;
                    before_shutdown(&ctl, cfg.shutdown).await;
                    tracing::info!("system shutting down");
                    inhibitor = None;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
        }
    }
    ctl.core.lock().await.event(PowerEvent::Stop {
        ts: now(),
        online: hw.online(),
        capacity: hw.capacity(),
    })?;
    drop(inhibitor);
    tracing::info!("stopped");
    Ok(())
}

/// Before sleep: start the top-off now, arm a quiet wake, or keep holding.
async fn before_sleep(ctl: &Ctl, cfg: &config::Config, quiet_wake: bool, hw: &hw::Hw) {
    if ctl.mode != Mode::Control || ctl.paused.lock().await.is_some() {
        return;
    }
    let (d, unplugged_topoff) = {
        let mut c = ctl.core.lock().await;
        match c.decide(now()) {
            Ok(d) => (d, c.unplugged_topoff(now()).unwrap_or(None)),
            Err(e) => {
                tracing::error!("decide before sleep: {e:#}");
                return;
            }
        }
    };
    let action = plan_sleep(
        &d,
        now(),
        quiet_wake,
        cfg.presleep_window_min * 60,
        cfg.sleep_fallback,
        unplugged_topoff,
    );
    tracing::info!(?action, state = d.state.name(), "sleep plan");
    let top = ctl.core.lock().await.policy.top();
    let mut act = ctl.actuator.lock().await;
    let Some(a) = act.as_mut() else { return };
    match action {
        SleepAction::Nothing => {}
        SleepAction::StartNow => {
            if let Err(e) = a.apply(top, now()) {
                tracing::error!("pre-sleep top-off: {e:#}");
            }
        }
        SleepAction::Wake { at } => {
            if wakeplan::pending_selftest(&cfg.runtime_dir, now()) {
                tracing::info!("selftest wake pending: leaving its plan in place");
                return;
            }
            let full = a.thresholds_for(top);
            let gap = hw
                .profile
                .as_ref()
                .map(|p| p.thresholds.min_gap)
                .unwrap_or(1);
            let batteries: Vec<std::path::PathBuf> = a
                .batteries()
                .iter()
                .map(|b| hw.root.path(format!("{PSY}/{}", b.name)))
                .collect();
            let plan = wakeplan::Plan {
                wake_at: at,
                start: full.start,
                end: full.end,
                batteries,
                min_gap: gap,
                selftest: false,
            };
            if let Err(e) = wakeplan::write(&cfg.runtime_dir, &plan) {
                // Without a plan, the hook does nothing. Fall back to topping off now.
                tracing::error!("writing wake plan: {e}; topping off before sleep instead");
                let _ = a.apply(top, now());
            }
        }
    }
}

/// Before power-off: leave the firmware holding the configured value.
async fn before_shutdown(ctl: &Ctl, v: ShutdownValue) {
    if ctl.mode != Mode::Control || ctl.paused.lock().await.is_some() {
        return;
    }
    // The firmware keeps this while off. Hold at the configured lowest level, since nothing will raise it.
    let target = match v {
        ShutdownValue::Hold => Target {
            end: ctl.core.lock().await.policy.min_hold,
        },
        ShutdownValue::Full => ctl.core.lock().await.policy.top(),
    };
    if let Some(a) = ctl.actuator.lock().await.as_mut() {
        match a.apply(target, now()) {
            Ok(o) => {
                tracing::info!(?o, "shutdown thresholds");
                if let Some(t) = a.current() {
                    let _ =
                        selfcheck::shutdown_written(&ctl.core.lock().await.store, t.start, t.end);
                }
            }
            Err(e) => tracing::error!("shutdown thresholds: {e:#}"),
        }
    }
}

/// Judge the last maintenance wake and clear its plan. A recent selftest's log is
/// left for `pbcctl selftest`.
fn absorb_wake_log(store: &Store, dir: &std::path::Path) {
    if !wakeplan::recent_selftest(dir, now()) {
        judge_wake_log(store, dir);
        wakeplan::clear(dir);
    }
}

/// Log the hook's decisions from the last sleep, and judge the quiet wake from them.
fn judge_wake_log(store: &Store, dir: &std::path::Path) {
    let lines = wakeplan::take_log(dir);
    for l in &lines {
        tracing::info!(decision = %l.decision, detail = %l.detail, awake_ms = l.awake_ms, "sleep hook");
    }
    let d: Vec<(String, String)> = lines.into_iter().map(|l| (l.decision, l.detail)).collect();
    if let Err(e) = selfcheck::wake_logged(store, now(), &d) {
        tracing::warn!("wake self-check: {e:#}");
    }
}

/// Follow GNOME Settings → Power → Battery Charging (see upower.rs).
async fn check_upower(ctl: &Ctl, d: &upower::DeviceProxy<'_>, switch: bool) {
    let limit = upower::limit_enabled(d).await;
    let mut p = ctl.paused.lock().await;
    match upower::verdict(switch, limit) {
        upower::Verdict::Run => {
            if p.take().is_some() {
                tracing::info!("Predictive Battery Charging on: pbcd resumes control");
                ctl.kick.notify_one();
            }
        }
        upower::Verdict::Pause(why) => {
            if p.as_deref() != Some(why.as_str()) {
                tracing::info!("{why}: pausing");
                // With the GNOME switch, "off" means charge normally. UPower has
                // already written 0/100 when it disabled its limit, but make sure,
                // in case pbcd was holding when the switch flipped.
                if switch {
                    if let Some(a) = ctl.actuator.lock().await.as_mut() {
                        if let Err(e) = a.release() {
                            tracing::error!("release on pause: {e:#}");
                        }
                    }
                } else {
                    ctl.core.lock().await.alerts.push(why.clone());
                }
                *p = Some(why);
                drop(p);
                ctl.kick.notify_one();
            }
        }
    }
}

async fn inhibit(l: &Login1Proxy<'_>) -> Option<zbus::zvariant::OwnedFd> {
    match l
        .inhibit(
            "sleep:shutdown",
            "pbcd",
            "Set charge thresholds before sleep",
            "delay",
        )
        .await
    {
        Ok(fd) => Some(fd),
        Err(e) => {
            tracing::warn!("could not take logind delay inhibitor: {e}");
            None
        }
    }
}

async fn next<S: futures_util::Stream + Unpin>(s: &mut Option<S>) -> Option<S::Item> {
    match s {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}
