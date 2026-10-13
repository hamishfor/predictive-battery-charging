use jiff::civil::date;
use jiff::tz::TimeZone;
use pbc_model::predict::{Day, Prediction, Predictor, Query, Schedule, ScheduleRule, WithSchedule};

fn ts(tz: &TimeZone, y: i16, m: i8, d: i8, h: i8, mi: i8) -> i64 {
    date(y, m, d)
        .at(h, mi, 0, 0)
        .to_zoned(tz.clone())
        .unwrap()
        .timestamp()
        .as_second()
}

fn weekday_8am() -> Schedule {
    use Day::*;
    Schedule::new(&[ScheduleRule {
        days: vec![Mon, Tue, Wed, Thu, Fri],
        full_by: "08:00".into(),
    }])
    .unwrap()
}

#[test]
fn next_weekday_morning() {
    let tz = TimeZone::get("Pacific/Auckland").unwrap();
    let s = weekday_8am();
    // Wed 2026-10-07 22:00 -> Thu 08:00
    let now = ts(&tz, 2026, 10, 7, 22, 0);
    assert_eq!(s.next_deadline(now, &tz), Some(ts(&tz, 2026, 10, 8, 8, 0)));
    // Wed 07:59 -> Wed 08:00
    let now = ts(&tz, 2026, 10, 7, 7, 59);
    assert_eq!(s.next_deadline(now, &tz), Some(ts(&tz, 2026, 10, 7, 8, 0)));
}

#[test]
fn weekend_looks_ahead_to_monday() {
    let tz = TimeZone::get("Pacific/Auckland").unwrap();
    let s = weekday_8am();
    let mon8 = ts(&tz, 2026, 10, 12, 8, 0);
    // Fri 09:00, Fri 20:00, Sat 12:00, Sun 20:00: all point at Mon 08:00.
    for (d, h) in [(9, 9), (9, 20), (10, 12), (11, 20)] {
        let now = ts(&tz, 2026, 10, d, h, 0);
        assert_eq!(s.next_deadline(now, &tz), Some(mon8), "from {d} Oct {h}:00");
    }
}

#[test]
fn single_weekly_rule_wraps_a_full_week() {
    let tz = TimeZone::get("Pacific/Auckland").unwrap();
    let s = Schedule::new(&[ScheduleRule {
        days: vec![Day::Wed],
        full_by: "08:00".into(),
    }])
    .unwrap();
    // Wed 2026-10-07 09:00, just after this week's deadline: next is Wed 14 Oct, 7 days less 1 hour away.
    let now = ts(&tz, 2026, 10, 7, 9, 0);
    assert_eq!(s.next_deadline(now, &tz), Some(ts(&tz, 2026, 10, 14, 8, 0)));
}

#[test]
fn no_rules_means_no_deadline() {
    let tz = TimeZone::get("Pacific/Auckland").unwrap();
    let s = Schedule::new(&[]).unwrap();
    assert_eq!(s.next_deadline(ts(&tz, 2026, 10, 9, 20, 0), &tz), None);
}

#[test]
fn dst_transition_day() {
    // NZ DST started Sun 2026-09-27 02:00 -> 03:00. A 02:30 rule doesn't exist that day.
    let tz = TimeZone::get("Pacific/Auckland").unwrap();
    let s = Schedule::new(&[ScheduleRule {
        days: vec![Day::Sun],
        full_by: "02:30".into(),
    }])
    .unwrap();
    let now = ts(&tz, 2026, 9, 26, 22, 0);
    let d = s
        .next_deadline(now, &tz)
        .expect("compatible strategy resolves the gap");
    assert!(d > now && d - now < 6 * 3600);
}

#[test]
fn bad_rules_rejected() {
    assert!(Schedule::new(&[ScheduleRule {
        days: vec![Day::Mon],
        full_by: "25:00".into()
    }])
    .is_err());
    assert!(Schedule::new(&[ScheduleRule {
        days: vec![],
        full_by: "08:00".into()
    }])
    .is_err());
}

/// A learned predictor that always answers with the same unplug time.
struct Learned(Option<i64>);
impl Predictor for Learned {
    fn predict(&self, _: &Query<'_>) -> Option<Prediction> {
        Some(Prediction {
            full_by: self.0?,
            confidence: 0.8,
            reason: "learned: 6 similar sessions".into(),
            source: "learned",
        })
    }
}

fn query(now: i64, tz: &TimeZone) -> Query<'_> {
    Query {
        now,
        plugged_since: None,
        capacity: Some(80),
        history: &[],
        tz,
    }
}

#[test]
fn schedule_alone_never_holds() {
    // 17:00 Tue, schedule says full by Wed 08:00, but nothing learned: no prediction, so charge to 100%.
    let tz = TimeZone::get("Pacific/Auckland").unwrap();
    let now = ts(&tz, 2026, 10, 13, 17, 0);
    let s = weekday_8am();
    let p = WithSchedule {
        learned: &Learned(None),
        schedule: &s,
    }
    .predict(&query(now, &tz));
    assert_eq!(p, None);
}

#[test]
fn schedule_brings_a_later_learned_unplug_forward() {
    // Learned: plugged in Sun night, usually unplugged Mon 10:00. Schedule: Mon 08:00.
    let tz = TimeZone::get("Pacific/Auckland").unwrap();
    let now = ts(&tz, 2026, 10, 11, 22, 0);
    let s = weekday_8am();
    let p = WithSchedule {
        learned: &Learned(Some(ts(&tz, 2026, 10, 12, 10, 0))),
        schedule: &s,
    }
    .predict(&query(now, &tz))
    .unwrap();
    assert_eq!(p.full_by, ts(&tz, 2026, 10, 12, 8, 0));
    assert_eq!(p.confidence, 0.8, "the learned confidence sets the hold");
    assert_eq!(p.source, "schedule");
    assert_eq!(
        p.reason,
        "learned: 6 similar sessions, schedule: full by Mon 08:00"
    );
}

#[test]
fn an_earlier_learned_unplug_is_kept() {
    // Learned unplug Tue 19:00 comes before the schedule's Wed 08:00: unchanged.
    let tz = TimeZone::get("Pacific/Auckland").unwrap();
    let now = ts(&tz, 2026, 10, 13, 17, 0);
    let s = weekday_8am();
    let learned = ts(&tz, 2026, 10, 13, 19, 0);
    let p = WithSchedule {
        learned: &Learned(Some(learned)),
        schedule: &s,
    }
    .predict(&query(now, &tz))
    .unwrap();
    assert_eq!(p.full_by, learned);
    assert_eq!(p.source, "learned");
}

#[test]
fn weekend_hold_still_learned() {
    // Fri 20:00, learned unplug Sat 09:00. The weekday rule's Mon 08:00 is later, so it doesn't matter.
    let tz = TimeZone::get("Pacific/Auckland").unwrap();
    let now = ts(&tz, 2026, 10, 9, 20, 0);
    let s = weekday_8am();
    let sat9 = ts(&tz, 2026, 10, 10, 9, 0);
    let p = WithSchedule {
        learned: &Learned(Some(sat9)),
        schedule: &s,
    }
    .predict(&query(now, &tz))
    .unwrap();
    assert_eq!(p.full_by, sat9);
}
