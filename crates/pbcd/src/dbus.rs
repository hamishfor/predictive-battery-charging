//! D-Bus API: `org.pbc.Daemon1` at `/org/pbc/Daemon1`.
//!
//! Read methods are open to everyone. Methods that change state check polkit
//! (action `org.pbc.daemon1.charge-now`). By default the active local user is
//! allowed without a password, matching UPower's charge-limit toggle.

use crate::config::Mode;
use crate::Ctl;
use std::collections::HashMap;
use std::sync::Arc;
use zbus::message::Header;
use zbus::zvariant::Value;
use zbus::{interface, Connection};

pub const NAME: &str = "org.pbc.Daemon1";
pub const PATH: &str = "/org/pbc/Daemon1";
pub const ACTION_CHARGE_NOW: &str = "org.pbc.daemon1.charge-now";
pub const ACTION_SELFTEST: &str = "org.pbc.daemon1.selftest";

pub struct Api {
    pub ctl: Arc<Ctl>,
    /// False on the session bus (development), where polkit doesn't apply.
    pub polkit: bool,
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn failed(e: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}

#[zbus::proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
trait Authority {
    #[allow(clippy::type_complexity)]
    fn check_authorization(
        &self,
        subject: &(&str, HashMap<&str, Value<'_>>),
        action_id: &str,
        details: HashMap<&str, &str>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<(bool, bool, HashMap<String, String>)>;
}

async fn authorize(conn: &Connection, hdr: &Header<'_>, action: &str) -> zbus::fdo::Result<()> {
    let sender = hdr
        .sender()
        .ok_or_else(|| zbus::fdo::Error::AccessDenied("no sender".into()))?
        .to_string();
    let auth = AuthorityProxy::new(conn).await.map_err(failed)?;
    let mut subj = HashMap::new();
    subj.insert("name", Value::from(sender.as_str()));
    // Flag 1 = AllowUserInteraction (lets polkit show a password prompt if the policy needs one).
    let (ok, _challenge, _) = auth
        .check_authorization(&("system-bus-name", subj), action, HashMap::new(), 1, "")
        .await
        .map_err(failed)?;
    if ok {
        Ok(())
    } else {
        Err(zbus::fdo::Error::AccessDenied(format!(
            "not authorized for {action}"
        )))
    }
}

#[interface(name = "org.pbc.Daemon1")]
impl Api {
    /// Full status as JSON. That keeps the API stable while fields are added.
    async fn status_json(&self) -> zbus::fdo::Result<String> {
        let mut st = self
            .ctl
            .core
            .lock()
            .await
            .status(now(), self.ctl.profile.clone())
            .map_err(failed)?;
        st.mode = mode_str(self.ctl.mode);
        st.thresholds = self
            .ctl
            .actuator
            .lock()
            .await
            .as_ref()
            .and_then(|a| a.current())
            .map(|t| format!("{}/{}", t.start, t.end));
        st.paused = self.ctl.paused.lock().await.clone();
        st.quiet_wake_ignore = self.ctl.ignore_wake().await;
        st.selftest_wake_ignore = self.ctl.selftest_ignore().await;
        let present = pbc_hw::Battery::discover(&self.ctl.root).unwrap_or_default();
        // Use the profile's labels only if they cover every battery present.
        let labelled = present.len() > 1
            && present
                .iter()
                .all(|b| self.ctl.battery_labels.contains_key(&b.name));
        st.batteries = present
            .iter()
            .map(|b| {
                let s = b.snapshot();
                crate::state::BatteryStatusJson {
                    name: b.name.clone(),
                    label: labelled.then(|| self.ctl.battery_labels[&b.name].clone()),
                    capacity: s.capacity_pct,
                    status: format!("{:?}", s.status),
                    thresholds: s.thresholds.map(|t| format!("{}/{}", t.start, t.end)),
                    energy_full_wh: s.energy_full_uwh.map(|e| (e as f64 / 1e5).round() / 10.0),
                }
            })
            .collect();
        if let Some(c) = pbc_hw::Battery::discover(&self.ctl.root)
            .ok()
            .filter(|v| !v.is_empty())
        {
            // Report the live combined charge, not the last sample's.
            st.capacity =
                pbc_hw::combine(&c.iter().map(|b| b.snapshot()).collect::<Vec<_>>()).capacity_pct;
        }
        st.gnome_switch = self.ctl.gnome_switch;
        serde_json::to_string(&st).map_err(failed)
    }

    /// Finished sessions since a unix time, as a JSON array.
    async fn sessions_json(&self, since: i64) -> zbus::fdo::Result<String> {
        let core = self.ctl.core.lock().await;
        let s = core.store.sessions_since(since).map_err(failed)?;
        serde_json::to_string(&s).map_err(failed)
    }

    /// "Charge to full" (true), or cancel it (false). Plugged in, this session
    /// charges to 100%. On battery, the next plug-in does. Either way it ends
    /// when that plug-in session ends.
    async fn charge_now(
        &self,
        on: bool,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> zbus::fdo::Result<()> {
        if self.polkit {
            authorize(conn, &hdr, ACTION_CHARGE_NOW).await?;
        }
        self.ctl
            .core
            .lock()
            .await
            .set_override(on)
            .map_err(failed)?;
        tracing::info!(on, "charge-now override");
        self.ctl.kick.notify_one();
        Ok(())
    }

    /// Record a result from `pbcctl selftest`. `capability` is a key such as
    /// "quiet_wake". `working` false records a confirmed failure. An empty
    /// capability with `working` false clears every confirmation.
    async fn confirm_capability(
        &self,
        capability: String,
        working: bool,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> zbus::fdo::Result<()> {
        use pbc_model::selfcheck::Capability;
        if self.polkit {
            authorize(conn, &hdr, ACTION_SELFTEST).await?;
        }
        let core = self.ctl.core.lock().await;
        let targets: Vec<Capability> = if capability.is_empty() {
            Capability::ALL.to_vec()
        } else {
            vec![Capability::ALL
                .into_iter()
                .find(|c| c.key() == capability)
                .ok_or_else(|| {
                    zbus::fdo::Error::InvalidArgs(format!("unknown capability {capability}"))
                })?]
        };
        if capability.is_empty() {
            core.store
                .set(crate::KV_SELFTEST_IGNORE, "")
                .map_err(failed)?;
        }
        for c in targets {
            let mut r = core.store.capability(c).map_err(failed)?;
            r.confirmed = if capability.is_empty() {
                None
            } else {
                Some(working)
            };
            core.store.set_capability(&r).map_err(failed)?;
            tracing::info!(capability = c.key(), confirmed = ?r.confirmed, "selftest result");
        }
        drop(core);
        self.ctl.kick.notify_one();
        Ok(())
    }

    /// Save the charger wakeup sources that `pbcctl selftest` confirmed fire on
    /// every quiet wake. Replaces the previous list. Only AC adapters and USB
    /// power supplies are accepted, so a keyboard, lid or button can't be listed.
    async fn set_wake_ignore(
        &self,
        sources: Vec<String>,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> zbus::fdo::Result<()> {
        if self.polkit {
            authorize(conn, &hdr, ACTION_SELFTEST).await?;
        }
        for s in &sources {
            let ty = self
                .ctl
                .root
                .read_opt(format!("class/power_supply/{s}/type"));
            if s.contains('/')
                || s.contains('*')
                || !matches!(ty.as_deref(), Some("Mains") | Some("USB"))
            {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "{s} is not an AC or USB power supply"
                )));
            }
        }
        let json = serde_json::to_string(&sources).map_err(failed)?;
        self.ctl
            .core
            .lock()
            .await
            .store
            .set(crate::KV_SELFTEST_IGNORE, &json)
            .map_err(failed)?;
        tracing::info!(
            ?sources,
            "selftest: quiet wake ignores these charger sources"
        );
        Ok(())
    }

    /// Emitted when anything the UI shows changes (state, capacity, override,
    /// yield). Clients then call StatusJson. That avoids polling.
    #[zbus(signal)]
    pub async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    async fn mode(&self) -> String {
        mode_str(self.ctl.mode).into()
    }

    #[zbus(property)]
    async fn plugged(&self) -> bool {
        self.ctl.core.lock().await.online()
    }

    #[zbus(property)]
    async fn state(&self) -> String {
        self.ctl.core.lock().await.control.name().into()
    }

    #[zbus(property)]
    async fn version(&self) -> String {
        env!("PBC_VERSION").into()
    }
}

fn mode_str(m: Mode) -> &'static str {
    match m {
        Mode::Collect => "collect",
        Mode::Control => "control",
    }
}
