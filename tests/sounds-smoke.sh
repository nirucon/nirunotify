#!/bin/sh
# Feedback, power and device sounds through a real daemon (D-Bus + Xvfb).
# A fake pw-play records what would play; a fake sysfs tree and injected
# uevents stand in for the charger, the battery, USB and monitors.
set -eu
BIN=${1:?usage: sounds-smoke.sh BIN}
BIN=$(CDPATH='' cd -- "$(dirname "$BIN")" && pwd)/$(basename "$BIN")
SOURCE=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
T=$(mktemp -d)
XVFB_PID=""
cleanup() {
  [ -z "$XVFB_PID" ] || kill "$XVFB_PID" 2>/dev/null || :
  rm -rf "$T"
}
trap cleanup EXIT INT TERM
mkdir -p "$T/home" "$T/runtime" "$T/bin"
chmod 700 "$T/runtime"
Xvfb :97 -screen 0 1024x768x24 -nolisten tcp >"$T/xvfb.log" 2>&1 &
XVFB_PID=$!
i=0
while [ ! -S /tmp/.X11-unix/X97 ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { cat "$T/xvfb.log"; exit 1; }
  sleep 0.05
done
export T SOURCE DISPLAY=:97 HOME="$T/home" XDG_CONFIG_HOME="$T/home/.config" \
  XDG_RUNTIME_DIR="$T/runtime" NIRUX_PREFIX="$T/prefix" NIRUNOTIFY_SYSFS="$T/sys" \
  NIRUNOTIFY_TEST_UEVENTS=1 PLAYED="$T/played" MUTE_FILE="$T/muted"
cat > "$T/bin/pw-play" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$PLAYED"
EOF
cat > "$T/bin/wpctl" <<'EOF'
#!/bin/sh
if [ -f "$MUTE_FILE" ]; then echo 'Volume: 0.40 [MUTED]'; else echo 'Volume: 0.40'; fi
EOF
chmod +x "$T/bin/pw-play" "$T/bin/wpctl"
export PATH="$T/bin:$PATH"

cat > "$T/body.sh" <<'BODY'
#!/bin/sh
set -eu
BIN=$1
SOUNDS=$NIRUX_PREFIX/share/nirux/sounds
mkdir -p "$SOUNDS/levels" "$XDG_CONFIG_HOME/nirux/sounds"
for e in volume-up volume-down volume-limit mute unmute power-plug power-unplug battery-low \
  battery-critical device-connect device-disconnect screenshot auth-fail brightness-up; do
  printf 'OggS\n' > "$SOUNDS/$e.ogg"
done
printf 'OggS\n' > "$SOUNDS/levels/volume-up-050.ogg"
cp "$SOURCE/fixtures/sounds-default.toml" "$XDG_CONFIG_HOME/nirux/sounds.toml"
put() { mkdir -p "$(dirname "$NIRUNOTIFY_SYSFS/$1")"; printf '%s\n' "$2" > "$NIRUNOTIFY_SYSFS/$1"; }
put class/power_supply/AC/type Mains
put class/power_supply/AC/online 1
put class/power_supply/BAT0/type Battery
put class/power_supply/BAT0/capacity 60
put class/power_supply/BAT0/status Charging
put class/drm/card0-DP-1/status disconnected

fail() { echo "FAIL $*"; cat "$T/daemon.log" 2>/dev/null; exit 1; }
# Wait until COUNT lines are recorded, then compare the newest with WANT.
expect() {
  want=$1 label=$2 count=${3:-1}
  i=0
  while [ "$(wc -l < "$PLAYED" 2>/dev/null || echo 0)" -lt "$count" ]; do
    i=$((i + 1)); [ "$i" -gt 60 ] && fail "$label: nothing played"
    sleep 0.05
  done
  sleep 0.2
  got=$(cat "$PLAYED"); : > "$PLAYED"
  [ "$(printf '%s\n' "$got" | wc -l)" -eq "$count" ] || fail "$label: expected $count, played: $got"
  case "$got" in *"$want") : ;; *) fail "$label: expected *$want, played: $got" ;; esac
}
silent() {
  sleep "${2:-0.5}"
  [ ! -s "$PLAYED" ] || fail "$1: expected silence, played $(cat "$PLAYED")"
}
uevent() { "$BIN" --test-uevent "$@" >/dev/null; }

: > "$PLAYED"
"$BIN" >"$T/daemon.log" 2>&1 &
DAEMON=$!
trap 'kill "$DAEMON" 2>/dev/null || :' EXIT INT TERM
i=0
until "$BIN" --dnd status >/dev/null 2>&1; do
  i=$((i + 1)); [ "$i" -gt 100 ] && fail 'daemon did not start'
  sleep 0.05
done

# Volume ticks: the level picks the tick, a held key is rate-limited, the
# OSD and the sound come from one call.
"$BIN" --sound volume-up 48 unmuted >/dev/null
expect "--volume=0.35 $SOUNDS/levels/volume-up-050.ogg" 'volume tick at 48 %'
for n in 1 2 3 4 5 6; do "$BIN" --sound volume-down 40 unmuted >/dev/null; done
expect "$SOUNDS/volume-down.ogg" 'held key' 1
sleep 0.1
"$BIN" --osd volume 45% --sound volume-limit 100 unmuted >/dev/null
expect "$SOUNDS/volume-limit.ogg" 'osd with sound'
if "$BIN" --sound nosuch >/dev/null 2>&1; then fail 'unknown event accepted'; fi
"$BIN" --sound brightness-up 50 >/dev/null
silent 'brightness is off by default'
"$BIN" --sound auth-fail >/dev/null
expect "$SOUNDS/auth-fail.ogg" 'auth-fail'

