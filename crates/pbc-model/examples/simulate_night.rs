//! Shows how the controller grades the limit over one night, for a learned
//! routine of weekday nights plugged in about 22:00 and unplugged about 07:45.
//! cargo run -q -p pbc-model --example simulate_night
use jiff::civil::date;
use jiff::tz::TimeZone;
use pbc_model::control::{decide, Input, Policy, State};
use pbc_model::learned::{Routine, RoutineConfig};
use pbc_model::predict::{Predictor, Query};
use pbc_model::session::{End, Session, When};

fn main() {
    let tz = TimeZone::get("Pacific/Auckland").unwrap();
    let at = |d: i8, h: i8, m: i8| {
        date(2026, 10, d)
            .at(h, m, 0, 0)
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp()
            .as_second()
    };
    for weeks in [1usize, 2, 4] {
        // Weekday nights over `weeks` weeks, ending the Friday before Mon 19 Oct.
        let mut h = Vec::new();
        for w in 0..weeks {
            for d in 0..5 {
                let day = 19 - 7 * (w as i8 + 1) + d;
                if day < 1 {
                    continue;
                }
                let jit = ((w * 5 + d as usize) * 7 % 30) as i8;
                h.push(Session {
                    plugged: When::exact(at(day, 21, 45 + jit / 3)),
                    end: End::Unplugged(When::exact(at(day + 1, 7, 30 + jit / 2))),
                    capacity_at_plug: Some(50),
                    capacity_at_end: Some(100),
                    merged_gaps: 0,
                });
            }
        }
        let start = at(19, 22, 0);
        let pol = Policy::default();
        let r = Routine::new(RoutineConfig::default(), 1.0);
        let mut state = State::Unplugged;
        println!("\n{} weeks of history ({} sessions):", weeks, h.len());
        for (hh, mm) in [
            (22, 5),
            (23, 0),
            (1, 0),
            (3, 0),
            (4, 30),
            (5, 30),
            (6, 15),
            (6, 40),
            (7, 0),
        ] {
            let day = if hh >= 12 { 19 } else { 20 };
            let now = at(day, hh, mm);
            let q = Query {
                now,
                plugged_since: Some(When::exact(start)),
                capacity: Some(80),
                history: &h,
                tz: &tz,
            };
            let p = r.predict(&q);
            let i = Input {
                now,
                online: true,
                prediction: p.as_ref(),
                override_requested: false,
                secs_to_full: 40 * 60,
                secs_ready_to_full: 25 * 60,
            };
            let d = decide(&state, &i, &pol);
            println!(
                "  {hh:02}:{mm:02}  limit {:>3}%   {}",
                d.target.end,
                d.reason.split(" (").next().unwrap_or("")
            );
            state = d.state;
        }
    }
}
