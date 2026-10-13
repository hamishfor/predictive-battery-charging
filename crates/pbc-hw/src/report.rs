//! Hardware reports: what one laptop model exposes and what it was seen to do.
//!
//! `pbcctl report` produces these, and `pbcctl report --write` merges them into
//! `hardware/<vendor>/<model>.yaml`, one file per model with one entry per
//! machine that reported. Tests replay every committed file against the
//! builtin profiles, so a report doubles as a device-support test.
//!
//! A report holds model facts only: DMI identity, software versions, the
//! charge-control files, battery health, self-check results and the learned
//! charge curve. No serial numbers, host names, usage history or battery wear.

use crate::sysfs::SysRoot;
use crate::{Battery, DmiInfo, Profile};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SCHEMA: u32 = 1;

/// Everything known about one model: `hardware/<vendor>/<model>.yaml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelFile {
    pub schema: u32,
    /// DMI `sys_vendor`.
    pub vendor: String,
    /// DMI `product_family`.
    pub family: String,
    /// [`DmiInfo::model`]: `product_name`, or `product_version` on Lenovo.
    /// Names the file, and is what profiles list in `[quiet_wake] validated`.
    pub model: String,
    /// The profile that matched, or none.
    pub profile: Option<String>,
    pub reports: Vec<Report>,
}

/// One machine's report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// When the report was written, as YYYY-MM-DD.
    pub date: String,
    /// pbcctl version that wrote it.
    pub pbcctl: String,
    /// DMI `product_name`, which `dmidecode -s system-product-name` prints.
    pub product: String,
    /// DMI `product_version`. On Lenovo it holds the model name, elsewhere often a revision.
    #[serde(default)]
    pub version: String,
    pub bios: String,
    #[serde(default)]
    pub software: BTreeMap<String, String>,
    /// Entries of `/sys/power/mem_sleep`. The active one is listed first.
    #[serde(default)]
    pub mem_sleep: Vec<String>,
    pub rtc_wakealarm: bool,
    /// Loaded kernel modules that some profile matches on.
    #[serde(default)]
    pub drivers: Vec<String>,
    pub batteries: BTreeMap<String, BatteryReport>,
    /// Non-battery power supplies, by name, with their `type`.
    #[serde(default)]
    pub other_supplies: BTreeMap<String, String>,
    /// Self-check results, keyed by capability (`enforce`, `resume`, `persist`, `quiet_wake`).
    #[serde(default)]
    pub checks: BTreeMap<String, Check>,
    /// Learned minutes per 5% band, such as `80-85: 5.5`. Only bands this machine has learned.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub charge_curve_min: BTreeMap<String, f64>,
    /// Free text: firmware quirks, what was seen or heard during the wake test.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatteryReport {
    pub start_threshold: bool,
    pub end_threshold: bool,
    /// `charge_behaviour` values the driver offers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub behaviours: Vec<String>,
    /// `scope` from sysfs, if the driver sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Design capacity. Wear and cycle count describe one unit, not the model, so they're left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_wh: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckVerdict {
    Working,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// Never judged.
    None,
    /// Judged by pbcd during normal use.
    Observed,
    /// Confirmed by a person in a guided test.
    Confirmed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub verdict: CheckVerdict,
    pub evidence: Evidence,
    #[serde(default)]
    pub passes: u32,
    #[serde(default)]
    pub failures: u32,
    /// Date of the last observation, as YYYY-MM-DD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
}

impl Check {
    /// Is this a stronger result than `other`? Confirmed beats observed, then more observations win.
    fn stronger_than(&self, other: &Check) -> bool {
        (self.evidence, self.passes + self.failures)
            > (other.evidence, other.passes + other.failures)
    }
}

/// Lower-case, with runs of anything but letters and digits turned into one `-`.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

impl ModelFile {
    /// A stand-alone file name, such as `lenovo-thinkpad-x1-carbon-gen-13.yaml`.
    pub fn file_name(&self) -> String {
        format!("{}-{}.yaml", slug(&self.vendor), slug(&self.model))
    }

    /// Where this model's file lives under the repository's `hardware/` directory.
    pub fn path_in(&self, dir: &Path) -> PathBuf {
        dir.join(slug(&self.vendor))
            .join(format!("{}.yaml", slug(&self.model)))
    }

    pub fn parse(src: &str) -> anyhow::Result<Self> {
        Ok(serde_norway::from_str(src)?)
    }

