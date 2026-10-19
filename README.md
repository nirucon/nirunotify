# NIRUNOTIFY

Native notification service and hardware OSD for the NIRU X11 desktop.
**A personal project for the author's own machines; others are welcome to try it.**

Status: **0.1.1**, an early native provider. NIRUX still defaults to Dunst; select
NIRUNOTIFY explicitly after verifying it on your session.

- Implements `org.freedesktop.Notifications` with replacement IDs, timeouts,
  close reasons, urgency accents, bounded text and memory-only history.
- Native themed surfaces; ordinary notifications never take keyboard focus.
- Suppresses and queues content while `_NIRU_LOCK_READY` names a live lock window.
- DND and history controls through a documented Unix socket CLI.
- Coalesced hardware OSD: `nirunotify --osd volume 45`.
- Bounded live/replacement maps, 100-entry history and a 100-command lock queue;
  `CloseNotification` emits exactly one close reason.
- No markup or image support; `body` is the advertised capability.

## Install

```sh
./install.sh
./install.sh --build
```

Requires a session D-Bus and X11. Settings:
`~/.config/nirunotify/config.toml` (see `config.example.toml`).

## Use

```sh
nirunotify                 # run the service (one bus owner)
notify-send "Hello" "Body" # ordinary client
nirunotify --dnd on|off|toggle|status
nirunotify --history
nirunotify --dismiss ID
nirunotify --clear
nirunotify --osd volume 45
nirunotify --check-config
```

The service exits with a clear error if another notification daemon owns
`org.freedesktop.Notifications`; it never evicts an existing provider.

Design contract: [DESIGN.md](DESIGN.md) · Roadmap:
[nirux R6](https://github.com/nirucon/nirux/blob/main/ROADMAP.md).

MIT. Copyright 2026 Nicklas Rudolfsson.
