# NIRUNOTIFY

Native notification service and hardware OSD for the NIRU X11 desktop. It owns
`org.freedesktop.Notifications` when no other provider is active; Dunst remains
the fallback.

Part of the [NIRUX suite](https://github.com/nirucon/nirux): [NIRUWM](https://github.com/nirucon/niruwm) · [NIRUBAR](https://github.com/nirucon/nirubar) · [NIRUSCREENSAVER](https://github.com/nirucon/niruscreensaver) · [NIRULOCK](https://github.com/nirucon/nirulock) · [NIRULAUNCHER](https://github.com/nirucon/nirulauncher) · [NIRUNOTIFY](https://github.com/nirucon/nirunotify) · [NIRUSHOT](https://github.com/nirucon/nirushot).

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
nirunotify --check-config
```

The service exits with a clear error if another daemon owns the bus name; it
never evicts an existing provider.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
sh tests/ui-smoke.sh
```

MIT. Copyright 2026 Nicklas Rudolfsson.
