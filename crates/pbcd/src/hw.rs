//! Hardware access for the daemon: profile, AC state, and the machine's batteries.
//!
//! With several batteries, learning and prediction
//! use the *combined* charge, energy-weighted like UPower's display device.
//! That's what the user sees as "battery %", and what runtime depends on.

use pbc_hw::{combine, AcState, Battery, DmiInfo, PowerSnapshot, Profile, SysRoot};
use pbc_store::Sample;

pub struct Hw {
    pub root: SysRoot,
    pub profile: Option<Profile>,
}

impl Hw {
    pub fn probe(root: SysRoot, extra_profiles: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let mut ps = Profile::builtin();
        if let Some(d) = extra_profiles {
            ps.extend(Profile::load_dir(d)?);
        }
        let dmi = DmiInfo::read(&root);
        let profile = Profile::detect(&ps, &dmi, &root).cloned();
        let hw = Self { root, profile };
        let names: Vec<String> = hw.batteries().into_iter().map(|b| b.name).collect();
        tracing::info!(
            vendor = %dmi.sys_vendor, model = %dmi.product_version,
            profile = hw.profile.as_ref().map(|p| p.id.as_str()).unwrap_or("none"),
            batteries = %if names.is_empty() { "none".to_string() } else { names.join(",") },
            "hardware probed"
        );
        Ok(hw)
    }

    /// The machine's batteries right now. Re-read every call, so a hot swap is picked up.
    pub fn batteries(&self) -> Vec<Battery> {
        Battery::discover(&self.root).unwrap_or_default()
    }

    /// The first battery's name. UPower and the quiet-wake plan are keyed by it.
    pub fn primary(&self) -> Option<String> {
        self.batteries().into_iter().next().map(|b| b.name)
    }

    pub fn combined(&self) -> Option<PowerSnapshot> {
        let snaps: Vec<PowerSnapshot> = self.batteries().iter().map(|b| b.snapshot()).collect();
        (!snaps.is_empty()).then(|| combine(&snaps))
    }

    pub fn online(&self) -> bool {
        pbc_hw::battery::ac_state(&self.root) == AcState::Online
    }

    pub fn capacity(&self) -> Option<u8> {
        self.combined()?.capacity_pct
    }

    pub fn sample(&self, ts: i64) -> Sample {
        let online = self.online();
        match self.combined() {
            Some(s) => Sample {
                ts,
                online,
                status: Some(format!("{:?}", s.status)),
                capacity: s.capacity_pct,
                energy_uwh: s.energy_now_uwh,
                power_uw: s.power_now_uw,
                th_start: s.thresholds.map(|t| t.start),
                th_end: s.thresholds.map(|t| t.end),
            },
            None => Sample {
                ts,
                online,
                status: None,
                capacity: None,
                energy_uwh: None,
                power_uw: None,
                th_start: None,
                th_end: None,
            },
        }
    }
}
