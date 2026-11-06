#!/bin/sh
# Real D-Bus + X11 notification smoke test.
set -eu
BIN=${1:?usage: ui-smoke.sh BIN}
BIN=$(CDPATH='' cd -- "$(dirname "$BIN")" && pwd)/$(basename "$BIN")
T=$(mktemp -d)
XVFB_PID=""
cleanup() {
  [ -z "$XVFB_PID" ] || kill "$XVFB_PID" 2>/dev/null || :
  rm -rf "$T"
}
trap cleanup EXIT INT TERM
mkdir -p "$T/home" "$T/runtime"
chmod 700 "$T/runtime"
export T
Xvfb :98 -screen 0 1280x800x24 -nolisten tcp >"$T/xvfb.log" 2>&1 &
XVFB_PID=$!
i=0
while [ ! -S /tmp/.X11-unix/X98 ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { cat "$T/xvfb.log"; exit 1; }
  sleep 0.05
done
export DISPLAY=:98 HOME="$T/home" XDG_CONFIG_HOME="$T/home/.config" \
  XDG_RUNTIME_DIR="$T/runtime" XDG_CACHE_HOME="$T/home/.cache"
: > "$T/body.sh"
cat > "$T/body.sh" <<'BODY'
#!/bin/sh
set -eu
BIN="$1"
mkdir -p "$XDG_CONFIG_HOME/nirunotify" "$XDG_CONFIG_HOME/niruwm"
printf 'width = 360\n' > "$XDG_CONFIG_HOME/nirunotify/config.toml"
printf 'theme = "niru-noir"\n' > "$XDG_CONFIG_HOME/niruwm/config.toml"
count() {
  xdotool search --class NIRUNOTIFY 2>/dev/null | wc -l | tr -d ' '
}
# `notify-send --wait APP TIMEOUT SUMMARY`: send, then return once the
# server emits NotificationClosed for this notification. notify-send 0.8.8 on
# the Ubuntu 26.04 runners never returns from --wait, so gdbus is used where
# it exists and notify-send stays the fallback.
wait_client() {
  if ! command -v gdbus >/dev/null 2>&1; then
    notify-send --wait -a "$1" -t "$2" "$3" body
    return
  fi
  log=$(mktemp "$T/closed.XXXXXX")
  dbus-monitor --session "type='signal',interface='org.freedesktop.Notifications',member='NotificationClosed'" >"$log" 2>&1 &
  mon=$!
  sleep 0.2
  id=$(gdbus call --session --dest org.freedesktop.Notifications \
    --object-path /org/freedesktop/Notifications \
    --method org.freedesktop.Notifications.Notify \
    "$1" 0 '' "$3" body '[]' '{}' "$2" | sed 's/.*uint32 \([0-9]*\).*/\1/')
  n=0
  until grep -A1 'member=NotificationClosed' "$log" | grep -q "uint32 $id\$"; do
    n=$((n + 1)); [ "$n" -gt 400 ] && break
    sleep 0.05
  done
  kill "$mon" 2>/dev/null || :
}
"$BIN" >"$T/daemon.log" 2>&1 &
DAEMON=$!
lock_cleanup() { kill "$DAEMON" 2>/dev/null || :; }
trap lock_cleanup EXIT INT TERM
sleep 1
notify-send -a TestApp -u critical -t 6000 "Hello summary" "Body line one"
i=0
while [ "$(count)" -lt 1 ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { cat "$T/daemon.log"; echo "FAIL no notification window"; exit 1; }
  sleep 0.05
done
ID=$("$BIN" --history | awk 'NR == 1 {print $1}')
[ -n "$ID" ] || { echo "FAIL no history id"; exit 1; }
# Rendered theme values: critical strip is the theme error colour and the
# popup body is the derived surface colour.
WIN=$(xdotool search --class NIRUNOTIFY | head -1)
xwd -id "$WIN" -silent > "$T/notify.xwd"
colours=$(convert "$T/notify.xwd" -format %c histogram:info:- 2>/dev/null)
printf '%s\n' "$colours" | grep -qi '#d77a7a' || {
  echo "FAIL theme error strip not rendered"
  printf '%s\n' "$colours" | head -20
  exit 1
}
printf '%s\n' "$colours" | grep -qi '#202021' || {
  echo "FAIL theme surface not rendered"
  printf '%s\n' "$colours" | head -20
  exit 1
}
# Live theme/reload: niructl bumps _NIRU_CONFIG_SERIAL. The running service
# must rebuild existing surfaces with the new theme and width, not just the
# next notification.
mkdir -p "$XDG_CONFIG_HOME/nirunotify" "$XDG_CONFIG_HOME/niruwm"
printf 'width = 500\n' > "$XDG_CONFIG_HOME/nirunotify/config.toml"
printf 'theme = "niru-nord"\n' > "$XDG_CONFIG_HOME/niruwm/config.toml"
xprop -root -f _NIRU_CONFIG_SERIAL 32c -set _NIRU_CONFIG_SERIAL 42
i=0
while :; do
  WIN=$(xdotool search --class NIRUNOTIFY 2>/dev/null | head -1 || true)
  if [ -n "$WIN" ]; then
    WIDTH=$(xdotool getwindowgeometry --shell "$WIN" 2>/dev/null | sed -n 's/^WIDTH=//p' || true)
    [ "$WIDTH" = 500 ] && break
  fi
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL live theme reload did not resize"; exit 1; }
  sleep 0.05
done
# Stacking: a later window that covers the popup must not stay above it.
xmessage -title stack-test -buttons OK -center stack-test >/dev/null 2>&1 &
STACK_PID=$!
i=0
while [ -z "$(xdotool search --name '^stack-test$' 2>/dev/null | head -1 || true)" ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL stack-test window"; exit 1; }
  sleep 0.05
done
STACK_WIN=$(xdotool search --name '^stack-test$' | head -1)
NOTIFY_WIN=$(xdotool search --class NIRUNOTIFY | head -1)
eval "$(xdotool getwindowgeometry --shell "$NOTIFY_WIN" | sed -n 's/^\([XY]=\)/\1/p')"
xdotool windowmove "$STACK_WIN" "$X" "$Y"
xdotool windowraise "$STACK_WIN"
sleep 0.5
notify_line=$(xwininfo -root -tree | grep -n 'NIRUNOTIFY' | head -1 | cut -d: -f1)
stack_line=$(xwininfo -root -tree | grep -n '"stack-test"' | head -1 | cut -d: -f1)
[ -n "$notify_line" ] && [ -n "$stack_line" ] && [ "$notify_line" -lt "$stack_line" ] || {
  echo "FAIL notification stayed below a covering window"
  exit 1
}
kill "$STACK_PID" 2>/dev/null || :
notify-send -a TestApp -r "$ID" -t 6000 "Replaced summary" "Body two"
sleep 0.3
[ "$(count)" -eq 1 ] || { echo "FAIL replacement created a second window"; exit 1; }
"$BIN" --dnd on >/dev/null
notify-send -a Quiet -t 6000 "Quiet summary" "Body three"
sleep 0.3
"$BIN" --history | grep -q 'Quiet summary' || { echo "FAIL DND did not keep history"; exit 1; }
"$BIN" --history | grep -q 'Replaced summary' || { echo "FAIL replacement lost history"; exit 1; }
"$BIN" --dnd off >/dev/null
notify-send -a Short -t 400 "Short lived" "gone soon"
i=0
while [ "$(count)" -ge 2 ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL short notification did not expire"; exit 1; }
  sleep 0.05
done
"$BIN" --history | awk '{print $1}' | while read -r id; do
  [ -n "$id" ] && "$BIN" --dismiss "$id" >/dev/null || :
done
i=0
while [ "$(count)" -ne 0 ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL notifications did not dismiss"; exit 1; }
  sleep 0.05
done
"$BIN" --osd volume 45 >/dev/null
i=0
while [ "$(count)" -lt 1 ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL OSD did not map"; exit 1; }
  sleep 0.05
done
i=0
while [ "$(count)" -ne 0 ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL OSD did not expire"; exit 1; }
  sleep 0.05
done
# ActionInvoked: a body click on a notification with a default action invokes it.
dbus-monitor --session "type='signal',interface='org.freedesktop.Notifications',member='ActionInvoked'" >"$T/action.log" 2>&1 &
MON=$!
sleep 0.2
# gdbus sends the actions as given. notify-send 0.8.8 on the Ubuntu 26.04
# runners decides that no server supports actions (GetCapabilities does list
# them) and then waits forever, so it is only the fallback here.
CLIENT=
if command -v gdbus >/dev/null 2>&1; then
  gdbus call --session --dest org.freedesktop.Notifications \
    --object-path /org/freedesktop/Notifications \
    --method org.freedesktop.Notifications.Notify \
    Action 0 '' 'Action test' body "['default', 'Open']" '{}' 6000 >/dev/null
else
  notify-send --wait -a Action -A default=Open -t 6000 "Action test" "body" &
  CLIENT=$!
fi
i=0
while [ "$(count)" -lt 1 ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL action notification did not map"; exit 1; }
  sleep 0.05
done
WIN=$(xdotool search --class NIRUNOTIFY | head -1)
xdotool mousemove --window "$WIN" 10 10 click 1
i=0
while [ "$(count)" -ne 0 ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL action did not close notification"; exit 1; }
  sleep 0.05
done
i=0
while [ -n "$CLIENT" ] && kill -0 "$CLIENT" 2>/dev/null; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL action client did not finish"; exit 1; }
  sleep 0.05
done
i=0
until grep -q 'ActionInvoked' "$T/action.log"; do
  i=$((i + 1)); [ "$i" -gt 100 ] && break
  sleep 0.05
done
kill "$MON" 2>/dev/null || :
grep -q 'ActionInvoked' "$T/action.log" || { cat "$T/action.log"; echo "FAIL ActionInvoked signal"; exit 1; }
grep -q 'default' "$T/action.log" || { cat "$T/action.log"; echo "FAIL action key"; exit 1; }
# DND must still expire notifications, otherwise a --wait client hangs forever
# waiting for a NotificationClosed that no popup will ever produce.
"$BIN" --dnd on >/dev/null
wait_client QuietWait 500 "Quiet wait" &
QCLIENT=$!
i=0
while kill -0 "$QCLIENT" 2>/dev/null; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL DND notification never closed"; exit 1; }
  sleep 0.05
done
"$BIN" --dnd off >/dev/null

# A notification displaced by max_visible must also close its client.
printf 'width = 500\nmax_visible = 1\n' > "$XDG_CONFIG_HOME/nirunotify/config.toml"
xprop -root -f _NIRU_CONFIG_SERIAL 32c -set _NIRU_CONFIG_SERIAL 43
sleep 0.3
wait_client First 6000 "First visible" &
FIRST=$!
i=0
while [ "$(count)" -lt 1 ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL first visible notification"; exit 1; }
  sleep 0.05
done
wait_client Second 800 "Second visible" &
SECOND=$!
i=0
while kill -0 "$FIRST" 2>/dev/null; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL displaced notification client never closed"; exit 1; }
  sleep 0.05
done
i=0
while kill -0 "$SECOND" 2>/dev/null; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL second notification client never closed"; exit 1; }
  sleep 0.05
done

# Lock privacy: publish a viewable lock-ready window, send, expect suppression.
xmessage -title lock-test -buttons OK -center lock-test >/dev/null 2>&1 &
XMESSAGE=$!
sleep 0.3
LOCKWIN=""
i=0
while [ -z "$LOCKWIN" ]; do
  LOCKWIN=$(xdotool search --name lock-test 2>/dev/null | head -1 || true)
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL xmessage test window"; exit 1; }
  sleep 0.05
done
xprop -root -f _NIRU_LOCK_READY 32x -set _NIRU_LOCK_READY "$LOCKWIN"
notify-send -a Private -t 6000 "Private summary" "must not display"
sleep 0.3
[ "$(count)" -eq 0 ] || { echo "FAIL notification appeared above the lock"; exit 1; }
xprop -root -remove _NIRU_LOCK_READY
sleep 0.5
kill "$XMESSAGE" 2>/dev/null || :
echo "PASS ui-smoke"
BODY
chmod +x "$T/body.sh"
dbus-run-session -- "$T/body.sh" "$BIN"
