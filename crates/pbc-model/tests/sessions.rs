use pbc_model::session::{build, Config, End, PowerEvent as E, When};

const H: i64 = 3600;
fn ac(ts: i64, online: bool) -> E {
    E::Ac {
        ts,
        online,
        capacity: Some(50),
    }
}

#[test]
fn simple_session() {
    let s = build(
        Config::default(),
        &[
            ac(0, false),
            ac(10, true),
            ac(3 * H, false),
            ac(4 * H, false),
        ],
        None,
    )
    .unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].plugged, When::exact(10));
    assert_eq!(s[0].end, End::Unplugged(When::exact(3 * H)));
    assert_eq!(s[0].merged_gaps, 0);
}

#[test]
fn brief_unplug_is_merged() {
    let ev = [
        ac(0, true),
        ac(H, false),
        ac(H + 120, true),
        ac(2 * H, false),
    ];
    let s = build(Config::default(), &ev, Some(3 * H)).unwrap();
    assert_eq!(s.len(), 1, "{s:#?}");
    assert_eq!(s[0].plugged, When::exact(0));
    assert_eq!(s[0].end, End::Unplugged(When::exact(2 * H)));
    assert_eq!(s[0].merged_gaps, 1);
}

#[test]
fn long_gap_is_two_sessions() {
    let ev = [
        ac(0, true),
        ac(H, false),
        ac(H + 600, true),
        ac(2 * H, false),
    ];
    let s = build(Config::default(), &ev, Some(3 * H)).unwrap();
    assert_eq!(s.len(), 2, "{s:#?}");
    assert_eq!(s[0].end, End::Unplugged(When::exact(H)));
    assert_eq!(s[1].plugged, When::exact(H + 600));
}

#[test]
fn unplug_during_sleep_is_interval() {
    let ev = [
        ac(0, true),
        E::Suspend {
            ts: H,
            capacity: Some(80),
        },
        E::Resume {
            ts: 9 * H,
            online: false,
            capacity: Some(78),
        },
    ];
    let s = build(Config::default(), &ev, Some(10 * H)).unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].end, End::Unplugged(When::between(H, 9 * H)));
    assert_eq!(s[0].capacity_at_end, Some(78));
}

#[test]
fn plug_during_sleep_is_interval() {
    let ev = [
        ac(0, false),
        E::Suspend {
            ts: H,
            capacity: Some(30),
        },
        E::Resume {
            ts: 8 * H,
            online: true,
            capacity: Some(80),
        },
        ac(9 * H, false),
    ];
    let s = build(Config::default(), &ev, Some(10 * H)).unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].plugged, When::between(H, 8 * H));
    assert_eq!(s[0].end, End::Unplugged(When::exact(9 * H)));
}

#[test]
fn shutdown_on_ac_then_boot_on_battery() {
    // Plugged, shut down on AC, booted later on battery: the unplug happened while off.
    let ev = [
        ac(0, true),
        E::Stop {
            ts: 2 * H,
            online: true,
            capacity: Some(80),
        },
        E::Start {
            ts: 10 * H,
            online: false,
            capacity: Some(79),
            last_seen: Some(2 * H),
        },
    ];
    let s = build(Config::default(), &ev, Some(11 * H)).unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].end, End::Unplugged(When::between(2 * H, 10 * H)));
}

#[test]
fn crash_without_stop_uses_last_seen() {
    let ev = [
        ac(0, true),
        E::Start {
            ts: 5 * H,
            online: false,
            capacity: Some(60),
            last_seen: Some(2 * H),
        },
    ];
    let s = build(Config::default(), &ev, Some(6 * H)).unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].end, End::Unplugged(When::between(2 * H, 5 * H)));
}

#[test]
fn still_plugged_at_end_is_censored() {
    let s = build(Config::default(), &[ac(0, true)], Some(5 * H)).unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].end, End::Censored(5 * H));
    assert!(s[0].is_censored());
    assert_eq!(s[0].duration(), 5 * H);
}

#[test]
fn imprecise_unplug_is_not_merged_with_replug() {
    // Unplugged at some point during sleep, replugged right after resume.
    let ev = [
        ac(0, true),
        E::Suspend {
            ts: H,
            capacity: None,
        },
        E::Resume {
            ts: 2 * H,
            online: false,
            capacity: None,
        },
        ac(2 * H + 30, true),
    ];
    let s = build(Config::default(), &ev, Some(3 * H)).unwrap();
    assert_eq!(s.len(), 2, "{s:#?}");
}

#[test]
fn sleep_on_ac_resume_on_ac_is_one_session() {
    let ev = [
        ac(0, true),
        E::Suspend {
            ts: H,
            capacity: None,
        },
        E::Resume {
            ts: 8 * H,
            online: true,
            capacity: None,
        },
        ac(9 * H, false),
    ];
    let s = build(Config::default(), &ev, Some(10 * H)).unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].plugged, When::exact(0));
    assert_eq!(s[0].end, End::Unplugged(When::exact(9 * H)));
}

