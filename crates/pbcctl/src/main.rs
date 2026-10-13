use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use pbc_hw::thresholds::apply;
use pbc_hw::*;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod bus;
mod selftest;

#[derive(Parser)]
#[command(
    name = "pbcctl",
    version,
    about = "Predictive battery charging control"
)]
struct Cli {
    /// Use another sysfs root, such as a fake tree for testing
    #[arg(long, global = true, default_value = "/sys")]
    sysfs: PathBuf,
    /// Extra profile directory
    #[arg(long, global = true)]
    profiles: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show DMI identity, matched profile and capabilities
    Probe,
    /// Print this laptop's hardware report as YAML, with its self-check results
    Report {
        /// Save the report. Inside a clone of the repository (the current directory,
        /// or PATH if it's a directory) it goes to hardware/<vendor>/<model>.yaml.
        /// Elsewhere it's <vendor>-<model>.yaml in that directory. A PATH that is a
        /// file is used as is. An existing file is merged into.
        #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = ".")]
        write: Option<PathBuf>,
        /// Talk to a development daemon on the session bus
        #[arg(long)]
        user: bool,
    },
    /// Guided checks that improve Predictive Battery Charging on this laptop (needs sudo)
    Selftest {
        /// Run one check: enforce, resume or quiet_wake
        #[arg(long)]
        only: Option<String>,
        /// Talk to a development daemon on the session bus
        #[arg(long)]
        user: bool,
        /// Clear every confirmed result, so pbcd relies on what it observes
        #[arg(long)]
        reset: bool,
    },
    /// Show current battery and charger state
    Status,
    /// Set charge thresholds (needs root)
    Set {
        #[arg(long)]
        start: u8,
        #[arg(long)]
        end: u8,
        /// Only this battery (default: all batteries)
        #[arg(long)]
        battery: Option<String>,
        /// Allow writing on hardware with no matching profile
        #[arg(long)]
        force: bool,
    },
    /// Hold at the profile's default ceiling
    Hold,
    /// Charge to 100% (firmware defaults)
    Full,
    /// Log a CSV sample every INTERVAL seconds
    Log {
        #[arg(long, default_value_t = 30)]
        interval: u64,
        /// Stop after this many samples (default: run forever)
        #[arg(long)]
        count: Option<u64>,
    },
    /// Show the daemon's status and learned sessions
    Daemon {
        /// Also list sessions from the last N days
        #[arg(long)]
        sessions: Option<i64>,
        /// Talk to a development daemon on the session bus
        #[arg(long)]
        user: bool,
    },
    /// Charge to 100% this plug-in session, or the next one if on battery
    ChargeNow {
        /// Return to Predictive Battery Charging
        #[arg(long)]
        cancel: bool,
        /// Talk to a development daemon on the session bus
        #[arg(long)]
        user: bool,
    },
}

fn profiles(cli: &Cli) -> Result<Vec<Profile>> {
    let mut ps = Profile::builtin();
    if let Some(d) = &cli.profiles {
        ps.extend(Profile::load_dir(d)?);
    }
    Ok(ps)
}

fn pick_battery(root: &SysRoot, name: Option<&str>) -> Result<Battery> {
    let bats = Battery::discover(root).context("listing power supplies")?;
    match name {
        Some(n) => bats
            .into_iter()
            .find(|b| b.name == n)
            .context("no such battery"),
        None => bats.into_iter().next().context("no battery found"),
    }
}

fn opt<T: std::fmt::Display>(v: Option<T>, unit: &str) -> String {
    v.map(|x| format!("{x}{unit}"))
        .unwrap_or_else(|| "-".into())
}

fn print_status(s: &PowerSnapshot) {
    let wh = |v: Option<u64>| v.map(|x| format!("{:.2} Wh", x as f64 / 1e6));
    let health = match (s.energy_full_uwh, s.energy_full_design_uwh) {
        (Some(f), Some(d)) if d > 0 => format!("{:.1}%", f as f64 * 100.0 / d as f64),
        _ => "-".into(),
    };
    println!("battery:     {}", s.battery);
    println!("status:      {:?}", s.status);
    println!("ac:          {:?}", s.ac);
    println!("capacity:    {}", opt(s.capacity_pct, "%"));
    println!(
        "energy:      {} / {}",
        opt(wh(s.energy_now_uwh), ""),
        opt(wh(s.energy_full_uwh), "")
    );
    println!("health:      {health}");
    println!(
        "power:       {}",
        opt(
            s.power_now_uw.map(|p| format!("{:.2}", p as f64 / 1e6)),
            " W"
        )
    );
    println!("cycles:      {}", opt(s.cycle_count, ""));
    println!("thresholds:  {}", opt(s.thresholds, ""));
    println!("behaviour:   {}", opt(s.behaviour.as_deref(), ""));
}

