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
count() {
  xdotool search --class NIRUNOTIFY 2>/dev/null | wc -l | tr -d ' '
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
