//! Trains the unplug model on a simulated six-week history and writes its
//! weights and one prediction's feature rows as JSON, for the NPU benchmark.
//! Also times the native Rust prediction.
//! cargo run -q --release -p pbc-model --example export_hazard > /tmp/hazard.json
use jiff::civil::date;
use jiff::tz::TimeZone;
use pbc_model::hazard::{train, Hazard, HazardConfig, DIM};
use pbc_model::predict::{Predictor, Query};
use pbc_model::session::{End, Session, When};
use std::time::Instant;

fn main() {
    let tz = TimeZone::get("Pacific/Auckland").unwrap();
    let at = |m: i8, d: i8, h: i8, mi: i8| {
        date(2026, m, d)
            .at(h, mi, 0, 0)
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp()
            .as_second()
    };
    let mut h = Vec::new();
    for i in 0..42i64 {
        let frac = (i as f64 * 0.618_034).fract();
        let a = at(9, 1, 18, 0) + i * 86400 + (frac * 8.0 * 3600.0) as i64;
        let b = at(9, 2, 7, 40) + i * 86400;
        h.push(Session {
            plugged: When::exact(a),
            end: End::Unplugged(When::exact(b)),
            capacity_at_plug: Some(50),
            capacity_at_end: Some(100),
            merged_gaps: 0,
        });
    }
    let now = at(10, 13, 23, 0);
    let t0 = Instant::now();
    let m = train(&h, now, &tz, &HazardConfig::default()).unwrap();
    let train_ms = t0.elapsed().as_secs_f64() * 1e3;
    let q = Query {
        now,
        plugged_since: Some(When::exact(at(10, 13, 22, 30))),
        capacity: Some(50),
        history: &h,
        tz: &tz,
    };
    let hz = Hazard {
        model: &m,
        cfg: HazardConfig::default(),
        accuracy_factor: 1.0,
    };
    let n = 10_000;
    let t1 = Instant::now();
    let mut last = None;
    for _ in 0..n {
        last = hz.predict(&q);
    }
    let predict_us = t1.elapsed().as_secs_f64() * 1e6 / n as f64;
    eprintln!(
        "rust: train {train_ms:.1} ms, predict {predict_us:.2} us, full_by {:?}",
        last.map(|p| p.full_by)
    );
    println!(
        "{}",
        serde_json::json!({"dim": DIM, "weights": m.weights, "rust_train_ms": train_ms, "rust_predict_us": predict_us})
    );
}