#[test]
fn out_of_order_is_rejected() {
    assert!(build(Config::default(), &[ac(10, true), ac(5, false)], None).is_err());
}

#[test]
fn repeated_samples_are_idempotent() {
    let ev: Vec<_> = (0..100)
        .map(|i| ac(i * 60, true))
        .chain([ac(101 * 60, false)])
        .collect();
    let s = build(Config::default(), &ev, Some(200 * 60)).unwrap();
    assert_eq!(s.len(), 1);
}

#[test]
fn tick_releases_pending_after_merge_window() {
    let mut b = pbc_model::session::SessionBuilder::new(Config::default());
    b.push(&ac(0, true)).unwrap();
    assert!(
        b.push(&ac(H, false)).unwrap().is_empty(),
        "held for the merge window"
    );
    assert!(b.tick(H + 299).is_empty());
    let s = b.tick(H + 301);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].end, End::Unplugged(When::exact(H)));
    assert!(b.tick(H + 1000).is_empty(), "released only once");
}

/// 2026-10-12: unplugged at 22:14, slept on battery at 66% from 23:01, plugged in
/// and out while asleep, woke at 08:07 on battery at 79%.
#[test]
fn charge_gained_during_sleep_on_battery_is_a_session() {
    let ev = [
        E::Ac {
            ts: 0,
            online: false,
            capacity: Some(76),
        },
        E::Suspend {
            ts: H,
            capacity: Some(66),
        },
        E::Resume {
            ts: 10 * H,
            online: false,
            capacity: Some(79),
        },
    ];
    let s = build(Config::default(), &ev, Some(11 * H)).unwrap();
    assert_eq!(s.len(), 1, "{s:#?}");
    assert_eq!(s[0].plugged, When::between(H, 10 * H));
    assert_eq!(s[0].end, End::Unplugged(When::between(H, 10 * H)));
    assert_eq!(
        (s[0].capacity_at_plug, s[0].capacity_at_end),
        (Some(66), Some(79))
    );
}

#[test]
fn normal_drain_during_sleep_is_not_a_session() {
    let ev = [
        E::Ac {
            ts: 0,
            online: false,
            capacity: Some(76),
        },
        E::Suspend {
            ts: H,
            capacity: Some(66),
        },
        E::Resume {
            ts: 10 * H,
            online: false,
            capacity: Some(63),
        },
    ];
    assert!(build(Config::default(), &ev, Some(11 * H))
        .unwrap()
        .is_empty());
    // One point up is rounding or resume jitter, not a charge.
    let ev = [
        E::Ac {
            ts: 0,
            online: false,
            capacity: Some(76),
        },
        E::Suspend {
            ts: H,
            capacity: Some(66),
        },
        E::Resume {
            ts: 10 * H,
            online: false,
            capacity: Some(67),
        },
    ];
    assert!(build(Config::default(), &ev, Some(11 * H))
        .unwrap()
        .is_empty());
}

#[test]
fn charge_gained_while_powered_off_is_a_session() {
    let ev = [
        E::Ac {
            ts: 0,
            online: false,
            capacity: Some(40),
        },
        E::Stop {
            ts: H,
            online: false,
            capacity: Some(40),
        },
        E::Start {
            ts: 9 * H,
            online: false,
            capacity: Some(80),
            last_seen: Some(H),
        },
    ];
    let s = build(Config::default(), &ev, Some(10 * H)).unwrap();
    assert_eq!(s.len(), 1, "{s:#?}");
    assert_eq!(s[0].plugged, When::between(H, 9 * H));
    assert_eq!(
        (s[0].capacity_at_plug, s[0].capacity_at_end),
        (Some(40), Some(80))
    );
}

#[test]
fn hidden_charge_after_a_normal_session_keeps_both() {
    // A real session, unplugged at 2h, then a hidden one during sleep. The first is released, not lost.
    let ev = [
        ac(0, true),
        E::Ac {
            ts: 2 * H,
            online: false,
            capacity: Some(70),
        },
        E::Suspend {
            ts: 2 * H + 60,
            capacity: Some(70),
        },
        E::Resume {
            ts: 10 * H,
            online: false,
            capacity: Some(90),
        },
    ];
    let s = build(Config::default(), &ev, Some(11 * H)).unwrap();
    assert_eq!(s.len(), 2, "{s:#?}");
    assert_eq!(s[0].end, End::Unplugged(When::exact(2 * H)));
    assert_eq!(s[1].plugged, When::between(2 * H + 60, 10 * H));
}
