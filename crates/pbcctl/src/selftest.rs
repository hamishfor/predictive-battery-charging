//! Guided self-test. Walks the user through the checks that can't be observed
//! passively, and saves the results to pbcd, which acts on them straight away.
//!
//! Each step changes the charge limits and puts them back afterwards. Ctrl-C
//! at any point leaves the limits as pbcd will next write them, within a minute.

use crate::bus;
use anyhow::{bail, Context, Result};
use pbc_hw::sysfs::PSY;
use pbc_hw::thresholds::{apply, Thresholds};
use pbc_hw::wakeplan;
use pbc_hw::{Battery, BatteryStatus, DmiInfo, Profile, SysRoot};
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

pub struct Opts {
    pub user_bus: bool,
    pub only: Option<String>,
}

fn ask(q: &str) -> Result<bool> {
    print!("{q} [y/n] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

fn pause(msg: &str) -> Result<()> {
    print!("{msg} Press Enter when ready. ");
    std::io::stdout().flush()?;
    std::io::stdin().lock().read_line(&mut String::new())?;
    Ok(())
}

fn energy(b: &Battery) -> Option<u64> {
    b.snapshot().energy_now_uwh
}

/// Wait up to `timeout` for `cond`, checking every 5 s. Prints a dot per check.
fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < timeout {
        if cond() {
            println!();
            return true;
        }
        print!(".");
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_secs(5));
    }
    println!();
    false
}

struct Ctx {
    root: SysRoot,
    bats: Vec<Battery>,
    rules: pbc_hw::profile::ThresholdRules,
    opts: Opts,
}

impl Ctx {
    fn set_all(&self, t: Thresholds) -> Result<()> {
        for b in &self.bats {
            apply(&self.root, b, t, &self.rules)
                .with_context(|| format!("writing {} to {}", t, b.name))?;
        }
        Ok(())
    }

    fn save(&self, key: &str, working: bool) -> Result<()> {
        bus::call(
            self.opts.user_bus,
            "ConfirmCapability",
            &["sb", key, if working { "true" } else { "false" }],
        )?;
        println!(
            "  saved: {key} = {}",
            if working { "working" } else { "not working" }
        );
        Ok(())
    }

    fn wants(&self, key: &str) -> bool {
        self.opts.only.as_deref().is_none_or(|o| o == key)
    }
}

