//! Every report in `hardware/` is a device-support test: its fake sysfs tree
//! must match the profile it names, and every `[quiet_wake] validated` model
//! must have a report that confirms the quiet wake.

use pbc_hw::report::{write_fake_sysfs, CheckVerdict, Evidence, ModelFile};
use pbc_hw::*;
use std::path::Path;

fn reports() -> Vec<(std::path::PathBuf, ModelFile)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hardware");
    ModelFile::load_all(&dir).expect("hardware/ reports must parse")
}

#[test]
fn there_are_reports() {
    assert!(!reports().is_empty());
}

#[test]
fn files_are_where_their_model_says() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hardware");
    for (path, f) in reports() {
        assert_eq!(
            path,
            f.path_in(&dir),
            "rename it, or fix vendor/model inside"
        );
        assert_eq!(f.schema, report::SCHEMA, "{}", path.display());
        assert!(!f.reports.is_empty(), "{}: no reports", path.display());
    }
}

#[test]
fn each_report_matches_its_profile() {
    let profiles = Profile::builtin();
    for (path, f) in reports() {
        for r in &f.reports {
            let t = tempfile::tempdir().unwrap();
            write_fake_sysfs(&f, r, t.path()).unwrap();
            let root = SysRoot::new(t.path());
            let got =
                Profile::detect(&profiles, &DmiInfo::read(&root), &root).map(|p| p.id.clone());
            assert_eq!(got, f.profile, "{} ({})", path.display(), r.product);
            if got.is_some() {
                let bats = Battery::discover(&root).unwrap();
                assert_eq!(
                    bats.iter().map(|b| b.name.clone()).collect::<Vec<_>>(),
                    r.batteries.keys().cloned().collect::<Vec<_>>(),
                    "{}: batteries pbcd would control",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn validated_models_have_a_confirmed_quiet_wake() {
    let all = reports();
    for p in Profile::builtin() {
        for model in &p.quiet_wake.validated {
            let ok = all.iter().any(|(_, f)| {
                f.model == *model
                    && f.profile.as_deref() == Some(p.id.as_str())
                    && f.reports.iter().any(|r| {
                        r.checks.get("quiet_wake").is_some_and(|c| {
                            c.verdict == CheckVerdict::Working && c.evidence == Evidence::Confirmed
                        })
                    })
            });
            assert!(
                ok,
                "{}: \"{model}\" is validated but no report in hardware/ confirms the quiet wake",
                p.id
            );
        }
    }
}
