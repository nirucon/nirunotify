# NIRUNOTIFY

Native notification service and hardware OSD for the NIRU X11 desktop.
**A personal project for the author's own machines; others are welcome to try it.**

NIRUX's `auto` provider starts nirunotify when installed and uses Dunst as fallback.
Select `native`, `dunst` or `external` explicitly in the WM session configuration.

- Implements `org.freedesktop.Notifications` with replacement IDs, timeouts,
  close reasons, urgency accents, clickable actions/`ActionInvoked`, bounded text
  and memory-only history.
- Native surfaces that follow the active NIRUWM theme, including live
  `niructl theme` and `niructl reload` changes to existing notifications and OSD.
  The urgency strip uses the theme accent, muted and error colours.
- Selects the pointer monitor, RandR primary or a named output; per-theme
  position/width, a configurable critical timeout/DND policy, and a gap below
  the bar derived from its live strut (re-read when the bar maps after the
  notifier).
- Suppresses and queues content while `_NIRU_LOCK_READY` names a live lock window.
- DND and history controls through a documented Unix socket CLI.
- Coalesced hardware OSD: `nirunotify --osd volume 45`.
- Bounded live/replacement maps, 100-entry history and a 100-command lock queue;
  `CloseNotification` emits exactly one close reason.
- No markup or image support; `body` and `actions` are the advertised capabilities.

## Install

Use [NIRUX](https://github.com/nirucon/nirux#install) for the complete desktop on
**Void glibc, Debian and Arch Linux**, or install the x86_64/glibc release separately:

```sh
git clone https://github.com/nirucon/nirunotify
cd nirunotify
./install.sh
./install.sh --build
```

Requires a session D-Bus and X11. Settings:
`~/.config/nirunotify/config.toml` (see `config.example.toml`). Colours are
not duplicated here: surfaces follow `theme` in `~/.config/niruwm/config.toml`
and reuse the shared appearance font, margin and spacing. `--check-config`
prints the effective theme and palette.

## Use

```sh
nirunotify                 # run the service (one bus owner)
notify-send "Hello" "Body" # ordinary client
nirunotify --dnd on|off|toggle|status
nirunotify --history
nirunotify --count
nirunotify --dismiss ID
nirunotify --clear
nirunotify --osd volume 45
nirunotify --check-config
```

The service exits with a clear error if another notification daemon owns
`org.freedesktop.Notifications`; it never evicts an existing provider.

Design contract: [DESIGN.md](DESIGN.md) · Roadmap:
[NIRUX roadmap](https://github.com/nirucon/nirux/blob/main/ROADMAP.md).

MIT. Copyright 2026 Nicklas Rudolfsson.
