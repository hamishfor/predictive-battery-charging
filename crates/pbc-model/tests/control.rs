use pbc_model::charge::{ChargeModel, ChargeSample};
use pbc_model::control::*;
use pbc_model::predict::Prediction;

const H: i64 = 3600;
fn pred(full_by: i64, confidence: f64) -> Prediction {
    Prediction {
        full_by,
        confidence,
        reason: "test".into(),
        source: "test",
    }
}
fn input<'a>(now: i64, online: bool, p: Option<&'a Prediction>) -> Input<'a> {
    Input {
        now,
        online,
        prediction: p,
        override_requested: false,
        secs_to_full: 40 * 60,
        secs_ready_to_full: 25 * 60,
    }
}
fn end(d: &Decision) -> u8 {
    d.target.end
}

#[test]
fn full_always_means_100() {
    assert_eq!(Target::FULL.end, 100);
    assert!(Target::FULL.is_full());
    assert!(!Target { end: 99 }.is_full());
}

#[test]
fn no_prediction_charges_to_100() {
    let d = decide(&State::Unplugged, &input(0, true, None), &Policy::default());
    assert_eq!((d.state.clone(), end(&d)), (State::Normal, 100));
}

#[test]
fn below_min_confidence_charges_to_100() {
    let p = pred(10 * H, 0.2);
    let d = decide(
        &State::Unplugged,
        &input(0, true, Some(&p)),
        &Policy::default(),
    );
    assert_eq!((d.state.clone(), end(&d)), (State::Normal, 100));
}

#[test]
fn hold_level_scales_with_confidence() {
    let pol = Policy::default();
    assert_eq!(pol.hold_for(1.0), 70, "certain: lowest hold");
    assert_eq!(
        pol.hold_for(pol.min_confidence),
        99,
        "barely confident: highest hold"
    );
    let mid = pol.hold_for(0.65);
    assert!(mid > 70 && mid < 99, "{mid}");
    // Monotonic: more confidence never holds higher.
    let mut last = 100;
    for c in 30..=100 {
        let h = pol.hold_for(c as f64 / 100.0);
        assert!(h <= last, "{c}: {h} > {last}");
        assert!((70..=99).contains(&h));
        last = h;
    }
}

#[test]
fn confident_long_session_holds_low_then_steps_up_then_tops_off() {
    let pol = Policy::default();
    let p = pred(10 * H, 1.0);
    // Far from the unplug: lowest hold.
    let d = decide(&State::Unplugged, &input(0, true, Some(&p)), &pol);
    assert!(matches!(d.state, State::Holding { .. }));
    assert_eq!(end(&d), 70);
    // Within 3 h of the unplug: the ready level.
    let d = decide(&d.state, &input(7 * H + 60, true, Some(&p)), &pol);
    assert_eq!(end(&d), 90, "{}", d.reason);
    // Top-off time: 100.
    let topoff = 10 * H - 40 * 60 - 15 * 60;
    let d = decide(&d.state, &input(topoff, true, Some(&p)), &pol);
    assert_eq!(
        (d.state.clone(), end(&d)),
        (State::ToppingOff { full_by: 10 * H }, 100)
    );
}

#[test]
fn weak_prediction_holds_high() {
    let pol = Policy::default();
    let p = pred(10 * H, 0.35);
    let d = decide(&State::Unplugged, &input(0, true, Some(&p)), &pol);
    assert!(end(&d) >= 95 && end(&d) < 100, "{}", end(&d));
    // Near the unplug it doesn't step down to the ready level.
    let d = decide(&d.state, &input(8 * H, true, Some(&p)), &pol);
    assert!(end(&d) >= 95, "{}", end(&d));
}

#[test]
fn topoff_latches_even_if_prediction_moves() {
    let pol = Policy::default();
    let s = State::ToppingOff { full_by: 10 * H };
    let tomorrow = pred(34 * H, 1.0);
    let d = decide(&s, &input(9 * H, true, Some(&tomorrow)), &pol);
    assert_eq!(end(&d), 100);
    let d = decide(&s, &input(11 * H + 1, true, Some(&tomorrow)), &pol);
    assert!(matches!(d.state, State::Holding { .. }), "{d:?}");
}

#[test]
fn override_is_100_until_unplug() {
    let pol = Policy::default();
    let p = pred(10 * H, 1.0);
    let mut i = input(0, true, Some(&p));
    i.override_requested = true;
    let d = decide(
        &State::Holding {
            full_by: 10 * H,
            topoff_at: 9 * H,
        },
        &i,
        &pol,
    );
    assert_eq!((d.state.clone(), end(&d)), (State::Override, 100));
}

#[test]
fn override_armed_on_battery_opens_the_limit() {
    let mut i = input(0, false, None);
    i.override_requested = true;
    let d = decide(&State::Unplugged, &i, &Policy::default());
    assert_eq!((d.state.clone(), end(&d)), (State::Unplugged, 100));
}

