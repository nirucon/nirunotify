# Implementation contract — M4

Design only; no implementation is present. Follow nirux/ROADMAP.md. Do not
replace Dunst or change installer defaults merely because this repository exists.

## Protocol and ownership

Rust with native X11 windows and a maintained D-Bus library. Implement
org.freedesktop.Notifications: GetCapabilities, GetServerInformation, Notify,
CloseNotification, NotificationClosed, ActionInvoked. A service which cannot own
the well-known bus name exits with a clear error; never evict an existing owner.

Unique nonzero IDs, replacement of existing IDs, expiry -1 (server default),
0 (persistent), and positive milliseconds need explicit unit tests. Report
correct close reasons for expiry, dismissal and client close. Advertise only
implemented capabilities; do not claim markup/images/actions until supported.
Treat untrusted notification text and action names as data, not shell commands.

## Rendering and resources

Configurable NIRU palette, font, width, padding, row limits, margins, placement,
monitor selection, urgency policy and timeouts. Clamp the stack to the selected
monitor's work area. Support notification updates and monitor disappearance
without leaks. No keyboard grab for ordinary notifications; explicit click
actions, clear dismiss behaviour, and no steal-focus window policy.

Use event/deadline-driven waits. Bound string/image sizes, pending queue, visible
notifications, history and resource lifetime. Fairness under a notification flood
must not starve X/D-Bus processing. Memory-only history by default. If persistence
is later added: opt-in, bounded retention, atomic writes and private permissions.

DND and history controls must use a documented client interface rather than
spawning a second bus owner. Proposed CLI: daemon default, --version,
--check-config, and documented control subcommands implemented after the server.
Config belongs in XDG_CONFIG_HOME/nirunotify/config.toml; validation works offline.

## Session and lock integration

Add a notification_provider setting to niruwm's session config. During migration,
keep Dunst fallback and respect an existing external provider. Start exactly one
provider, track its child and clean it up on logout. Make failure visible in
nirux doctor. Do not race two providers for bus ownership.

While locked, suppress private content and keep notification surfaces below the
locker. Define how pending notifications resume after authenticated unlock.
Do not rely only on window stacking to protect notification content. Coordinate
a tested lock-state contract with nirulock/niruwm before enabling this integration.

## Release gates

- Private D-Bus tests for every method, signal, ID and timeout case.
- Xvfb tests for expiry, replacement, action clicks, placement and dismissal.
- Flood/resource limits, DND/history bounds, malformed hints and Unicode.
- Lock-state privacy and exactly-one-provider session tests.
- Real notify-send interoperability on Arch, Debian and Void glibc.
- Release binary + SHA256SUMS; only then update nirux manifest/staging,
  status/doctor/rollback/uninstall and every relevant regression fixture.
