# Device check scripts

These scripts check whether a laptop's firmware behaves the way Predictive Battery Charging needs. Run them in order. The full walkthrough, and what to send back, is in [docs/DEVICE_SUPPORT.md](../docs/DEVICE_SUPPORT.md).

| Script | Checks | Needs |
|---|---|---|
| `01-persistence.sh` | Limits survive a reboot and a power-off | Two restarts |
| `02-enforcement.sh` | Charging stops at the limit and resumes when released | Charger plugged in |
| `03-rtc-wake.sh` | A timed wake from sleep works and re-sleeps | Charger, lid closed |
| `04-charge-curve.sh` | How long 80% to 100% takes | Charger, about an hour |
| `05-upower.sh` | How GNOME's charge setting interacts with ours | None |
| `06-quiet-wake.sh` | A wake during sleep can stay invisible to the desktop | Charger, lid closed, several runs |

- Every script needs `sudo` and a release build (`cargo build --release`).
- Results are appended to `spike/out/spike.log`.
- Set `PBC_BAT=BAT1` to check a second battery.
- Run `sudo spike/01-persistence.sh restore` when you're done, to put the limits back to 0/100.