fn set(cli: &Cli, t: Thresholds, battery: Option<&str>, force: bool) -> Result<()> {
    let root = SysRoot::new(&cli.sysfs);
    let ps = profiles(cli)?;
    let dmi = DmiInfo::read(&root);
    let rules = match Profile::detect(&ps, &dmi, &root) {
        Some(p) => p.thresholds.clone(),
        None if force => {
            eprintln!("warning: no matching profile, using generic rules");
            toml_generic_rules()
        }
        None => bail!("no hardware profile matches this machine (use --force to override)"),
    };
    let bats = match battery {
        Some(_) => vec![pick_battery(&root, battery)?],
        None => Battery::discover(&root)?
            .into_iter()
            .filter(|b| b.has_thresholds())
            .collect(),
    };
    if bats.is_empty() {
        bail!("no battery with charge thresholds found");
    }
    for bat in bats {
        set_one(&root, &bat, t, &rules)?;
    }
    Ok(())
}

fn set_one(
    root: &SysRoot,
    bat: &Battery,
    t: Thresholds,
    rules: &pbc_hw::profile::ThresholdRules,
) -> Result<()> {
    let before = bat.thresholds();
    let after = apply(root, bat, t, rules).map_err(|e| {
        if matches!(&e, ThresholdError::Io { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied) {
            anyhow::anyhow!("{e}\nhint: writing thresholds needs root (try sudo)")
        } else {
            e.into()
        }
    })?;
    println!("{}: {} -> {}", bat.name, opt(before, ""), after);
    if after != t {
        bail!("firmware reports {after}, expected {t}");
    }
    Ok(())
}

fn toml_generic_rules() -> pbc_hw::profile::ThresholdRules {
    pbc_hw::profile::ThresholdRules {
        start_min: 0,
        start_max: 99,
        end_min: 1,
        end_max: 100,
        min_gap: 1,
        persists_poweroff: false,
    }
}

/// The root of a clone of this repository containing `dir`, if any.
fn find_repo(dir: &std::path::Path) -> Option<PathBuf> {
    let dir = std::fs::canonicalize(dir).ok()?;
    dir.ancestors()
        .find(|d| {
            d.join("hardware").is_dir()
                && d.join("profiles").is_dir()
                && d.join("crates/pbc-hw").is_dir()
        })
        .map(|d| d.to_path_buf())
}

/// Under sudo, hand a file we created back to the user who ran sudo, so they can edit and share it.
fn give_to_sudo_user(path: &std::path::Path) {
    let id = |k: &str| std::env::var(k).ok()?.parse::<u32>().ok();
    if let (Some(uid), Some(gid)) = (id("SUDO_UID"), id("SUDO_GID")) {
        let _ = std::os::unix::fs::chown(path, Some(uid), Some(gid));
    }
}

/// A unix time as a local YYYY-MM-DD.
fn date(ts: i64) -> String {
    jiff::Timestamp::from_second(ts)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).date().to_string())
        .unwrap_or_default()
}

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = SysRoot::new(&cli.sysfs);
    match &cli.cmd {
        Cmd::Report { write, user } => report(&cli, *user, write.as_deref())?,
        Cmd::Selftest { only, user, reset } => {
            if *reset {
                bus::call(*user, "ConfirmCapability", &["sb", "", "false"])?;
                println!("Confirmed results cleared.");
            } else {
                selftest::run(
                    &cli.sysfs,
                    selftest::Opts {
                        user_bus: *user,
                        only: only.clone(),
                    },
                )?;
            }
        }
        Cmd::Probe => {
            let dmi = DmiInfo::read(&root);
            println!("vendor:   {}", dmi.sys_vendor);
            println!(
                "model:    {} ({} / {})",
                dmi.model(),
                dmi.product_name,
                dmi.product_version
            );
            println!("bios:     {}", dmi.bios_version);
            let ps = profiles(&cli)?;
            match Profile::detect(&ps, &dmi, &root) {
                Some(p) => {
                    let r = &p.thresholds;
                    println!("profile:  {} ({})", p.id, p.name);
                    println!(
                        "  start {}..={}, end {}..={}, gap {}",
                        r.start_min, r.start_max, r.end_min, r.end_max, r.min_gap
                    );
                    println!(
                        "  hold default: {}/{}",
                        p.defaults.hold_start, p.defaults.hold_end
                    );
                    println!("  behaviours: {}", p.behaviour.supported.join(", "));
                }
                None => println!("profile:  none (collect-only mode)"),
            }
            for b in Battery::discover(&root)? {
                println!(
                    "battery:  {} thresholds={}",
                    b.name,
                    if b.has_thresholds() { "yes" } else { "no" }
                );
            }
            let sleep =
                std::fs::read_to_string(cli.sysfs.join("power/mem_sleep")).unwrap_or_default();
            println!("mem_sleep: {}", sleep.trim());
            println!(
                "rtc wake:  {}",
                cli.sysfs.join("class/rtc/rtc0/wakealarm").exists()
            );
        }
        Cmd::Status => {
            let snaps: Vec<_> = Battery::discover(&root)?
                .iter()
                .map(|b| b.snapshot())
                .collect();
            for (i, s) in snaps.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                print_status(s);
            }
            if snaps.len() > 1 {
                let c = combine(&snaps);
                println!(
                    "\ncombined:    {} ({})",
                    opt(c.capacity_pct, "%"),
                    c.battery
                );
            }
        }
        Cmd::Set {
            start,
            end,
            battery,
            force,
        } => set(
            &cli,
            Thresholds {
                start: *start,
                end: *end,
            },
            battery.as_deref(),
            *force,
        )?,
        Cmd::Hold => {
            let ps = profiles(&cli)?;
            let p = Profile::detect(&ps, &DmiInfo::read(&root), &root)
                .context("no matching profile")?;
            let t = Thresholds {
                start: p.defaults.hold_start,
                end: p.defaults.hold_end,
            };
            set(&cli, t, None, false)?
        }
        Cmd::Full => set(&cli, Thresholds { start: 0, end: 100 }, None, false)?,
        Cmd::Daemon { sessions, user } => {
            let call = |method: &str, extra: &[&str]| bus::call(*user, method, extra);
            let st: serde_json::Value = serde_json::from_str(&call("StatusJson", &[])?)?;
            println!("{}", serde_json::to_string_pretty(&st)?);
            if let Some(days) = sessions {
                let since = (now_s() as i64 - days * 86400).to_string();
                let ss: serde_json::Value =
                    serde_json::from_str(&call("SessionsJson", &["x", &since])?)?;
                println!("{}", serde_json::to_string_pretty(&ss)?);
            }
        }
        Cmd::ChargeNow { cancel, user } => {
            let mut c = std::process::Command::new("busctl");
            if *user {
                c.arg("--user");
            }
            let on = if *cancel { "false" } else { "true" };
            let out = c
                .args([
                    "call",
                    "org.pbc.Daemon1",
                    "/org/pbc/Daemon1",
                    "org.pbc.Daemon1",
                    "ChargeNow",
                    "b",
                    on,
                ])
                .output()
                .context("running busctl")?;
            if !out.status.success() {
                bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
            }
            println!(
                "{}",
                if *cancel {
                    "Predictive Battery Charging resumed"
                } else {
                    "charging to 100% this plug-in session (on battery: the next one)"
                }
            );
        }
        Cmd::Log { interval, count } => {
            let bat = pick_battery(&root, None)?;
            let mut out = std::io::stdout().lock();
            writeln!(
                out,
                "ts,status,ac,capacity,energy_now_uwh,energy_full_uwh,power_uw,start,end"
            )?;
            let mut n = 0;
            loop {
                let s = bat.snapshot();
                let (ts, te) = s.thresholds.map(|t| (t.start, t.end)).unzip();
                writeln!(
                    out,
                    "{},{:?},{:?},{},{},{},{},{},{}",
                    now_s(),
                    s.status,
                    s.ac,
                    opt(s.capacity_pct, ""),
                    opt(s.energy_now_uwh, ""),
                    opt(s.energy_full_uwh, ""),
                    opt(s.power_now_uw, ""),
                    opt(ts, ""),
                    opt(te, "")
                )?;
                out.flush()?;
                n += 1;
                if count.is_some_and(|c| n >= c) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(*interval));
            }
        }
    }
    Ok(())
}

