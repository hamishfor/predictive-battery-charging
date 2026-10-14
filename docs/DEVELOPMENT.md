# Development

This document is aimed at software developers.

## Layout

| Path | Description |
|---|---|
| `crates/pbc-hw` | Battery sysfs access, hardware profiles, safe threshold writes |
| `crates/pbc-model` | Pure logic: plug sessions, schedules, the charge controller, the charge-time model |
| `crates/pbc-store` | SQLite storage for events, sessions and samples |
| `crates/pbcd` | The daemon, its D-Bus API, and `pbcd-sleep-hook` |
| `crates/pbcctl` | CLI |
| `extension/` | GNOME Shell extension (GJS) |
| `profiles/` | Hardware profiles (TOML) |
| `hardware/` | Hardware reports, one YAML file per model, from `pbcctl report --write` |
| `dist/` | systemd unit, D-Bus and polkit policy, example config, source installer |
| `packaging/` | `.deb` build script and Debian control files |
| `.github/workflows/` | CI on every push and pull request, and the tagged release |
| `spike/` | Detailed hardware check scripts, for debugging a laptop that fails the guided test |

Hardware reports, one YAML file per model: [hardware/](../hardware/).

## Build and test

Prerequisites:

- **Rust 1.89 or newer**, from <https://rustup.rs> or your distribution (`sudo apt install cargo rustc` on Ubuntu 26.04).
- **A C compiler**, to build the bundled SQLite: `sudo apt install build-essential`.
- **git**, to clone this repository.
- **Optional, for the tile:** GNOME Shell 48 or newer, which provides the `gnome-extensions` command used by `extension/install.sh`.

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
gjs -m extension/tests/status-test.js
```

The tests run against fake sysfs trees, so they need no root and no particular laptop.

## Run without installing

The daemon can run as your user, on the session bus, with a scratch database. It only reads sysfs in this mode.

```sh
cargo run -p pbcd -- --session-bus --db /tmp/pbcd.db
cargo run -p pbcctl -- daemon --user
```

To try control mode safely, point `sysfs` in a test config at a fake tree. `crates/pbc-hw/tests/fake_sysfs.rs` shows the layout. Add `--session-upower` to talk to a fake UPower on the session bus.

## pbcctl for development

Every command is listed in the README's [Command line](../README.md#command-line) section. These options help during development:

| Option | Commands | Effect |
|---|---|---|
| `--sysfs DIR` | all | Read and write a fake sysfs tree instead of `/sys`. `crates/pbc-hw/tests/fake_sysfs.rs` shows the layout. |
| `--profiles DIR` | all | Load extra profile TOML files alongside the builtin ones, to try a new profile without rebuilding |
| `--user` | `daemon`, `charge-now`, `report`, `selftest` | Talk to a development pbcd on the session bus (`pbcd --session-bus`) |

pbcctl talks to pbcd through `busctl`, so it needs no D-Bus crate.

```sh
cargo run -p pbcctl -- --sysfs /tmp/fake probe
cargo run -p pbcctl -- --profiles ./my-profiles probe
pbcctl log --interval 30 > log.csv
```

To re-run charge-time learning on a log, convert it to the `ts,capacity,charging` columns `learn_from_csv` reads:

```sh
awk -F, 'NR > 1 { print $1 "," $4 "," ($2 == "Charging") }' log.csv > samples.csv
cargo run -p pbc-model --example learn_from_csv -- samples.csv
```

## Adding a laptop model

See [DEVICE_SUPPORT.md](DEVICE_SUPPORT.md#get-your-model-supported). The checklist is the pull request template in `.github/PULL_REQUEST_TEMPLATE/device_support.md`.

## Hardware profiles

pbcd drives the kernel's generic `charge_control_start_threshold` and `charge_control_end_threshold` files. A profile adds what varies by vendor: how to recognise the laptop, which values the firmware accepts, and sensible defaults. Profiles are TOML files in `profiles/`, compiled into pbcd. Extra ones can be loaded from `profiles_dir` in the config.

| Section | Field | Meaning |
|---|---|---|
| top level | `id`, `name` | Short id shown by `pbcctl probe`, and a readable name |
| `[match]` | `sys_vendor` | DMI vendor strings, any of which match |
| | `product_version_contains` | Substrings of the DMI product version |
| | `kernel_module` | A driver that must be loaded, if any |
| `[thresholds]` | `start_min`, `start_max`, `end_min`, `end_max` | Values the firmware accepts |
| | `min_gap` | Minimum distance between start and end. Writes are ordered to respect it. |
| | `persists_poweroff` | Whether the firmware keeps the limit while off. The self-checks confirm this per machine. |
| `[behaviour]` | `supported` | `charge_behaviour` values, if the driver has them |
| `[defaults]` | `hold_start`, `hold_end` | The fixed hold used by `pbcctl hold`. pbcd itself grades the hold by confidence. |
| `[quiet_wake]` | `validated` | Product versions where the quiet wake is confirmed to work |
| `[battery_labels]` | `BAT0 = "Internal"` and so on | Friendly names, shown only when every present battery has one |

`profiles/lenovo-thinkpad.toml` is a complete example. A laptop that matches no profile is never written to.

Self-check logic lives in `crates/pbc-model/src/selfcheck.rs` and `crates/pbcd/src/selfcheck.rs`. Results are stored in each machine's database. A contributor's `pbcctl report --write` copies them into the model's file in `hardware/`.

## Hardware reports

`hardware/<vendor>/<model>.yaml` holds what's known about one model. `<model>` is the DMI `product_name`, the same string as `sudo dmidecode -s system-product-name`, made lower-case with dashes. Lenovo is the exception: its `product_name` is a machine type such as `21NSS18R00`, which differs between regional variants of one model, so its `product_version` is used instead. `DmiInfo::model()` in `crates/pbc-hw/src/profile.rs` makes that choice, and the profiles' `[quiet_wake] validated` lists use the same string. It has one entry under `reports:` per machine type and BIOS that reported. The schema is `crates/pbc-hw/src/report.rs`. `--write` finds the clone by looking up from the current directory, or from a directory given as `--write DIR`, for `hardware/`, `profiles/` and `crates/pbc-hw/`. pbcctl is usually installed, not run from the clone, so it can't use its own location. Outside a clone it saves `<vendor>-<model>.yaml` in that directory and says where it belongs. `--write FILE` writes or merges into that file. Re-running it on the same machine replaces that machine's entry, but keeps any stronger self-check result, the learned curve and the notes. A quick re-run never drops a confirmed result.

`crates/pbc-hw/tests/hardware_reports.rs` checks every file:

- The file's path matches its `vendor` and `model`.
- A fake sysfs tree built from each report matches the profile the file names, and exposes the same batteries.
- Every model in a profile's `[quiet_wake] validated` has a report with `quiet_wake` working and confirmed.

Reports leave out serial numbers, host names, usage history and battery wear. `notes` is free text for anything a person saw or heard.

## D-Bus API

Service `org.pbc.Daemon1`, object `/org/pbc/Daemon1`, on the system bus.

| Member | Purpose |
|---|---|
| `StatusJson() → s` | Current state as JSON |
| `SessionsJson(x since) → s` | Recorded plug-in sessions as JSON |
| `ChargeNow(b on)` | Charge to full (checked by polkit action `org.pbc.daemon1.charge-now`) |
| `ConfirmCapability(s key, b working)` | Save a guided-test result. An empty key clears all (polkit action `org.pbc.daemon1.selftest`) |
| `Changed` signal | Something the UI shows has changed |
| `Mode`, `State`, `Plugged`, `Version` | Properties |

## Releases

`packaging/build-deb.sh` builds `dist-out/predictive-battery-charging_<version>_<arch>.deb` and the extension zip `dist-out/predictive-battery-charging@pbc.shell-extension.zip`. It needs `cargo`, `dpkg-deb`, and `gnome-extensions` or `zip`.

The `.deb` installs to `/usr`: `pbcd` and `pbcctl` in `/usr/bin`, the sleep hook in `/usr/libexec`, the unit in `/usr/lib/systemd/system`, and the extension system-wide in `/usr/share/gnome-shell/extensions`. `/etc/pbcd/config.toml` is a conffile, so upgrades keep local edits. Removing the package runs `pbcd --release` first, so the battery goes back to charging to 100%. Purging also deletes `/var/lib/pbcd`. `dist/install.sh` installs to `/usr/local` instead, and refuses to run while the package is installed.

To release, bump `[workspace.package] version` in the root `Cargo.toml`, run `cargo build` to update `Cargo.lock`, commit both, and merge to `main`.

The release is the last job of `.github/workflows/ci.yml`. On a push to `main`, once lint, tests, the Rust 1.89 build and the package job all pass, it checks whether `v<version>` is tagged. If not, it tags the commit and publishes a GitHub release with the `.deb` and zip that the package job built and test-installed. Otherwise it does nothing, so editing dependencies doesn't release. A version with a suffix, such as `0.3.0-rc.1`, is published as a pre-release and packaged as `0.3.0~rc.1`, which Debian sorts before `0.3.0`.

Don't create tags or releases by hand. A tag for the version stops the workflow from releasing it. To retry a failed release, fix it and push again, or run CI from the Actions tab.

The extension's `version` in `extension/metadata.json` is separate. extensions.gnome.org sets it on upload.
