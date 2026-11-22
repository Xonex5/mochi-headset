# Headset battery

The battery of a USB headset that UPower doesn't see, read with
[headsetcontrol](https://github.com/Sapd/HeadsetControl), on Mochi's battery
card next to the mouse. Made for the Logitech PRO X Wireless, whose dongle
doesn't report its battery to the kernel.

## How it works

- The backend runs `headsetcontrol -b -o json` every 30 seconds and publishes
  each headset with a battery as `devices`, the same fields the battery
  module publishes for its devices. A headset that is off is left out.
- `qml/overrides/battery/Card.qml` replaces the battery module's card. It is
  the builtin `Card.qml` from Mochi 0.4.5, with the headsets from
  `Daemon.state("headset")` added after UPower's devices. A headset UPower
  already lists under the same name is not added twice.
- When the headset drops to `low` (15% by default), the island shows a notice
  once, like the battery module does for a mouse. It shows again only after
  the headset charges or climbs 5% above `low`.

## Install

1. `headsetcontrol` must be installed and work without root
   (`headsetcontrol -b` prints the level).
2. List the plugin in `~/.config/mochi/plugins.toml`:

   ```toml
   [plugins.headset]
   source = "path:plugins/headset"
   ```

3. `mochi plugins install headset --yes` builds it.
4. Add `"headset"` to `modules`. The settings panel keeps its own copy of
   `modules` in `~/.config/mochi/changes.toml`, so add it there when that file
   has one. Then run `mochi reload`.

## Use

- `mochi ipc headset status` prints each headset's level.
- `mochi ipc headset refresh` reads the level now.

Settings go in `[module.headset]`, see `settings.toml`.

## Mochi updates

The card override is a copy of the builtin `Card.qml`. After a Mochi update,
compare it with the new `modules/battery/qml/Card.qml` and carry the
headset lines (the `devices` property and its two uses) over if the builtin
changed. Bump the `mochi-sdk` tag in `Cargo.toml` to the installed release.