pub fn run(sysfs: &std::path::Path, opts: Opts) -> Result<()> {
    if !is_root() {
        bail!("run with sudo: the self-test changes the charge limits");
    }
    let root = SysRoot::new(sysfs);
    let dmi = DmiInfo::read(&root);
    let profiles = Profile::builtin();
    let Some(p) = Profile::detect(&profiles, &dmi, &root) else {
        bail!("no hardware profile matches this laptop. See docs/DEVICE_SUPPORT.md to add one");
    };
    let bats: Vec<Battery> = Battery::discover(&root)?
        .into_iter()
        .filter(|b| b.has_thresholds())
        .collect();
    if bats.is_empty() {
        bail!("no battery exposes charge limits");
    }
    // Fail early if pbcd can't take results.
    bus::call(opts.user_bus, "StatusJson", &[]).context("pbcd must be running to save results")?;
    let cx = Ctx {
        root,
        bats,
        rules: p.thresholds.clone(),
        opts,
    };

    println!(
        "Predictive Battery Charging self-test on {} ({}).",
        dmi.model(),
        cx.bats
            .iter()
            .map(|b| b.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("Results are saved to pbcd as you go. You can stop at any time with Ctrl-C.\n");
    println!("pbcd keeps running. It may rewrite the limits within a minute after each step, which is fine.\n");

    if cx.wants("enforce") || cx.wants("resume") {
        limits(&cx)?;
    }
    if cx.wants("quiet_wake") {
        quiet_wake(&cx)?;
    }
    println!("\nDone. `pbcctl report` shows the results, and `pbcctl report --write`, run inside a clone of the repository, saves them for a device-support pull request. Thanks!");
    Ok(())
}

/// Steps 1 and 2: the limit stops charging, and lifting it starts it again.
fn limits(cx: &Ctx) -> Result<()> {
    println!("== Charge limit (about 3 minutes)");
    let b = &cx.bats[0];
    let cap = b.snapshot().capacity_pct.unwrap_or(0);
    if !(15..=95).contains(&cap) {
        println!("  Skipped: the battery is at {cap}%. Run this between 15% and 95%.");
        return Ok(());
    }
    pause("  Plug in the charger.")?;
    let end = cap.saturating_sub(2).max(cx.rules.end_min);
    let start = end.saturating_sub(5).max(cx.rules.start_min);
    cx.set_all(Thresholds { start, end })?;
    print!("  Limit set to {end}%. Waiting for charging to stop");
    let stopped = wait_for(Duration::from_secs(120), || {
        let s = b.snapshot();
        s.status != BatteryStatus::Charging && s.power_now_uw.unwrap_or(0) < 1_000_000
    });
    cx.set_all(Thresholds { start: 0, end: 100 })?;
    if cx.wants("enforce") {
        cx.save("enforce", stopped)?;
    }
    if !stopped {
        println!("  Charging didn't stop. Predictive Battery Charging can't hold the battery on this laptop.");
        return Ok(());
    }
    print!("  Limit lifted. Waiting for charging to start again");
    let e1 = energy(b);
    let resumed = wait_for(Duration::from_secs(120), || {
        b.snapshot().status == BatteryStatus::Charging
            || energy(b).zip(e1).is_some_and(|(e, e1)| e > e1)
    });
    if cx.wants("resume") {
        cx.save("resume", resumed)?;
    }
    if !resumed {
        println!("  Charging didn't restart. pbcd will not limit charging on this laptop.");
    }
    Ok(())
}

/// Step 3: the quiet wake, with the user watching. It uses the installed
/// pbcd sleep hook, through a one-off plan in /run/pbcd.
fn quiet_wake(cx: &Ctx) -> Result<()> {
    println!("\n== Wake during sleep (about 10 minutes)");
    println!("  pbcd can wake the laptop for a moment at night to finish charging.");
    println!("  This checks that the wake is invisible. You'll close the lid and watch.");
    let hook = std::path::Path::new("/usr/lib/systemd/system-sleep/pbcd");
    if !hook.exists() {
        println!("  Skipped: the pbcd sleep hook isn't installed. Run sudo dist/install.sh first.");
        return Ok(());
    }
    if !ask("  Run this step?")? {
        return Ok(());
    }
    pause("  Plug in the charger and unplug any external monitor.")?;
    use pbc_model::selfcheck::{classify_wake, diagnose_wake, WakeOutcome};

    let decisions = test_wake(cx)?;
    let explain = || {
        if let Some(g) = diagnose_wake(&decisions) {
            println!("  {}", g.what);
            for l in &g.fix {
                println!("  {l}");
            }
        }
    };
    match classify_wake(&decisions) {
        WakeOutcome::Clean => println!("  The wake ran and the laptop went back to sleep."),
        WakeOutcome::Bad(why) => {
            println!("  The wake went wrong: {why}");
            explain();
            cx.save("quiet_wake", false)?;
            return Ok(());
        }
        WakeOutcome::NotTested => {
            println!("  The test wake didn't finish.");
            explain();
            println!(
                "  Nothing was saved. Run `sudo pbcctl selftest --only quiet_wake` to try again."
            );
            return Ok(());
        }
    }
    let silent = ask("  Was the laptop dark and silent the whole time? (no screen, keyboard light, fan, sound or headphone reconnect)")?;
    cx.save("quiet_wake", silent)?;
    Ok(())
}

/// Arm one test wake, wait for the user, and return the hook's decisions.
fn test_wake(cx: &Ctx) -> Result<Vec<(String, String)>> {
    let dir = std::path::Path::new(wakeplan::DIR);
    let wake_at = now() + 120;
    let _ = std::fs::remove_file(wakeplan::log_path(dir));
    // Hold now. The wake should raise the limit to 0/100.
    cx.set_all(Thresholds { start: 75, end: 80 })?;
    wakeplan::write(
        dir,
        &wakeplan::Plan {
            wake_at,
            start: 0,
            end: 100,
            batteries: cx
                .bats
                .iter()
                .map(|b| cx.root.path(format!("{PSY}/{}", b.name)))
                .collect(),
            min_gap: cx.rules.min_gap,
            selftest: true,
        },
    )?;
    let at = jiff::Timestamp::from_second(wake_at)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_default();
    println!("  Test wake set for {at}.");
    println!(
        "  Close the lid now. Keep it closed for 5 minutes, and watch and listen the whole time."
    );
    pause("  Then open the lid and come back here.")?;

    // pbcd left the plan and log for us. Clear them now we've read them.
    let decisions = wakeplan::take_log(dir)
        .into_iter()
        .map(|l| (l.decision, l.detail))
        .collect();
    wakeplan::clear(dir);
    Ok(decisions)
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn is_root() -> bool {
    // /proc/self/status: "Uid: real effective saved fs"
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(2))
                .map(|e| e == "0")
        })
        .unwrap_or(false)
}
