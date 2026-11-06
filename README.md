# NIRUNOTIFY

**Part of [NIRUX - A minimal X11 desktop by Nicklas Rudolfsson](https://github.com/nirucon/nirux).**

**Early beta.** Native notification service and hardware OSD for the NIRU X11 desktop. It owns
`org.freedesktop.Notifications` when no other provider is active; Dunst remains
the fallback.


## Features

- Replacement IDs, timeouts, close reasons, urgency accents, clickable actions,
  `ActionInvoked`, bounded text and memory-only history.
- Native popups that follow the active NIRUWM theme, including live theme and
  reload changes.
- Popups stay above ordinary windows without taking keyboard focus.
- Monitor selection from the pointer, RandR primary or a named output.
- Critical timeout, DND policy and bar-strut gap.
- Suppression while `_NIRU_LOCK_READY` names a live lock window.
- DND/history controls over a documented Unix-socket CLI.
- Coalesced hardware OSD, for example `nirunotify --osd volume 45`.
- Notification sounds through one coalescing worker that reaps its child, so
  notification bursts cannot stack audio or leave zombie processes.
- The NIRUX system sounds: volume, mute, brightness and screenshot feedback,
  charger and battery, devices (USB, audio, monitors) and the lock screen.
  Played natively, without a shell; charger and devices come from kernel
  uevents, with no polling.
- Bounded live/replacement maps, 100-entry history and a 100-command queue.

No markup or images; the advertised capabilities are `body` and `actions`.

## Install

Use [NIRUX](https://github.com/nirucon/nirux#install) for the complete desktop,
or install the x86_64/glibc release separately:

```sh
git clone https://github.com/nirucon/nirunotify
cd nirunotify
./install.sh
./install.sh --build
```

Requires a session D-Bus and X11. Settings live in
`~/.config/nirunotify/config.toml`; see [config.example.toml](config.example.toml).
Colours are not duplicated: surfaces follow `theme` in
`~/.config/niruwm/config.toml` and the shared appearance font and spacing.

## Use

```sh
nirunotify
notify-send "Hello" "Body"
nirunotify --dnd on|off|toggle|status
nirunotify --history
nirunotify --count
nirunotify --dismiss ID
nirunotify --clear
nirunotify --osd volume 45
nirunotify --osd volume 45% --sound volume-up 45
nirunotify --sound auth-fail
nirunotify --check-config
```

## System sounds

`nirunotify --sound EVENT [LEVEL] [muted|unmuted]` plays a NIRUX sound event
(`volume-up`, `volume-down`, `volume-limit`, `mute`, `unmute`,
`brightness-up`, `brightness-down`, `screenshot`, `power-plug`,
`power-unplug`, `battery-low`, `battery-critical`, `battery-full`,
`device-connect`, `device-disconnect`, `auth-fail`, `lock`, `unlock`, and
the session events). `--osd … --sound …` shows the OSD and plays the sound in
one call; `LEVEL` picks the volume tick for the new level, and `unmuted` saves
a `wpctl` call when the caller knows the sink state. Other programs can send
the same request as one line, `sound EVENT`, on the control socket.

The settings are NIRUX's: `~/.config/nirux/sounds.toml`, managed with
`nirux sounds`, reread when the file changes. The sound files are
`~/.config/nirux/sounds/EVENT.ogg` (yours) or the NIRUX ones under
`$NIRUX_PREFIX/share/nirux/sounds`. A held key plays at most one tick per
`rate_limit_ms`; Do Not Disturb silences notification sounds only; a muted sink
silences everything when `respect_mute` is on.

Charger, battery and device events come from one thread that sleeps on a
kernel uevent socket: the charger (`power_supply` Mains/USB `online`), the
machine's batteries (`capacity`, `status`; peripherals with `scope = Device`
are ignored), `usb` devices (not interfaces or root hubs), `sound` cards,
`drm` connector status (monitors) and, when enabled, `bluetooth` links. A
burst such as a dock is one sound, and the charger wins over its devices.
`--check-config` prints what the watcher reads now, and
`NIRUNOTIFY_DEBUG_SOUNDS=1` explains each decision on stderr. See
[NIRUX sounds](https://github.com/nirucon/nirux/blob/main/docs/SOUNDS.md).

The service exits with a clear error if another daemon owns the bus name; it
never evicts an existing provider.

## NIRUX suite

[NIRUX](https://github.com/nirucon/nirux) · [NIRUWM](https://github.com/nirucon/niruwm) · [NIRUBAR](https://github.com/nirucon/nirubar) · [NIRUSCREENSAVER](https://github.com/nirucon/niruscreensaver) · [NIRULOCK](https://github.com/nirucon/nirulock) · [NIRULAUNCHER](https://github.com/nirucon/nirulauncher) · [NIRUNOTIFY](https://github.com/nirucon/nirunotify) · [NIRUSHOT](https://github.com/nirucon/nirushot)

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
sh tests/ui-smoke.sh target/release/nirunotify
sh tests/sounds-smoke.sh target/release/nirunotify
```

`tests/fixtures/sounds-default.toml` is the `sounds.toml` a new NIRUX writes
(`sounds.example.toml` in NIRUX); keep the two identical.

MIT. Copyright 2026 Nicklas Rudolfsson.