#[test]
fn on_battery_leaves_the_next_plug_ins_hold() {
    let pol = Policy::default();
    let p = pred(10 * H, 1.0);
    let d = decide(&State::Normal, &input(0, false, Some(&p)), &pol);
    assert_eq!((d.state.clone(), end(&d)), (State::Unplugged, 70));
    // No prediction: leave it at 100, so an unseen plug-in charges normally.
    let d = decide(&State::Normal, &input(0, false, None), &pol);
    assert_eq!(end(&d), 100);
}

#[test]
fn plugged_in_late_tops_off_immediately() {
    let p = pred(30 * 60, 1.0);
    let d = decide(
        &State::Unplugged,
        &input(0, true, Some(&p)),
        &Policy::default(),
    );
    assert_eq!(end(&d), 100);
}

#[test]
fn past_deadline_is_ignored() {
    let p = pred(-H, 1.0);
    let d = decide(
        &State::Unplugged,
        &input(0, true, Some(&p)),
        &Policy::default(),
    );
    assert_eq!((d.state.clone(), end(&d)), (State::Normal, 100));
}

#[test]
fn sleep_plans() {
    let pol = Policy::default();
    let p = pred(10 * H, 1.0);
    let d = decide(&State::Unplugged, &input(0, true, Some(&p)), &pol);
    let topoff = match d.state {
        State::Holding { topoff_at, .. } => topoff_at,
        _ => panic!(),
    };
    assert_eq!(
        plan_sleep(&d, 0, true, 30 * 60, SleepFallback::Full, None),
        SleepAction::Wake { at: topoff }
    );
    assert_eq!(
        plan_sleep(&d, 0, false, 30 * 60, SleepFallback::Full, None),
        SleepAction::StartNow
    );
    assert_eq!(
        plan_sleep(&d, 0, false, 30 * 60, SleepFallback::Hold, None),
        SleepAction::Nothing
    );
    assert_eq!(
        plan_sleep(
            &d,
            topoff - 20 * 60,
            true,
            30 * 60,
            SleepFallback::Hold,
            None
        ),
        SleepAction::StartNow
    );
    let normal = decide(&State::Unplugged, &input(0, true, None), &pol);
    assert_eq!(
        plan_sleep(&normal, 0, true, 30 * 60, SleepFallback::Full, None),
        SleepAction::Nothing
    );
}

#[test]
fn sleeping_on_battery_arms_a_wake_for_the_deadline() {
    let pol = Policy::default();
    let p = pred(10 * H, 1.0);
    let d = decide(&State::Unplugged, &input(0, false, Some(&p)), &pol);
    let topoff = 10 * H - 40 * 60 - 15 * 60;
    assert_eq!(
        plan_sleep(&d, 0, true, 30 * 60, SleepFallback::Full, Some(topoff)),
        SleepAction::Wake { at: topoff }
    );
    assert_eq!(
        plan_sleep(&d, 0, false, 30 * 60, SleepFallback::Full, Some(topoff)),
        SleepAction::Nothing
    );
    assert_eq!(
        plan_sleep(&d, 0, true, 30 * 60, SleepFallback::Full, None),
        SleepAction::Nothing
    );
    assert_eq!(
        plan_sleep(&d, 0, true, 30 * 60, SleepFallback::Full, Some(-1)),
        SleepAction::Nothing
    );
}

#[test]
fn charge_model_default_matches_m0() {
    let m = ChargeModel::default();
    let t = m.secs_to_full(80);
    // Measured on the X1 Carbon Gen 13: 39.5 min from 80% to 100%.
    assert!((35 * 60..=45 * 60).contains(&t), "{t}");
    assert_eq!(m.secs_to_full(100), 0);
}

#[test]
fn charge_model_learns() {
    let mut m = ChargeModel::default();
    // Synthetic: 30 s per percent from 50 to 60, three times over.
    let mut s = Vec::new();
    for rep in 0..3 {
        let base = rep * 10_000;
        for (i, c) in (50..=60).enumerate() {
            s.push(ChargeSample {
                ts: base + i as i64 * 30,
                capacity: c,
                charging: true,
            });
        }
    }
    let changed = m.learn(&s, 300, 3);
    // 50% is where each run starts, and its start time is unknown, so it's skipped. 51..59 are learned.
    assert_eq!(changed, 9);
    assert_eq!(m.secs_to_full(51) - m.secs_to_full(60), 270);
    // The skipped level keeps its prior.
    assert_eq!(
        m.secs_to_full(50) - m.secs_to_full(51),
        ChargeModel::default().secs_to_full(50) - ChargeModel::default().secs_to_full(51)
    );
}

fn run(base: i64, step_s: i64, caps: &[u8]) -> Vec<ChargeSample> {
    caps.iter()
        .enumerate()
        .map(|(i, &c)| ChargeSample {
            ts: base + i as i64 * step_s,
            capacity: c,
            charging: true,
        })
        .collect()
}

#[test]
fn flat_samples_count_toward_the_level() {
    // 60 s samples, one percent every 3 samples (3 min per percent): 70,70,70,71,71,71,72,...
    let caps: Vec<u8> = (70..=80).flat_map(|c| [c, c, c]).collect();
    let mut s = Vec::new();
    for r in 0..3 {
        s.extend(run(r * 100_000, 60, &caps));
    }
    let mut m = ChargeModel::default();
    m.learn(&s, 600, 3);
    for c in 71..80 {
        assert_eq!(m.secs_to_full(c) - m.secs_to_full(c + 1), 180, "level {c}");
    }
}

