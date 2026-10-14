use crate::sysfs::SysRoot;
use serde::Deserialize;
use std::path::Path;

/// DMI identity strings from `/sys/class/dmi/id`.
#[derive(Debug, Clone, Default)]
pub struct DmiInfo {
    pub sys_vendor: String,
    pub product_name: String,
    pub product_version: String,
    pub bios_version: String,
}

impl DmiInfo {
    pub fn read(root: &SysRoot) -> Self {
        let r = |a: &str| {
            root.read_opt(format!("class/dmi/id/{a}"))
                .unwrap_or_default()
        };
        Self {
            sys_vendor: r("sys_vendor"),
            product_name: r("product_name"),
            product_version: r("product_version"),
            bios_version: r("bios_version"),
        }
    }

    /// The model's marketing name, such as "ThinkPad X1 Carbon Gen 13" or "XPS 13 9340".
    ///
    /// Most vendors put it in `product_name` (what `dmidecode -s system-product-name`
    /// prints). Lenovo puts a machine type such as `21NSS18R00` there, which differs
    /// between regional variants of one model, and the name in `product_version`.
    pub fn model(&self) -> &str {
        if self.sys_vendor.eq_ignore_ascii_case("LENOVO") && !self.product_version.is_empty() {
            &self.product_version
        } else {
            &self.product_name
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(rename = "match")]
    pub matcher: Matcher,
    pub thresholds: ThresholdRules,
    #[serde(default)]
    pub behaviour: BehaviourRules,
    pub defaults: Defaults,
    #[serde(default)]
    pub quiet_wake: QuietWakeRules,
    /// Friendly names for batteries, shown in the UI, keyed by kernel name.
    /// Only used when every battery present has a label.
    #[serde(default)]
    pub battery_labels: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuietWakeRules {
    /// Models where the quiet wake was validated, as [`DmiInfo::model`] gives them.
    #[serde(default)]
    pub validated: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Matcher {
    #[serde(default)]
    pub sys_vendor: Vec<String>,
    #[serde(default)]
    pub product_version_contains: Vec<String>,
    pub kernel_module: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThresholdRules {
    pub start_min: u8,
    pub start_max: u8,
    pub end_min: u8,
    pub end_max: u8,
    pub min_gap: u8,
    #[serde(default)]
    pub persists_poweroff: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BehaviourRules {
    #[serde(default)]
    pub supported: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Defaults {
    pub hold_end: u8,
    pub hold_start: u8,
}

/// Profiles compiled into the binary. Extra profiles can be loaded from disk.
pub const BUILTIN: &[&str] = &[include_str!("../../../profiles/lenovo-thinkpad.toml")];

impl Profile {
    pub fn parse(src: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(src)?)
    }

    pub fn builtin() -> Vec<Profile> {
        BUILTIN
            .iter()
            .map(|s| Profile::parse(s).expect("builtin profile must parse"))
            .collect()
    }

    pub fn load_dir(dir: &Path) -> anyhow::Result<Vec<Profile>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.extension().and_then(|x| x.to_str()) == Some("toml") {
                out.push(Profile::parse(&std::fs::read_to_string(&p)?)?);
            }
        }
        Ok(out)
    }

    pub fn matches(&self, dmi: &DmiInfo, root: &SysRoot) -> bool {
        let m = &self.matcher;
        let vendor_ok = m.sys_vendor.is_empty()
            || m.sys_vendor
                .iter()
                .any(|v| v.eq_ignore_ascii_case(&dmi.sys_vendor));
        let version_ok = m.product_version_contains.is_empty()
            || m.product_version_contains
                .iter()
                .any(|v| dmi.product_version.contains(v.as_str()));
        let module_ok = m
            .kernel_module
            .as_ref()
            .is_none_or(|k| root.exists(format!("module/{k}")));
        vendor_ok && version_ok && module_ok
    }

    /// First matching profile, or `None` (fail closed: collect-only mode).
    pub fn detect<'a>(
        profiles: &'a [Profile],
        dmi: &DmiInfo,
        root: &SysRoot,
    ) -> Option<&'a Profile> {
        profiles.iter().find(|p| p.matches(dmi, root))
    }
}