    pub fn to_yaml(&self) -> anyhow::Result<String> {
        Ok(serde_norway::to_string(self)?)
    }

    /// Add a report. A report from the same machine type and BIOS replaces the
    /// old one, but each check, the curve and the notes keep the stronger or
    /// non-empty value, so a quick re-run never discards a confirmed result.
    pub fn merge(&mut self, mut new: Report, profile: Option<String>) {
        self.profile = profile;
        match self
            .reports
            .iter_mut()
            .find(|r| r.product == new.product && r.bios == new.bios)
        {
            Some(old) => {
                for (k, c) in &old.checks {
                    if new.checks.get(k).is_none_or(|n| c.stronger_than(n)) {
                        new.checks.insert(k.clone(), c.clone());
                    }
                }
                if new.charge_curve_min.is_empty() {
                    new.charge_curve_min = std::mem::take(&mut old.charge_curve_min);
                }
                if new.notes.is_empty() {
                    new.notes = std::mem::take(&mut old.notes);
                }
                *old = new;
            }
            None => self.reports.push(new),
        }
    }

    /// Load every `*.yaml` under `dir`, recursively, with its path.
    pub fn load_all(dir: &Path) -> anyhow::Result<Vec<(PathBuf, ModelFile)>> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d)? {
                let p = e?.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|x| x.to_str()) == Some("yaml") {
                    let f = ModelFile::parse(&std::fs::read_to_string(&p)?)
                        .map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
                    out.push((p, f));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

/// The parts of a report that come from sysfs. The caller adds the date,
/// software versions, self-checks and charge curve.
pub fn collect(root: &SysRoot, profiles: &[Profile]) -> (ModelFile, Report) {
    let dmi = DmiInfo::read(root);
    let family = root
        .read_opt("class/dmi/id/product_family")
        .unwrap_or_default();
    let mut drivers: Vec<String> = profiles
        .iter()
        .filter_map(|p| p.matcher.kernel_module.clone())
        .filter(|m| root.exists(format!("module/{m}")))
        .collect();
    drivers.sort();
    drivers.dedup();

    // The active state, shown as `[s2idle]`, goes first.
    let mut mem_sleep: Vec<(bool, String)> = root
        .read_opt("power/mem_sleep")
        .unwrap_or_default()
        .split_whitespace()
        .map(|t| (t.starts_with('['), t.trim_matches(['[', ']']).to_string()))
        .collect();
    mem_sleep.sort_by_key(|(active, _)| !active);

    let mut batteries = BTreeMap::new();
    for b in Battery::discover(root).unwrap_or_default() {
        let s = b.snapshot();
        let behaviours = root
            .read_opt(b.attr("charge_behaviour"))
            .unwrap_or_default()
            .split_whitespace()
            .map(|t| t.trim_matches(['[', ']']).to_string())
            .collect();
        let wh = |u: u64| (u as f64 / 1e5).round() / 10.0;
        batteries.insert(
            b.name.clone(),
            BatteryReport {
                start_threshold: root.exists(b.attr("charge_control_start_threshold")),
                end_threshold: root.exists(b.attr("charge_control_end_threshold")),
                behaviours,
                scope: root.read_opt(b.attr("scope")),
                design_wh: s.energy_full_design_uwh.map(wh),
            },
        );
    }
    let other_supplies = root
        .list("class/power_supply")
        .unwrap_or_default()
        .into_iter()
        .filter(|n| !batteries.contains_key(n))
        .filter_map(|n| {
            let ty = root.read_opt(format!("class/power_supply/{n}/type"))?;
            (ty != "Battery").then_some((n, ty))
        })
        .collect();

    let profile = Profile::detect(profiles, &dmi, root).map(|p| p.id.clone());
    let file = ModelFile {
        schema: SCHEMA,
        vendor: dmi.sys_vendor.clone(),
        family,
        model: dmi.model().to_string(),
        profile,
        reports: Vec::new(),
    };
    let report = Report {
        date: String::new(),
        pbcctl: String::new(),
        product: dmi.product_name.clone(),
        version: dmi.product_version.clone(),
        // Lenovo pads the version: "N4BET77W (1.47 )".
        bios: dmi
            .bios_version
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace(" )", ")"),
        software: BTreeMap::new(),
        mem_sleep: mem_sleep.into_iter().map(|(_, s)| s).collect(),
        rtc_wakealarm: root.exists("class/rtc/rtc0/wakealarm"),
        drivers,
        batteries,
        other_supplies,
        checks: BTreeMap::new(),
        charge_curve_min: BTreeMap::new(),
        notes: String::new(),
    };
    (file, report)
}

/// Build a fake sysfs tree from a report, for tests: DMI, drivers and batteries.
pub fn write_fake_sysfs(file: &ModelFile, r: &Report, dir: &Path) -> std::io::Result<()> {
    let put = |rel: &str, v: &str| -> std::io::Result<()> {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, format!("{v}\n"))
    };
    put("class/dmi/id/sys_vendor", &file.vendor)?;
    put("class/dmi/id/product_family", &file.family)?;
    put("class/dmi/id/product_name", &r.product)?;
    put("class/dmi/id/product_version", &r.version)?;
    put("class/dmi/id/bios_version", &r.bios)?;
    for d in &r.drivers {
        std::fs::create_dir_all(dir.join("module").join(d))?;
    }
    for (name, ty) in &r.other_supplies {
        put(&format!("class/power_supply/{name}/type"), ty)?;
    }
    for (name, b) in &r.batteries {
        let a = |f: &str| format!("class/power_supply/{name}/{f}");
        put(&a("type"), "Battery")?;
        put(&a("present"), "1")?;
        put(&a("status"), "Discharging")?;
        put(&a("capacity"), "50")?;
        if let Some(s) = &b.scope {
            put(&a("scope"), s)?;
        }
        if b.start_threshold {
            put(&a("charge_control_start_threshold"), "0")?;
        }
        if b.end_threshold {
            put(&a("charge_control_end_threshold"), "100")?;
        }
        if !b.behaviours.is_empty() {
            let mut v = b.behaviours.clone();
            v[0] = format!("[{}]", v[0]);
            put(&a("charge_behaviour"), &v.join(" "))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(
            slug("ThinkPad X1 Carbon Gen 13"),
            "thinkpad-x1-carbon-gen-13"
        );
        assert_eq!(slug(" LENOVO "), "lenovo");
        assert_eq!(slug("XPS 13 9340 (2024)"), "xps-13-9340-2024");
    }

    fn check(evidence: Evidence, passes: u32) -> Check {
        Check {
            verdict: CheckVerdict::Working,
            evidence,
            passes,
            failures: 0,
            last: None,
            last_failure: None,
        }
    }

    fn report(product: &str, checks: &[(&str, Check)]) -> Report {
        Report {
            date: "2026-10-13".into(),
            pbcctl: "0.1.0".into(),
            product: product.into(),
            version: String::new(),
            bios: "1.0".into(),
            software: BTreeMap::new(),
            mem_sleep: vec!["s2idle".into()],
            rtc_wakealarm: true,
            drivers: vec![],
            batteries: BTreeMap::new(),
            other_supplies: BTreeMap::new(),
            checks: checks
                .iter()
                .map(|(k, c)| (k.to_string(), c.clone()))
                .collect(),
            charge_curve_min: BTreeMap::new(),
            notes: String::new(),
        }
    }

    #[test]
    fn merge_keeps_stronger_checks_and_appends_other_machines() {
        let mut f = ModelFile {
            schema: SCHEMA,
            vendor: "V".into(),
            family: "F".into(),
            model: "M".into(),
            profile: None,
            reports: vec![report(
                "A",
                &[("quiet_wake", check(Evidence::Confirmed, 1))],
            )],
        };
        f.reports[0].notes = "fan stayed off".into();
        // Same machine type, re-run with nothing confirmed: keeps the confirmed check and notes.
        f.merge(
            report(
                "A",
                &[
                    ("quiet_wake", check(Evidence::None, 0)),
                    ("enforce", check(Evidence::Observed, 3)),
                ],
            ),
            Some("p".into()),
        );
        assert_eq!(f.reports.len(), 1);
        assert_eq!(
            f.reports[0].checks["quiet_wake"].evidence,
            Evidence::Confirmed
        );
        assert_eq!(f.reports[0].checks["enforce"].passes, 3);
        assert_eq!(f.reports[0].notes, "fan stayed off");
        assert_eq!(f.profile.as_deref(), Some("p"));
        // Another machine type: appended.
        f.merge(report("B", &[]), Some("p".into()));
        assert_eq!(f.reports.len(), 2);
        // Round-trips through YAML.
        assert_eq!(ModelFile::parse(&f.to_yaml().unwrap()).unwrap(), f);
    }
}