#[test]
fn multi_point_jump_is_shared() {
    // 10 min gap, capacity +5: each level gets 120 s.
    let mut s = Vec::new();
    for r in 0..3 {
        s.extend(run(r * 100_000, 600, &[40, 41, 46]));
    }
    let mut m = ChargeModel::default();
    m.learn(&s, 600, 3);
    for c in 41..46 {
        assert_eq!(m.secs_to_full(c) - m.secs_to_full(c + 1), 120, "level {c}");
    }
}

#[test]
fn interruption_restarts_the_run() {
    // Charging stops (hold), resumes later: the level in progress at the stop isn't timed across the pause.
    let mut s = Vec::new();
    for r in 0..3 {
        let b = r * 100_000;
        s.extend(run(b, 60, &[60, 61, 61]));
        s.push(ChargeSample {
            ts: b + 180,
            capacity: 61,
            charging: false,
        });
        s.extend(run(b + 4000, 60, &[61, 62, 63]));
    }
    let mut m = ChargeModel::default();
    m.learn(&s, 600, 3);
    // 61 was entered at different times in two runs. Only the second run's 62 is learnable.
    assert_eq!(m.secs_to_full(62) - m.secs_to_full(63), 60);
    let d = ChargeModel::default();
    assert_eq!(
        m.secs_to_full(61) - m.secs_to_full(62),
        d.secs_to_full(61) - d.secs_to_full(62),
        "61 not learned across a pause"
    );
}

#[test]
fn capacity_drop_while_charging_is_ignored() {
    let mut s = Vec::new();
    for r in 0..3 {
        s.extend(run(r * 100_000, 60, &[50, 51, 49, 50, 51, 52]));
    }
    let mut m = ChargeModel::default();
    m.learn(&s, 600, 3);
    // After the drop, 49->50 is a new run start (skipped); 50 and 51 are each 60 s.
    assert_eq!(m.secs_to_full(50) - m.secs_to_full(51), 60);
    assert_eq!(m.secs_to_full(51) - m.secs_to_full(52), 60);
}

#[test]
fn charge_model_ignores_gaps_and_discharge() {
    let mut m = ChargeModel::default();
    let s = vec![
        ChargeSample {
            ts: 0,
            capacity: 50,
            charging: true,
        },
        ChargeSample {
            ts: 10_000,
            capacity: 51,
            charging: true,
        }, // gap too big
        ChargeSample {
            ts: 10_060,
            capacity: 52,
            charging: false,
        },
    ];
    assert_eq!(m.learn(&s, 300, 1), 0);
}

/// A real 80 to 100% curve from an X1 Carbon Gen 13 (USB-C, 30 s samples).
fn m0_curve() -> Vec<ChargeSample> {
    include_str!("data/x1-carbon-gen13-charge-curve.csv")
        .lines()
        .skip(1)
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            ChargeSample {
                ts: f[0].parse().unwrap(),
                capacity: f[3].parse().unwrap(),
                charging: f[1] == "Charging",
            }
        })
        .collect()
}

/// Every second sample, which is what pbcd's 60 s interval sees.
fn every_other(s: &[ChargeSample]) -> Vec<ChargeSample> {
    s.iter().step_by(2).copied().collect()
}

#[test]
fn learning_from_m0_curve_matches_measured_time() {
    // The measured curve takes 39.5 min from 80% to 100%. Learning from it must not shorten that.
    for (name, samples) in [("30s", m0_curve()), ("60s", every_other(&m0_curve()))] {
        // Three identical runs, so every level gets at least min_obs observations.
        let mut runs = Vec::new();
        for r in 0..3 {
            runs.extend(samples.iter().map(|s| ChargeSample {
                ts: s.ts + r * 100_000,
                ..*s
            }));
        }
        let mut m = ChargeModel::default();
        m.learn(&runs, 600, 3);
        let mins = m.secs_to_full(80) as f64 / 60.0;
        assert!(
            (37.0..=42.0).contains(&mins),
            "{name}: learned 80->100 = {mins:.1} min, measured 39.5"
        );
        // Near full, each percent must take longer than mid-range (taper).
        assert!(
            m.secs_to_full(95) - m.secs_to_full(96) > m.secs_to_full(80) - m.secs_to_full(81),
            "{name}: taper lost"
        );
    }
}

#[test]
fn learned_bands_skip_the_prior() {
    assert!(ChargeModel::default().learned_bands(5).is_empty());
    // 3 min per percent from 70 to 80, three runs.
    let caps: Vec<u8> = (70..=81).flat_map(|c| [c, c, c]).collect();
    let mut s = Vec::new();
    for r in 0..3 {
        s.extend(run(r * 100_000, 60, &caps));
    }
    let mut m = ChargeModel::default();
    m.learn(&s, 600, 3);
    // 70 is each run's start level, so it isn't learned. 71..=80 are.
    assert_eq!(m.learned_bands(5), [(75, 15.0)]);
}
