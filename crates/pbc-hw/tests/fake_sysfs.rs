use pbc_hw::thresholds::{apply, Which};
use pbc_hw::*;
use std::fs;
use std::path::Path;

fn put(root: &Path, rel: &str, v: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, v).unwrap();
}

/// A fake tree modelled on the X1 Carbon Gen 13.
fn x1c13(dir: &Path) {
    put(dir, "class/dmi/id/sys_vendor", "LENOVO\n");
    put(
        dir,
        "class/dmi/id/product_version",
        "ThinkPad X1 Carbon Gen 13\n",
    );
    put(dir, "class/dmi/id/product_name", "21NSS18R00\n");
    fs::create_dir_all(dir.join("module/thinkpad_acpi")).unwrap();
    put(dir, "class/power_supply/AC/type", "Mains\n");
    put(dir, "class/power_supply/AC/online", "1\n");
    let b = "class/power_supply/BAT0";
    put(dir, &format!("{b}/type"), "Battery\n");
    put(dir, &format!("{b}/status"), "Charging\n");
    put(dir, &format!("{b}/capacity"), "64\n");
    put(dir, &format!("{b}/energy_now"), "33000000\n");
    put(dir, &format!("{b}/energy_full"), "52420000\n");
    put(dir, &format!("{b}/charge_control_start_threshold"), "0\n");
    put(dir, &format!("{b}/charge_control_end_threshold"), "100\n");
    put(
        dir,
        &format!("{b}/charge_behaviour"),
        "[auto] inhibit-charge force-discharge\n",
    );
    put(
        dir,
        "class/power_supply/ucsi-source-psy-USBC000:001/type",
        "USB\n",
    );
    put(
        dir,
        "class/power_supply/ucsi-source-psy-USBC000:001/online",
        "0\n",
    );
}

#[test]
fn detects_thinkpad_and_reads_snapshot() {
    let t = tempfile::tempdir().unwrap();
    x1c13(t.path());
    let root = SysRoot::new(t.path());
    let profiles = Profile::builtin();
    let p = Profile::detect(&profiles, &DmiInfo::read(&root), &root).expect("should match");
    assert_eq!(p.id, "lenovo-thinkpad");

    let bats = Battery::discover(&root).unwrap();
    assert_eq!(bats.len(), 1);
    let s = bats[0].snapshot();
    assert_eq!(s.capacity_pct, Some(64));
    assert_eq!(s.ac, AcState::Online);
    assert_eq!(s.status, BatteryStatus::Charging);
    assert_eq!(s.behaviour.as_deref(), Some("auto"));
    assert_eq!(s.thresholds, Some(Thresholds { start: 0, end: 100 }));
}

#[test]
fn no_match_without_module_or_vendor() {
    let t = tempfile::tempdir().unwrap();
    x1c13(t.path());
    fs::remove_dir(t.path().join("module/thinkpad_acpi")).unwrap();
    let root = SysRoot::new(t.path());
    let profiles = Profile::builtin();
    assert!(Profile::detect(&profiles, &DmiInfo::read(&root), &root).is_none());

    put(t.path(), "class/dmi/id/sys_vendor", "Dell Inc.\n");
    fs::create_dir_all(t.path().join("module/thinkpad_acpi")).unwrap();
    assert!(Profile::detect(&profiles, &DmiInfo::read(&root), &root).is_none());
}

#[test]
fn applies_thresholds_round_trip() {
    let t = tempfile::tempdir().unwrap();
    x1c13(t.path());
    let root = SysRoot::new(t.path());
    let p = &Profile::builtin()[0];
    let bat = &Battery::discover(&root).unwrap()[0];
    let hold = Thresholds { start: 75, end: 80 };
    assert_eq!(apply(&root, bat, hold, &p.thresholds).unwrap(), hold);
    let full = Thresholds { start: 0, end: 100 };
    assert_eq!(apply(&root, bat, full, &p.thresholds).unwrap(), full);
}

#[test]
fn rejects_invalid_thresholds() {
    let p = &Profile::builtin()[0];
    assert!(Thresholds { start: 80, end: 80 }
        .validate(&p.thresholds)
        .is_err());
    assert!(Thresholds { start: 90, end: 80 }
        .validate(&p.thresholds)
        .is_err());
    assert!(Thresholds { start: 10, end: 0 }
        .validate(&p.thresholds)
        .is_err());
    assert!(Thresholds { start: 0, end: 100 }
        .validate(&p.thresholds)
        .is_ok());
}

