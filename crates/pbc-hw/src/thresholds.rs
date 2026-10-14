use crate::battery::Battery;
use crate::profile::ThresholdRules;
use crate::sysfs::{SysRoot, END_THRESHOLD, START_THRESHOLD};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Thresholds {
    pub start: u8,
    pub end: u8,
}

impl fmt::Display for Thresholds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "start={} end={}", self.start, self.end)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ThresholdError {
    #[error("start {0} outside allowed range {1}..={2}")]
    StartRange(u8, u8, u8),
    #[error("end {0} outside allowed range {1}..={2}")]
    EndRange(u8, u8, u8),
    #[error("start {start} must be at least {gap} below end {end}")]
    Gap { start: u8, end: u8, gap: u8 },
    #[error("battery {0} does not expose charge thresholds")]
    Unsupported(String),
    #[error("writing {attr}={value}: {source}")]
    Io {
        attr: String,
        value: u8,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Start,
    End,
}

impl Thresholds {
    pub fn validate(&self, r: &ThresholdRules) -> Result<(), ThresholdError> {
        if !(r.start_min..=r.start_max).contains(&self.start) {
            return Err(ThresholdError::StartRange(
                self.start,
                r.start_min,
                r.start_max,
            ));
        }
        if !(r.end_min..=r.end_max).contains(&self.end) {
            return Err(ThresholdError::EndRange(self.end, r.end_min, r.end_max));
        }
        if self.start.saturating_add(r.min_gap) > self.end {
            return Err(ThresholdError::Gap {
                start: self.start,
                end: self.end,
                gap: r.min_gap,
            });
        }
        Ok(())
    }

    fn valid_pair(start: u8, end: u8, gap: u8) -> bool {
        start.saturating_add(gap) <= end
    }
}

/// Order the two sysfs writes so every intermediate state keeps
/// `start + gap <= end`. Firmware commonly rejects writes that break this rule.
pub fn plan_writes(current: Thresholds, target: Thresholds, gap: u8) -> Vec<(Which, u8)> {
    let mut steps = Vec::new();
    let end_first = Thresholds::valid_pair(current.start, target.end, gap);
    let mut push = |w: Which| match w {
        Which::Start if current.start != target.start => steps.push((Which::Start, target.start)),
        Which::End if current.end != target.end => steps.push((Which::End, target.end)),
        _ => {}
    };
    if end_first {
        push(Which::End);
        push(Which::Start);
    } else {
        push(Which::Start);
        push(Which::End);
    }
    steps
}

/// Validate and apply thresholds to a battery, then read them back.
pub fn apply(
    root: &SysRoot,
    bat: &Battery,
    target: Thresholds,
    rules: &ThresholdRules,
) -> Result<Thresholds, ThresholdError> {
    target.validate(rules)?;
    let current = bat
        .thresholds()
        .ok_or_else(|| ThresholdError::Unsupported(bat.name.clone()))?;
    for (which, value) in plan_writes(current, target, rules.min_gap) {
        let attr = match which {
            Which::Start => START_THRESHOLD,
            Which::End => END_THRESHOLD,
        };
        root.write(bat.attr(attr), &value.to_string())
            .map_err(|source| ThresholdError::Io {
                attr: attr.into(),
                value,
                source,
            })?;
    }
    bat.thresholds()
        .ok_or_else(|| ThresholdError::Unsupported(bat.name.clone()))
}