# Do Not Disturb silences notifications only.
"$BIN" --dnd on >/dev/null
"$BIN" --sound screenshot >/dev/null
expect "$SOUNDS/screenshot.ogg" 'screenshot during DND'

# Charger: one sound per change.
put class/power_supply/AC/online 0
put class/power_supply/BAT0/status Discharging
uevent ACTION=change SUBSYSTEM=power_supply DEVPATH=/devices/AC
expect "$SOUNDS/power-unplug.ogg" 'unplug during DND'
"$BIN" --dnd off >/dev/null
uevent ACTION=change SUBSYSTEM=power_supply DEVPATH=/devices/BAT0
silent 'no change, no sound'

# Battery: low once, then critical once.
put class/power_supply/BAT0/capacity 20
uevent ACTION=change SUBSYSTEM=power_supply DEVPATH=/devices/BAT0
expect "$SOUNDS/battery-low.ogg" 'battery low'
put class/power_supply/BAT0/capacity 19
uevent ACTION=change SUBSYSTEM=power_supply DEVPATH=/devices/BAT0
silent 'battery low only once'
put class/power_supply/BAT0/capacity 5
uevent ACTION=change SUBSYSTEM=power_supply DEVPATH=/devices/BAT0
expect "$SOUNDS/battery-critical.ogg" 'battery critical'

# A dock: charger, a hub with devices, an audio card and a monitor at once.
put class/power_supply/AC/online 1
put class/power_supply/BAT0/status Charging
put class/drm/card0-DP-1/status connected
uevent ACTION=add SUBSYSTEM=usb DEVTYPE=usb_device DEVPATH=/d/usb3/3-1
uevent ACTION=add SUBSYSTEM=usb DEVTYPE=usb_device DEVPATH=/d/usb3/3-1/3-1.1
uevent ACTION=add SUBSYSTEM=sound DEVPATH=/d/usb3/3-1/3-1.1/3-1.1:1.0/sound/card2
uevent ACTION=change SUBSYSTEM=power_supply DEVPATH=/devices/AC
uevent ACTION=change SUBSYSTEM=drm DEVPATH=/d/drm/card0
uevent ACTION=add SUBSYSTEM=usb DEVTYPE=usb_device DEVPATH=/d/usb3/3-1/3-1.2
expect "$SOUNDS/power-plug.ogg" 'dock is one sound and the charger wins'
silent 'dock stragglers' 1.0

# One USB stick after the debounce, then pulled.
sleep 1.2
uevent ACTION=add SUBSYSTEM=usb DEVTYPE=usb_device DEVPATH=/d/usb1/1-4
expect "$SOUNDS/device-connect.ogg" 'usb stick'
uevent ACTION=remove SUBSYSTEM=usb DEVTYPE=usb_device DEVPATH=/d/usb1/1-4
expect "$SOUNDS/device-disconnect.ogg" 'usb stick removed'

# A muted sink silences everything but what the caller marks unmuted.
touch "$MUTE_FILE"
sleep 1.6
uevent ACTION=add SUBSYSTEM=usb DEVTYPE=usb_device DEVPATH=/d/usb1/1-5
silent 'muted sink' 0.8
"$BIN" --sound unmute unmuted >/dev/null
expect "$SOUNDS/unmute.ogg" 'unmute confirmation'
rm -f "$MUTE_FILE"

# A user's file replaces any sound; config changes apply without a restart.
printf 'OggS mine\n' > "$XDG_CONFIG_HOME/nirux/sounds/auth-fail.ogg"
"$BIN" --sound auth-fail >/dev/null
expect "$XDG_CONFIG_HOME/nirux/sounds/auth-fail.ogg" 'user override'
sed 's/^auth-fail = true/auth-fail = false/' "$XDG_CONFIG_HOME/nirux/sounds.toml" > "$T/c"
cat "$T/c" > "$XDG_CONFIG_HOME/nirux/sounds.toml"
"$BIN" --sound auth-fail >/dev/null
silent 'auth-fail switched off'

# Idle: the sound and uevent threads sleep; no CPU while nothing happens.
sleep 1
t0=$(awk '{print $14 + $15}' "/proc/$DAEMON/stat")
sleep 3
t1=$(awk '{print $14 + $15}' "/proc/$DAEMON/stat")
[ $((t1 - t0)) -le 2 ] || fail "idle daemon used $((t1 - t0)) ticks in 3 s"
for task in /proc/"$DAEMON"/task/*; do
  case "$(cat "$task/comm")" in
    nirunotify-soun|nirunotify-ueve) [ "$(awk '{print $3}' "$task/stat")" = S ] || fail "$(cat "$task/comm") is not sleeping" ;;
  esac
done
echo "PASS sounds-smoke"
BODY
chmod +x "$T/body.sh"
dbus-run-session -- "$T/body.sh" "$BIN"
