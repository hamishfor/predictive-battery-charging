//! Re-run charge-time learning on a CSV of `ts,capacity,charging(0/1)` samples.
//! cargo run -q -p pbc-model --example learn_from_csv -- samples.csv
use pbc_model::charge::{ChargeModel, ChargeSample};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: learn_from_csv <samples.csv>");
    let samples: Vec<ChargeSample> = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            Some(ChargeSample {
                ts: f.first()?.parse().ok()?,
                capacity: f.get(1)?.parse().ok()?,
                charging: *f.get(2)? == "1",
            })
        })
        .collect();
    let d = ChargeModel::default();
    let mut m = ChargeModel::default();
    let n = m.learn(&samples, 600, 3);
    let per = |m: &ChargeModel, c: u8| m.secs_to_full(c) - m.secs_to_full(c + 1);
    println!("{} samples, {n} levels learned", samples.len());
    println!("level  default  learned");
    for c in (20..100).step_by(5) {
        println!("{c:>4}%  {:>6}s  {:>6}s", per(&d, c), per(&m, c));
    }
    for from in [20u8, 50, 80] {
        println!(
            "{from}->100: default {:.0} min, learned {:.0} min",
            d.secs_to_full(from) as f64 / 60.0,
            m.secs_to_full(from) as f64 / 60.0
        );
    }
}
