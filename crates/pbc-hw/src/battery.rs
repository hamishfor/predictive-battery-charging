use crate::sysfs::SysRoot;
use crate::thresholds::Thresholds;
use std::io;

const PSY: &str = "class/power_supply";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum BatteryStatus {
    Charging,
    Discharging,
    NotCharging,
    Full,
    Unknown,
}

impl BatteryStatus {
    fn parse(s: &str) -> Self {
        match s {
            "Charging" => Self::Charging,
            "Discharging" => Self::Discharging,
            "Not charging" => Self::NotCharging,
            "Full" => Self::Full,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcState {
    Online,
    Offline,
    Unknown,
}

/// One battery exposed under `/sys/class/power_supply/<name>`.
#[derive(Debug, Clone)]
pub struct Battery {
    pub name: String,
    root: SysRoot,
}

/// A point-in-time reading of the battery and charger.
#[derive(Debug, Clone)]
pub struct PowerSnapshot {
    pub battery: String,
    pub status: BatteryStatus,
    pub ac: AcState,
    pub capacity_pct: Option<u8>,
    pub energy_now_uwh: Option<u64>,
    pub energy_full_uwh: Option<u64>,
    pub energy_full_design_uwh: Option<u64>,
    pub power_now_uw: Option<u64>,
    pub cycle_count: Option<u64>,
    pub thresholds: Option<Thresholds>,
    pub behaviour: Option<String>,
}

impl Battery {
    /// The machine's own batteries, sorted by name.
    ///
    /// Skips `scope == Device` supplies (peripherals such as a Bluetooth mouse) and
    /// batteries with `present == 0`, such as a pulled removable second battery.
    /// It's a cheap directory listing, so call it whenever the set may have changed.
    pub fn discover(root: &SysRoot) -> io::Result<Vec<Battery>> {
        Ok(root
            .list(PSY)?
            .into_iter()
            .filter(|n| root.read_opt(format!("{PSY}/{n}/type")).as_deref() == Some("Battery"))
            .filter(|n| root.read_opt(format!("{PSY}/{n}/scope")).as_deref() != Some("Device"))
            .filter(|n| root.read_opt(format!("{PSY}/{n}/present")).as_deref() != Some("0"))
            .map(|name| Battery {
                name,
                root: root.clone(),
            })
            .collect())
    }

    pub fn attr(&self, a: &str) -> String {
        format!("{PSY}/{}/{a}", self.name)
    }

    fn u64(&self, a: &str) -> Option<u64> {
        self.root.read_u64(self.attr(a))
    }

    pub fn has_thresholds(&self) -> bool {
        self.root.exists(self.attr("charge_control_end_threshold"))
    }

    pub fn thresholds(&self) -> Option<Thresholds> {
        Some(Thresholds {
            start: self.u64("charge_control_start_threshold")? as u8,
            end: self.u64("charge_control_end_threshold")? as u8,
        })
    }

    /// The active value of `charge_behaviour`, e.g. "auto" from "[auto] inhibit-charge".
    pub fn behaviour(&self) -> Option<String> {
        let raw = self.root.read_opt(self.attr("charge_behaviour"))?;
        raw.split_whitespace()
            .find(|t| t.starts_with('['))
            .map(|t| t.trim_matches(|c| c == '[' || c == ']').to_string())
    }

    pub fn snapshot(&self) -> PowerSnapshot {
        PowerSnapshot {
            battery: self.name.clone(),
            status: self
                .root
                .read_opt(self.attr("status"))
                .map(|s| BatteryStatus::parse(&s))
                .unwrap_or(BatteryStatus::Unknown),
            ac: ac_state(&self.root),
            capacity_pct: self.u64("capacity").map(|v| v.min(100) as u8),
            energy_now_uwh: self.u64("energy_now"),
            energy_full_uwh: self.u64("energy_full"),
            energy_full_design_uwh: self.u64("energy_full_design"),
            power_now_uw: self.u64("power_now"),
            cycle_count: self.u64("cycle_count"),
            thresholds: self.thresholds(),
            behaviour: self.behaviour(),
        }
    }
}

/// AC is online if any `Mains` supply is online. USB-C (UCSI) supplies are
/// reported separately and also count, since many laptops charge over USB-C.
pub fn ac_state(root: &SysRoot) -> AcState {
    let Ok(names) = root.list(PSY) else {
        return AcState::Unknown;
    };
    let mut seen = false;
    for n in names {
        let ty = root.read_opt(format!("{PSY}/{n}/type"));
        if !matches!(ty.as_deref(), Some("Mains") | Some("USB")) {
            continue;
        }
        match root.read_opt(format!("{PSY}/{n}/online")).as_deref() {
            Some("1") => return AcState::Online,
            Some(_) => seen = true,
            None => {}
        }
    }
    if seen {
        AcState::Offline
    } else {
        AcState::Unknown
    }
}

/// Several batteries seen as one, the way UPower's DisplayDevice combines them.
///
/// The capacity is energy-weighted, so a 23 Wh second battery counts for less than a
/// 57 Wh internal one. If any battery lacks energy readings, it falls back to the
/// mean of the reported percentages. Status is `Charging` if any battery is
/// charging. Firmware usually charges one battery at a time.
pub fn combine(snaps: &[PowerSnapshot]) -> PowerSnapshot {
    let ac = snaps.first().map(|s| s.ac).unwrap_or(AcState::Unknown);
    let sum = |f: fn(&PowerSnapshot) -> Option<u64>| -> Option<u64> {
        snaps
            .iter()
            .map(f)
            .try_fold(0u64, |acc, v| v.map(|v| acc + v))
    };
    let (now, full) = (sum(|s| s.energy_now_uwh), sum(|s| s.energy_full_uwh));
    let capacity_pct = match (now, full) {
        (Some(n), Some(f)) if f > 0 && !snaps.is_empty() => {
            Some(((n as f64 / f as f64) * 100.0).round().min(100.0) as u8)
        }
        _ => {
            let caps: Vec<u8> = snaps.iter().filter_map(|s| s.capacity_pct).collect();
            if caps.is_empty() {
                None
            } else {
                Some((caps.iter().map(|&c| c as u32).sum::<u32>() / caps.len() as u32) as u8)
            }
        }
    };
    let status = if snaps.iter().any(|s| s.status == BatteryStatus::Charging) {
        BatteryStatus::Charging
    } else if !snaps.is_empty() && snaps.iter().all(|s| s.status == BatteryStatus::Full) {
        BatteryStatus::Full
    } else if snaps.iter().any(|s| s.status == BatteryStatus::Discharging) {
        BatteryStatus::Discharging
    } else if snaps.iter().any(|s| s.status == BatteryStatus::NotCharging) {
        BatteryStatus::NotCharging
    } else {
        BatteryStatus::Unknown
    };
    PowerSnapshot {
        battery: snaps
            .iter()
            .map(|s| s.battery.as_str())
            .collect::<Vec<_>>()
            .join("+"),
        status,
        ac,
        capacity_pct,
        energy_now_uwh: now,
        energy_full_uwh: full,
        energy_full_design_uwh: sum(|s| s.energy_full_design_uwh),
        power_now_uw: sum(|s| s.power_now_uw),
        cycle_count: snaps.iter().filter_map(|s| s.cycle_count).max(),
        // Only meaningful if every battery agrees.
        thresholds: snaps
            .first()
            .and_then(|f| f.thresholds)
            .filter(|t| snaps.iter().all(|s| s.thresholds == Some(*t))),
        behaviour: None,
    }
}
