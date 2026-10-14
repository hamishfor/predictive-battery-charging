# Testing your laptop

This document is aimed at power users.

Predictive Battery Charging will automatically configure itself over time as it monitors your behaviour. Depending on your behaviour, this can take anywhere from a week to a couple of months of normal use. If you choose, you can accelerate its learnings with a short guided test, if you'd like can upload your hardware tree to this repo and add your laptop model to the supported list so the daemon recognises itself as being tested on this device without having to run the tests each time.

pbcd uses the kernel's standard battery charge-limit interface. Any laptop whose driver exposes it can probably work, given a hardware profile. That includes many Dell XPS and Latitude models, and laptops with open firmware such as Framework, and Clevo-based designs from System76 and Tuxedo.

## What it checks by itself

While you use the laptop normally, pbcd watches four things:

| Check | Judged when |
|---|---|
| Charging stops at the limit | Each time it holds the battery |
| Charging starts again when the limit is lifted | Each time it finishes charging to full |
| The laptop keeps the limit while powered off | Each restart after a shutdown |
| A wake during sleep stays invisible | Each night it finishes charging while asleep |

If a check fails, pbcd switches that feature off on your laptop:
- **Charging didn't start again:** it stops limiting charging altogether. This is the one failure that could leave a battery unable to charge, so it's never retried.
- **A wake during sleep went wrong:** it finishes charging before sleep instead.

On a model that isn't on the tested list, the wake during sleep stays off unless you run the guided test, or set `quiet_wake_try_untested = true` in the config. Nothing on the laptop can tell whether the screen lit up or the fan spun, so someone has to watch it once.

See the results at any time:

```sh
pbcctl report
```

It prints a YAML report. The `checks:` section has the results.

## Guided test (about 15 minutes)

```sh
sudo pbcctl selftest
```

It walks you through each step and saves the results to pbcd as it goes. pbcd uses them straight away. You'll need the charger plugged in, the battery between 15% and 95%, and no external monitor.

| Step | What happens | Time |
|---|---|---|
| Charge limit | Sets a limit just below the current charge, checks charging stops, then lifts it and checks charging starts again | 3 minutes |
| Wake during sleep | Schedules a wake two minutes ahead. You close the lid, watch and listen, then answer one question. | 10 minutes |

To run one step on its own, use `--only enforce`, `--only resume` or `--only quiet_wake`. To clear what you've confirmed, so pbcd goes back to what it has observed, use `--reset`.

## Get your model supported

Support is added by pull request. You do the testing and the change yourself, using the checklist below, so a maintainer only has to review it.

1. **Fork and clone the repository,** and make a branch.
2. **Run the guided test:** `sudo pbcctl selftest`.
3. **Save the report** from inside your clone: `pbcctl report --write`. It writes `hardware/<vendor>/<model>.yaml`, such as `hardware/lenovo/thinkpad-x1-carbon-gen-13.yaml`, with your laptop's identity, its charge-limit files and the self-check results. If another owner has already reported your model, your machine is added to their file. Add anything you noticed to `notes:`.
4. **Make the changes** in [Adding support in a pull request](#adding-support-in-a-pull-request).
5. **Open a pull request** with the device support template. GitHub has no template picker for pull requests, so add `?expand=1&template=device_support.md` to the end of the "Compare changes" URL.

A model is marked Tested once a guided test passes every step. Other owners of that model then get the wake during sleep from day one, without running the test themselves.

### Adding support in a pull request

A profile doesn't drive the hardware. The kernel's vendor driver, such as `thinkpad_acpi`, exposes the standard charge-limit files, and pbcd only reads and writes those. The profile says which laptops pbcd may write to, and which values their firmware accepts. A laptop that matches no profile is never written to.

Check the report first:

| Report shows | What it means |
|---|---|
| `start_threshold: true` and `end_threshold: true` on each battery | The kernel exposes charge limits. Carry on. |
| Either one `false` | The kernel driver doesn't expose both limits. A profile can't fix this, so the laptop can't be supported until its kernel driver adds them. |
| `profile: null` | No profile matches. Add one (step 1). |
| `profile:` followed by a name | A profile matches already. Skip step 1. |

Then:

1. **Add or extend a profile** in `profiles/`. Copy `profiles/lenovo-thinkpad.toml`, and set the `[match]` section from the report's `vendor` and `model`, plus a driver from its `drivers`. Set `[thresholds]` from your firmware's accepted ranges. [DEVELOPMENT.md](DEVELOPMENT.md#hardware-profiles) explains every field. A new profile file must also be added to `BUILTIN` in `crates/pbc-hw/src/profile.rs`. Then run `pbcctl report --write` again, so the report names the new profile.
2. **Mark the quiet wake validated,** only if the report's `quiet_wake` check reads `working`. Add the report's `model`, exactly as written, to `[quiet_wake] validated` in the profile.
3. **Add a row** to the supported-hardware table in `README.md`, with the manufacturer, family, generation and profile. Use "Tested" only if every check reads `working`. Otherwise use "Expected to work".
4. **Run the checks:**

   ```sh
   cargo test
   cargo clippy --all-targets -- -D warnings
   ```

   The tests replay every file in `hardware/` against the profiles. They check that your report matches the profile it names, and that every validated model has a report confirming the quiet wake.

If your laptop fails a step in the guided test, the scripts in `spike/` check each behaviour in more detail. See [spike/README.md](../spike/README.md).

## If something goes wrong

- **The battery won't charge past a certain level:** run `pbcctl charge-now` to charge to full this time. To turn limits off altogether, run `sudo systemctl stop pbcd`, then `sudo pbcctl full` to set normal charging (0/100) on every battery.
- **The laptop won't stay asleep:** set `quiet_wake = "off"` in `/etc/pbcd/config.toml`, then run `sudo systemctl restart pbcd`.
- **The quiet-wake test says "the test wake didn't finish":** the lines after it say what stopped the wake and what to try. The hook's full log is in `sudo journalctl -k -b | grep pbcd-sleep-hook`.
- **A result looks wrong:** `sudo pbcctl selftest --reset` clears confirmed results. To clear everything, including what it observed, uninstall with `--purge` and reinstall.

## Developers

The scripts in `spike/` are the original detailed hardware checks. They're useful when debugging a laptop that fails the guided test. See [DEVELOPMENT.md](DEVELOPMENT.md).
