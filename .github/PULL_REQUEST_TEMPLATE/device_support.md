<!--
Device support pull request. Full instructions: docs/DEVICE_SUPPORT.md
GitHub opens this template when the compare URL ends in
?expand=1&template=device_support.md
-->

## Laptop

| | |
|---|---|
| Manufacturer | |
| Family | |
| Model | |
| Generation or year | |
| Number of batteries | |
| Distribution and desktop | |

## Report

- [ ] Ran `sudo pbcctl selftest`, then `pbcctl report --write` from inside the clone
- [ ] `hardware/<vendor>/<model>.yaml` is in this pull request, with anything you noticed in `notes:`

## What this changes

- [ ] Adds a new profile in `profiles/`, and adds it to `BUILTIN` in `crates/pbc-hw/src/profile.rs`
- [ ] Extends an existing profile: ___________
- [ ] Only adds a report, or marks a model as tested. The profile already matched.

## Checklist

- [ ] Every battery in the report has `start_threshold: true` and `end_threshold: true`
- [ ] `[match]` uses the report's `vendor` and `model`, and a driver from its `drivers`
- [ ] `[thresholds]` ranges and `min_gap` match what the firmware accepts
- [ ] The report's `model` is added to `[quiet_wake] validated`, only if its `quiet_wake` check is `working`
- [ ] A row is added to the supported-hardware table in `README.md`, marked "Tested" only if every check is `working`
- [ ] `cargo test` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes

## Anything unusual

<!-- Firmware quirks, steps that failed, things you saw or heard during the wake test. -->
