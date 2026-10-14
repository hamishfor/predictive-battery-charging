# Predictive Battery Charging for Linux

[![CI](https://github.com/hamishfor/predictive-battery-charging/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/hamishfor/predictive-battery-charging/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/hamishfor/predictive-battery-charging?include_prereleases)](https://github.com/hamishfor/predictive-battery-charging/releases/latest)
[![License: GPL v3+](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](LICENSE)

The battery health tooling for Linux is crude, Gnome's own tooling just holds the battery at 80%, you can switch profiles to then fully charge, but you have to;
- Remember to select fully charge 30 minutes before you intend to unplug.
- Remember to return the setting afterwards. 

Predictive Battery Charging (`PBC`), is the result of a desire for more automated battery health control.

Currently, it's what I'd describe as cautious. It will charge fully untill it reaches a degree of confidence around your schedule where it will start to hold your battery back, only finishing charging when it gets near to when it thinks you'll unplug. Schedules will give it a confidence level of 1, so will more confidently frequently hold back charging. If you absolutely rely on your laptops battery to be at 100% whenever you unplug it. I can't recommend installing PBC.

It runs as a small system daemon (`pbcd`), with an optional GNOME Shell extension to execute common battery management tasks and display information.

Currently in active development.

## Features

- **Learns your routine.** It uses six months of plug-in history, weighted towards recent weeks so new habits are picked up quickly. All data remains local to the device.
- **Holds by confidence.** A confident prediction of a long session, such as overnight, holds at `hold_min`. A less certain one holds higher, up to `hold_max`. With no prediction it charges to 100%.
- **Charge hedging.** When an unplug may be near, it steps up to `ready_pct`, so leaving early still gives a good charge. Then it finishes at 100% just before you usually unplug.
- **Schedules.** Optional "full by" times, such as weekday mornings.
- **Quiet wake.** If the charge needs to finish while the laptop is asleep, it wakes for a few seconds to raise the limit, then goes back to sleep. Your screen, sound, network and apps stay asleep. Opening the lid or pressing a key wakes the laptop normally. It is on for tested models and after a guided `pbcctl selftest`, and turns itself off if a wake ever goes wrong. Otherwise it starts charging to 100% before the laptop sleeps. See `quiet_wake` in [Configuration](#configuration).
- **Multi-battery.** Supports multiple internal batteries, such as ThinkPad Power Bridge models.
- **GNOME integration.** A Quick Settings tile, and GNOME's own Battery Charging setting as the on/off switch.

## Supported hardware

Predictive Battery Charging works with laptops whose kernel driver exposes battery charge limits, and with working sleep profiles. This repo maintains a list of tested devices that support everything pbcd requires. Most modern laptop devices _should_ work. Typically, devices with enterprise features such as those found in classes such as Dell Inspiron, or open hardware specifications such as Framework, Starlabs, and Clevo based designs will have a higher degree of success.

Devices with the status: `tested` in the following support list will get all features enabled immediately.

| Manufacturer | Family | Generation | Profile | Status |
|---|---|---|---|---|
| Lenovo | ThinkPad E | 430+ | `lenovo-thinkpad` | Expected to work |
| Lenovo | ThinkPad L | 420+ | `lenovo-thinkpad` | Expected to work |
| Lenovo | ThinkPad P | 51/71+ | `lenovo-thinkpad` | Expected to work |
| Lenovo | ThinkPad T Series | 420+ | `lenovo-thinkpad` | Expected to work |
| Lenovo | ThinkPad X | 220+ | `lenovo-thinkpad` | Expected to work |
| Lenovo | ThinkPad X1 Carbon | Gen 1+ | `lenovo-thinkpad` | Expected to work |
| Lenovo | ThinkPad X1 Carbon | Gen 13 | `lenovo-thinkpad` | Tested |
| Lenovo | ThinkPad Z | 13/16 Gen 1+ | `lenovo-thinkpad` | Expected to work |

**Status key:**
- **Tested:** someone ran the guided test on that model, every step passed, and committed the results. All features should automatically enable.
- **Expected to work:** the profile matches, and pbcd checks the laptop itself as you use it. Quiet wake disabled until `pbcctl selftest` is run.
- **Unsupported:** tested as partial or no support.

On a laptop without a matching profile, `pbcd` only records charging activity and never changes a setting. Check your laptop with `pbcctl probe` after installing. To get yours supported, see [Help support your laptop](#help-support-your-laptop).

## Requirements

| Requirement | Parameters |
|---|---|
| Laptop | Charge limits in `/sys/class/power_supply/BAT*/charge_control_*_threshold`, and a matching hardware profile (see above) |
| Kernel | Linux 5.9 or newer, for the standard charge-limit files |
| Init | systemd, with logind |
| D-Bus services | UPower and polkit |
| Desktop (Optional) | GNOME 48+ for the Quick Settings tile and the Settings switch. |

Without GNOME, the daemon and `pbcctl` still work. Turn it on and off in the config file with `upower_switch = false`. Feel free to build management tooling for other WM's and DE's against this application.

## Install

### Ubuntu 26.04 and Debian

Download the `.deb` from the [latest release](https://github.com/hamishfor/predictive-battery-charging/releases/latest), then:

```sh
sudo apt install ./predictive-battery-charging_*_amd64.deb
```

If you then want the Gnome extension; log out and back in, then turn on the tile.

```sh
gnome-extensions enable predictive-battery-charging@pbc
```

The tile appears in **Quick Settings**, and takes control of the GNOME power **Preserve Battery Health** setting.

### From source

Prerequisites:

- **Rust 1.89 or newer**, from <https://rustup.rs> or your distribution (`sudo apt install cargo rustc` on Ubuntu 26.04).
- **A C compiler**, to build the bundled SQLite: `sudo apt install build-essential`.
- **git**, to clone this repository.
- **Optional, for the tile:** GNOME Shell 48 or newer, which provides the `gnome-extensions` command used by `extension/install.sh`.

The runtime needs in [Requirements](#requirements) apply too.

Install the pre-reqs
```sh
sudo apt install cargo rustc git build-essential
```

Clone the repo.

```sh
git clone https://github.com/hamishfor/predictive-battery-charging.git
cd predictive-battery-charging
```

Build and install.

```sh
cargo build --release
sudo dist/install.sh
extension/install.sh          # optional; no sudo needed
```

Log out and back in to load the GNOME extension.

Check that the daemon found your hardware:

```sh
pbcctl probe
pbcctl daemon
```

`probe` should list a hardware profile and your batteries. If it shows `profile: none`, your laptop isn't supported yet. pbcd will only record activity.

## Enable PBC

We high-jack Gnome's settings.

In **Settings → Power → Battery Charging**, choose **Preserve Battery Health**. To disable `pbcd` you can select **Maximise Charge**.

| GNOME Settings | Predictive Battery Charging |
|---|---|
| Preserve Battery Health | On |
| Maximize Charge | Off. Always charges to 100%. |

```sh
sudoedit /etc/pbcd/config.toml
```

```toml
[[schedule]]
days = ["mon", "tue", "wed", "thu", "fri"]
full_by = "08:00"
```

Then run `sudo systemctl restart pbcd` or reboot.

## Command line

`pbcctl` checks the laptop, shows what pbcd is doing, and can manually manage charge limits. `pbcctl <command> --help` lists each command's options.

| Command | What it does | Needs |
|---|---|---|
| `pbcctl --version` | pbcctl's version and the running pbcd's. A build from source adds the git commit, such as `0.1.0 (git b288395)`. `--user` asks a development pbcd. | |
| `pbcctl probe` | Laptop identity, the matched profile and its accepted limits, each battery, sleep states and RTC wake | |
| `pbcctl status` | Each battery's charge, status, power and limits, plus the combined charge | |
| `pbcctl daemon` | pbcd's full status as JSON: state, prediction, hold level, alerts, self-checks. `--sessions N` adds the last N days of plug-in sessions. | pbcd running |
| `pbcctl charge-now` | Charge to 100% this plug-in, or the next one if on battery. `--cancel` goes back to predictive charging. | pbcd running |
| `pbcctl report` | Hardware report as YAML, with the self-check results. `--write` saves it. See [Help support your laptop](#help-support-your-laptop). | |
| `pbcctl selftest` | Guided checks, about 15 minutes. `--only enforce\|resume\|quiet_wake` runs one. `--reset` clears confirmed results. | sudo, pbcd running |
| `pbcctl set --start N --end N` | Write limits to every battery, or `--battery BAT1`. Values are checked against the profile. `--force` writes on a laptop with no profile, with generic checks. | sudo |
| `pbcctl hold` | Write the profile's fixed hold, such as 75/80 | sudo |
| `pbcctl full` | Write 0/100, normal charging to full | sudo |
| `pbcctl log` | Print a CSV sample every 30 s (`--interval`), for measuring a charge curve. `--count N` stops after N samples. | |

pbcd owns the limits while it runs in `control` mode. It reports a limit written by `set`, `hold` or `full` as overwritten, then writes its own back within a minute. To charge to full, use `charge-now`. To set limits by hand, stop pbcd first (`sudo systemctl stop pbcd`) or set `mode = "collect"`.

`set`, `hold` and `full` read every limit back after writing it, and fail if the firmware reports something else.

## Safety

- Below 25% it always charges, whatever it has learned, defined, or scheduled. You can change this level with `floor_pct` (see [Configuration](#configuration)).
- It only writes the battery charge limits. It never changes other device or app settings.
- Before shutdown it sets the limit it wants held while the laptop is off. Most firmware keeps it. pbcd checks this after each restart.
- If charging ever fails to start again after a hold, it stops limiting charging on that laptop for good.
- Uninstalling restores normal charging to 100%.

## Configuration

All settings live in `/etc/pbcd/config.toml`, with comments. Restart the daemon after changing any of them. The most useful ones:

| Setting | Default | Purpose |
|---|---|---|
| `mode` | `control` | `collect` to only record, never limit |
| `[[schedule]]` | none | Optional "full by" times. A hold never runs past one. |
| `hold_min`, `hold_max` | `70`, `99` | Range of the hold. Confident predictions hold low, weak ones high. |
| `ready_pct` | `90` | The level it steps up to when an unplug may be near |
| `floor_pct` | `25` | Always charge below this level, whatever is learned or scheduled. On two-battery models it uses the combined charge. |
| `quiet_wake` | `auto` | Brief wake during sleep to finish charging |
| `upower_switch` | `true` | Use GNOME's Battery Charging setting as the on/off switch |

## Uninstall

Both put the battery back to normal charging to 100%.

```sh
sudo apt remove predictive-battery-charging   # or: sudo apt purge ..., to also delete settings and learned history
```

From source:

```sh
extension/install.sh --uninstall
sudo dist/install.sh --uninstall     # add --purge to also delete learned history
```

## Privacy

Everything stays local on the laptop, in `/var/lib/pbcd/pbcd.db`. Unless you chose to contribute a profile here.

## Status

Under active development.

## Help support your laptop

Predictive Battery Charging checks itself as you use it, and switches off anything that doesn't work on your laptop. For better results sooner, run the 15-minute guided test:

```sh
sudo pbcctl selftest
```

To add your laptop model to the supported list for everyone, open a pull request with the results. [docs/DEVICE_SUPPORT.md](docs/DEVICE_SUPPORT.md) has the full checklist.

Developers: see [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## License and Disclaimer

Copyright (C) 2026 Predictive Battery Charging (`PBC`) Contributors

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. See [LICENSE](LICENSE).

PBC was built with the use of manually written code and coding agents, both proprietary and open-weight. If that's something you can't support, feel free to skip this tool. This is a hobby project, this program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