/// Everything needed to judge support for a laptop. Reads only; writes nothing.
fn report(cli: &Cli, user_bus: bool, write: Option<&std::path::Path>) -> Result<()> {
    use pbc_hw::report::{collect, Check, CheckVerdict, Evidence, ModelFile};
    use pbc_model::selfcheck::{Record, Source, Verdict};
    let root = SysRoot::new(&cli.sysfs);
    let ps = profiles(cli)?;
    let (mut file, mut r) = collect(&root, &ps);
    // First line of a command's output, or None if it isn't installed.
    let cmd = |c: &str, a: &[&str]| {
        std::process::Command::new(c)
            .args(a)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                s.lines().next().map(|l| l.trim().to_string())
            })
    };
    r.date = date(now_s() as i64);
    r.pbcctl = env!("CARGO_PKG_VERSION").into();
    let versions = [
        ("kernel", cmd("uname", &["-r"])),
        (
            "systemd",
            cmd("systemctl", &["--version"]).map(|v| v.replace("systemd ", "")),
        ),
        (
            "upower",
            cmd("upower", &["--version"]).map(|v| v.replace("UPower client version ", "")),
        ),
        (
            "gnome_shell",
            cmd("gnome-shell", &["--version"]).map(|v| v.replace("GNOME Shell ", "")),
        ),
    ];
    for (k, v) in versions {
        if let Some(v) = v {
            r.software.insert(k.into(), v);
        }
    }
    match bus::status(user_bus) {
        Ok(st) => {
            let recs: Vec<Record> =
                serde_json::from_value(st["capabilities"].clone()).unwrap_or_default();
            for c in recs {
                r.checks.insert(
                    c.capability.key().into(),
                    Check {
                        verdict: match c.verdict() {
                            Verdict::Working => CheckVerdict::Working,
                            Verdict::Failed => CheckVerdict::Failed,
                            Verdict::Unknown => CheckVerdict::Unknown,
                        },
                        evidence: match c.source() {
                            Some(Source::Confirmed) => Evidence::Confirmed,
                            Some(Source::Observed) => Evidence::Observed,
                            None => Evidence::None,
                        },
                        passes: c.passes,
                        failures: c.failures,
                        last: c.last_ts.map(date),
                        last_failure: c.last_failure,
                    },
                );
            }
            for b in st["charge_curve_min"].as_array().into_iter().flatten() {
                if let (Some(lo), Some(m)) = (b[0].as_u64(), b[1].as_f64()) {
                    r.charge_curve_min
                        .insert(format!("{lo}-{}", (lo + 5).min(100)), m);
                }
            }
        }
        Err(e) => eprintln!("pbcd not reachable, so the report has no self-checks: {e:#}"),
    }
    let profile = file.profile.clone();
    match write {
        None => {
            file.reports.push(r);
            print!("{}", file.to_yaml()?);
        }
        Some(dest) => {
            let repo = dest.is_dir().then(|| find_repo(dest)).flatten();
            let path = match (&repo, dest.is_dir()) {
                (Some(root), _) => file.path_in(&root.join("hardware")),
                (None, true) => dest.join(file.file_name()),
                (None, false) => dest.to_path_buf(),
            };
            let merging = path.exists();
            if merging {
                file = ModelFile::parse(&std::fs::read_to_string(&path)?)
                    .with_context(|| format!("reading {}", path.display()))?;
            }
            file.merge(r, profile);
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, file.to_yaml()?)
                .with_context(|| format!("writing {}", path.display()))?;
            give_to_sudo_user(&path);
            println!(
                "{} {}",
                if merging { "Updated" } else { "Saved" },
                path.display()
            );
            if repo.is_none() {
                println!(
                    "This isn't inside a clone of the repository. Move it to {} in your clone, \
                     or run this again from inside the clone. See docs/DEVICE_SUPPORT.md.",
                    file.path_in(std::path::Path::new("hardware")).display()
                );
            }
        }
    }
    Ok(())
}
