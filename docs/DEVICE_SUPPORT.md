# Testing your laptop

This document is aimed at power users.


## Get your model supported

Support is added by pull request. Some basic knowledge about software development and contributing to a project is assumed. 

1. **Fork and clone the repository,** and make a branch.
2. **Run the guided test:** `sudo pbcctl selftest`.
3. **Save the report** from inside your clone: `pbcctl report --write`. It writes `hardware/<vendor>/<model>.yaml`, with your laptop's identity and the self-check results. If another owner has already reported your model, your machine is added to their file. Add anything you noticed to `notes:`.
4. **Make the changes** in [Adding support in a pull request](#adding-support-in-a-pull-request).
5. **Open a pull request** with the device support template. GitHub has no template picker for pull requests, so add `?expand=1&template=device_support.md` to the end of the "Compare changes" URL.


## If something goes wrong

- **The battery won't charge past a certain level:** run `pbcctl charge-now` to charge to full this time. To turn limits off altogether, run `sudo systemctl stop pbcd`, then `sudo pbcctl full` to set normal charging (0/100) on every battery.
- **The laptop won't stay asleep:** set `quiet_wake = "off"` in `/etc/pbcd/config.toml`, then run `sudo systemctl restart pbcd`.
- **The quiet-wake test says "the test wake didn't finish":** the lines after it say what stopped the wake and what to try. The hook's full log is in `sudo journalctl -k -b | grep pbcd-sleep-hook`.
- **A result looks wrong:** `sudo pbcctl selftest --reset` clears confirmed results. To clear everything, including what it observed, uninstall with `--purge` and reinstall.
