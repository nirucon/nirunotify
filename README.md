# NIRUNOTIFY

A planned native notification service styled for the NIRU X11 desktop.
**A personal project for the author's own machines; others are welcome to try it.**

Status: design checkpoint, not yet an installable program or Dunst replacement.
No binary release is published; nirux retains Dunst until acceptance tests pass.

The intended service implements `org.freedesktop.Notifications`, with configurable
placement, appearance, expiry, DND and bounded history. Planned config location:
`~/.config/nirunotify/config.toml`.

Implementation contract: [DESIGN.md](DESIGN.md).
Authoritative tasks, dependencies and release gates:
[NIRU roadmap, R6](https://github.com/nirucon/nirux/blob/main/ROADMAP.md).