/// Every intermediate state of every planned write sequence must satisfy start < end.
#[test]
fn write_order_never_violates_gap() {
    let gap = 1;
    for cs in 0..=99u8 {
        for ce in (cs + gap)..=100 {
            for ts in (0..=99u8).step_by(7) {
                for te in ((ts + gap)..=100).step_by(5) {
                    let (mut s, mut e) = (cs, ce);
                    let target = Thresholds { start: ts, end: te };
                    for (w, v) in plan_writes(Thresholds { start: cs, end: ce }, target, gap) {
                        match w {
                            Which::Start => s = v,
                            Which::End => e = v,
                        }
                        assert!(s + gap <= e, "{cs}/{ce} -> {ts}/{te}: hit {s}/{e}");
                    }
                    assert_eq!((s, e), (ts, te));
                }
            }
        }
    }
}

/// A Power Bridge ThinkPad (e.g. T480): internal BAT0 plus a removable rear BAT1,
/// with a Bluetooth mouse battery reported through the same class.
fn power_bridge(dir: &Path) {
    x1c13(dir);
    put(dir, "class/dmi/id/product_version", "ThinkPad T480\n");
    put(dir, "class/power_supply/BAT0/energy_now", "40000000\n");
    put(dir, "class/power_supply/BAT0/energy_full", "50000000\n");
    let b = "class/power_supply/BAT1";
    put(dir, &format!("{b}/type"), "Battery\n");
    put(dir, &format!("{b}/present"), "1\n");
    put(dir, &format!("{b}/status"), "Not charging\n");
    put(dir, &format!("{b}/capacity"), "20\n");
    put(dir, &format!("{b}/energy_now"), "4600000\n");
    put(dir, &format!("{b}/energy_full"), "23000000\n");
    put(dir, &format!("{b}/charge_control_start_threshold"), "0\n");
    put(dir, &format!("{b}/charge_control_end_threshold"), "100\n");
    let m = "class/power_supply/hidpp_battery_1";
    put(dir, &format!("{m}/type"), "Battery\n");
    put(dir, &format!("{m}/scope"), "Device\n");
    put(dir, &format!("{m}/capacity"), "5\n");
}

#[test]
fn discover_skips_peripheral_and_absent_batteries() {
    let t = tempfile::tempdir().unwrap();
    power_bridge(t.path());
    let root = SysRoot::new(t.path());
    let names: Vec<String> = Battery::discover(&root)
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(names, ["BAT0", "BAT1"], "mouse battery excluded");
    // Rear battery pulled out: the kernel keeps the node with present=0.
    put(t.path(), "class/power_supply/BAT1/present", "0\n");
    let names: Vec<String> = Battery::discover(&root)
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(names, ["BAT0"]);
}

#[test]
fn combined_capacity_is_energy_weighted() {
    let t = tempfile::tempdir().unwrap();
    power_bridge(t.path());
    let root = SysRoot::new(t.path());
    let snaps: Vec<_> = Battery::discover(&root)
        .unwrap()
        .iter()
        .map(|b| b.snapshot())
        .collect();
    let c = combine(&snaps);
    // (40 + 4.6) / (50 + 23) Wh = 61%, not the mean of 64% and 20%.
    assert_eq!(c.capacity_pct, Some(61));
    assert_eq!(c.status, BatteryStatus::Charging, "BAT0 is charging");
    assert_eq!(c.battery, "BAT0+BAT1");
    assert_eq!(c.thresholds, Some(Thresholds { start: 0, end: 100 }));
    // Thresholds that differ aren't reported as one value.
    put(
        t.path(),
        "class/power_supply/BAT1/charge_control_end_threshold",
        "80\n",
    );
    let snaps: Vec<_> = Battery::discover(&root)
        .unwrap()
        .iter()
        .map(|b| b.snapshot())
        .collect();
    assert_eq!(combine(&snaps).thresholds, None);
}

#[test]
fn combined_single_battery_matches_it() {
    let t = tempfile::tempdir().unwrap();
    x1c13(t.path());
    let root = SysRoot::new(t.path());
    let s = Battery::discover(&root).unwrap()[0].snapshot();
    let c = combine(std::slice::from_ref(&s));
    assert_eq!(c.capacity_pct, Some(63), "33/52.42 Wh");
    assert_eq!(c.thresholds, s.thresholds);
}

#[test]
fn model_name_comes_from_the_right_dmi_field() {
    let dmi = |vendor: &str, name: &str, version: &str| DmiInfo {
        sys_vendor: vendor.into(),
        product_name: name.into(),
        product_version: version.into(),
        bios_version: String::new(),
    };
    assert_eq!(
        dmi("LENOVO", "21NSS18R00", "ThinkPad X1 Carbon Gen 13").model(),
        "ThinkPad X1 Carbon Gen 13"
    );
    assert_eq!(dmi("Dell Inc.", "XPS 13 9340", "A0").model(), "XPS 13 9340");
    assert_eq!(
        dmi("Framework", "Laptop 13 (AMD Ryzen 7040Series)", "A6").model(),
        "Laptop 13 (AMD Ryzen 7040Series)"
    );
}
