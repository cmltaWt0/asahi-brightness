# asahi-brightness

Auto-brightness daemon for Apple Silicon laptops (M1/M2/M3) on Linux.
Reads the AOP ALS via IIO sysfs, drives **both display and keyboard** backlights,
and stays out of the way of manual sliders and idle daemons.

Compositor-agnostic: works under Hyprland, niri, sway, KDE, and any other
Wayland compositor that implements `ext-idle-notify-v1`.

## Behaviour

- Polls `/sys/bus/iio/devices/<aop-sensors-als>/in_illuminance_input` (auto-discovered).
- Smooths lux with an EMA filter; only retargets when lux change crosses a hysteresis band.
- Maps lux to brightness via per-channel piecewise-linear curves (configurable).
- **Hard cutoff for keyboard**: above `cutoff_lux` the keyboard backlight is forced to 0.
- Smooth ramps (~200 ms / 20 steps) so transitions are imperceptible.
- **Manual override (per channel)**: an external change to display brightness
  (waybar slider, `brightnessctl`, `dms ipc call brightness ...`, function keys)
  pauses **display** auto-control for `override_timeout_s` — keyboard auto-control
  keeps tracking lux. An external change to keyboard brightness pauses **keyboard**
  auto-control independently. Each per-channel override exits when its timer
  expires or when ambient lux drifts more than `override_lux_drift_pct` from the
  value at override entry. Use `asahi-brightness pause` to pause **both** channels
  globally (e.g. for presentations).
- **Idle handoff**: while the compositor reports the seat idle, the daemon stops writing,
  letting `hypridle` / `swayidle` etc. own the screen. After resume there's a small grace
  period so `brightnessctl -r` lands cleanly before normal control resumes.

## Install

```sh
./packaging/install.sh
systemctl --user enable --now asahi-brightness.service
```

The installer builds in release mode, drops the binary in `~/.local/bin`, registers
a systemd user unit tied to `graphical-session.target`, and installs a udev rule so
your user (`video` group) can write the backlight sysfs files.

## CLI

```
asahi-brightness            # run daemon (default)
asahi-brightness status     # JSON: lux, current %s, per-channel override/idle/pause flags
asahi-brightness pause [N]  # pause BOTH channels for N seconds (0 = until resume)
asahi-brightness resume     # clear pause and per-channel overrides
asahi-brightness nudge ±N   # bias display curve by N % until lux changes (display only)
asahi-brightness dump-config
```

`status` returns `display_override_active` and `keyboard_override_active` as
separate fields. A channel-level override is set automatically when an external
process writes that channel's sysfs entry; it does not affect the other channel.
The global `pause` / `resume` commands always cover both channels.

Useful Hyprland keybinds (binds.conf):

```
bind = $mainMod, F1, exec, asahi-brightness pause 0
bind = $mainMod SHIFT, F1, exec, asahi-brightness resume
bind = $mainMod, F2, exec, asahi-brightness nudge -10
bind = $mainMod, F3, exec, asahi-brightness nudge 10
```

## Config

On first run the daemon writes a default config to
`~/.config/asahi-brightness/config.toml`. Pass `--config <path>` to override.

```toml
poll_interval_ms = 250
ramp_duration_ms = 200
ema_alpha = 0.2
override_timeout_s = 60
override_lux_drift_pct = 75
idle_timeout_ms = 30000

[display]
device = "apple-panel-bl"
class = "backlight"
min_pct = 5
hysteresis_pct = 1.5
curve = [[0,5], [10,15], [50,30], [200,55], [600,80], [2000,100]]

[keyboard]
device = "kbd_backlight"
class = "leds"
min_pct = 0
hysteresis_pct = 2
cutoff_lux = 150
curve = [[0,40], [20,25], [80,10], [150,0]]
```

## Tuning the curve

`asahi-brightness status` prints the current smoothed lux. Sit in the lighting
condition you want to calibrate, note the lux, then edit the curve point
nearest that lux.

The curve is piecewise-linear; the daemon interpolates between the two
nearest control points. Outside the curve range it clamps to the first/last
point. `min_pct` is a hard floor below which the channel never drops,
useful on OLED to avoid a fully black panel.

## Coexistence

- **`brightnessctl`** / **`dms`** / waybar sliders: an external write to a
  channel's sysfs entry puts **only that channel** into override. The other
  channel keeps tracking lux. So `brightnessctl set 50%` only pauses display
  auto-control; `brightnessctl --device=kbd_backlight set 0` only pauses
  keyboard auto-control. Adjust freely; the daemon yields per channel.
- **`hypridle`** / **`swayidle`**: the daemon stops writing when the compositor
  reports idle, so your idle config's `brightnessctl -s set 10` and
  `brightnessctl -r` work unchanged. Idle is global (the compositor reports
  per-seat, not per-device), so both channels pause and resume together.

## License

MIT
