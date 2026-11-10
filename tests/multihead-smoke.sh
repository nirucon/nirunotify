#!/bin/sh
# Two stacked screens: the OSD and notifications follow the NIRUWM focused
# screen, stay clear of that screen's bar, and the OSD is reused while a key
# repeats instead of being recreated.
set -eu
BIN=${1:?usage: multihead-smoke.sh BIN}
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
Xvfb :97 -screen 0 1280x1600x24 -nolisten tcp -noreset >"$T/xvfb.log" 2>&1 &
XVFB_PID=$!
i=0
while [ ! -S /tmp/.X11-unix/X97 ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { cat "$T/xvfb.log"; exit 1; }
  sleep 0.05
done
export DISPLAY=:97 HOME="$T/home" XDG_CONFIG_HOME="$T/home/.config" \
  XDG_RUNTIME_DIR="$T/runtime" XDG_CACHE_HOME="$T/home/.cache"
cat > "$T/body.sh" <<'BODY'
#!/bin/sh
set -eu
BIN="$1"
fail() { echo "FAIL $*"; cat "$T/daemon.log" 2>/dev/null || :; exit 1; }
mkdir -p "$XDG_CONFIG_HOME/nirunotify" "$XDG_CONFIG_HOME/niruwm"
printf 'width = 360\n' > "$XDG_CONFIG_HOME/nirunotify/config.toml"
printf 'theme = "niru-noir"\n' > "$XDG_CONFIG_HOME/niruwm/config.toml"
# Upper screen TOP, lower screen BOT, as NIRUWM would publish them.
xrandr --setmonitor TOP 1280/338x800/211+0+0 none >/dev/null 2>&1 || fail "setmonitor TOP"
xrandr --setmonitor BOT 1280/338x800/211+0+800 none >/dev/null 2>&1 || fail "setmonitor BOT"
xprop -root -f _NIRUWM_MONITORS 8u -set _NIRUWM_MONITORS \
  "$(printf 'TOP\t0\t0\t1280\t0\t10\t0\ttile\nBOT\t0\t800\t1280\t10\t10\t10\ttile\n')"
focus() { xprop -root -f _NET_CURRENT_DESKTOP 32c -set _NET_CURRENT_DESKTOP "$1"; }
# A 30 px "bar" at the top of the lower screen with a strut.
xmessage -title fakebar -geometry 1280x30+0+800 bar >/dev/null 2>&1 &
BAR_PID=$!
i=0
while [ -z "$(xdotool search --name '^fakebar$' 2>/dev/null | head -1 || true)" ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && fail "fake bar"
  sleep 0.05
done
BAR=$(xdotool search --name '^fakebar$' | head -1)
xdotool windowsize "$BAR" 1280 30
xdotool windowmove "$BAR" 0 800
xprop -id "$BAR" -f _NET_WM_STRUT_PARTIAL 32c -set _NET_WM_STRUT_PARTIAL 0,0,30,0,0,0,0,0,0,1279,0,0
"$BIN" >"$T/daemon.log" 2>&1 &
DAEMON=$!
trap 'kill "$DAEMON" "$BAR_PID" 2>/dev/null || :' EXIT INT TERM
sleep 1
wins() { xdotool search --class NIRUNOTIFY 2>/dev/null || true; }
geom() { xdotool getwindowgeometry --shell "$1" | sed -n "s/^$2=//p"; }
wait_one() {
  i=0
  while [ -z "$(wins | head -1)" ]; do
    i=$((i + 1)); [ "$i" -gt 100 ] && fail "$1 did not map"
    sleep 0.05
  done
  wins | head -1
}
wait_none() {
  i=0
  while [ -n "$(wins | head -1)" ]; do
    i=$((i + 1)); [ "$i" -gt 120 ] && fail "$1 did not go away"
    sleep 0.05
  done
}

# 1. Focus on the lower screen, pointer on the upper one: the OSD follows focus.
focus 12
xdotool mousemove 600 200
"$BIN" --osd volume 40% >/dev/null
W=$(wait_one "OSD")
Y=$(geom "$W" Y); H=$(geom "$W" HEIGHT); X=$(geom "$W" X); WD=$(geom "$W" WIDTH)
[ "$Y" -ge 830 ] && [ $((Y + H)) -le 1600 ] || fail "OSD not on the focused lower screen (y=$Y h=$H)"
[ $((X + WD / 2)) -ge 630 ] && [ $((X + WD / 2)) -le 650 ] || fail "OSD not centred (x=$X w=$WD)"
[ "$Y" -gt 1200 ] || fail "OSD should sit in the lower part of its screen (y=$Y)"
# The bar is drawn in the readable accent; the box is not text-only.
xwd -id "$W" -silent > "$T/osd.xwd"
COLOURS=$(convert "$T/osd.xwd" -format %c histogram:info:- 2>/dev/null | wc -l)
[ "$COLOURS" -ge 4 ] || fail "OSD looks empty ($COLOURS colours)"
# 2. Repeats reuse the window (no flicker) and Muted keeps the box.
"$BIN" --osd volume 45% >/dev/null
"$BIN" --osd volume Muted >/dev/null
sleep 0.1
[ "$(wins | head -1)" = "$W" ] || fail "OSD was recreated on a repeat"
[ "$(wins | wc -l | tr -d ' ')" -eq 1 ] || fail "more than one OSD window"
# A different channel replaces it.
"$BIN" --osd brightness 70% >/dev/null
sleep 0.1
[ "$(wins | wc -l | tr -d ' ')" -eq 1 ] || fail "two OSD channels at once"
wait_none "OSD"

# 3. Focus moves to the upper screen: the next OSD goes there.
focus 3
xdotool mousemove 600 1200
"$BIN" --osd brightness 70% >/dev/null
W=$(wait_one "OSD on upper screen")
Y=$(geom "$W" Y); H=$(geom "$W" HEIGHT)
[ $((Y + H)) -le 800 ] || fail "OSD not on the focused upper screen (y=$Y)"
wait_none "upper OSD"

# 4. Notifications: on the focused screen, below that screen's bar only.
focus 12
notify-send -a Test -t 4000 "Lower" "body"
W=$(wait_one "notification")
Y=$(geom "$W" Y)
[ "$Y" -ge 830 ] && [ "$Y" -lt 900 ] || fail "notification not below the lower screen's bar (y=$Y)"
"$BIN" --history | awk '{print $1}' | while read -r id; do "$BIN" --dismiss "$id" >/dev/null || :; done
wait_none "notification"
focus 3
notify-send -a Test -t 4000 "Upper" "body"
W=$(wait_one "upper notification")
Y=$(geom "$W" Y)
[ "$Y" -lt 60 ] || fail "upper notification took the lower bar's strut (y=$Y)"

# 5. pointer mode still works.
printf 'width = 360\nmonitor = "pointer"\n' > "$XDG_CONFIG_HOME/nirunotify/config.toml"
xprop -root -f _NIRU_CONFIG_SERIAL 32c -set _NIRU_CONFIG_SERIAL 7
sleep 0.3
"$BIN" --history | awk '{print $1}' | while read -r id; do "$BIN" --dismiss "$id" >/dev/null || :; done
wait_none "notification before pointer mode"
xdotool mousemove 600 1200
"$BIN" --osd volume 20% >/dev/null
W=$(wait_one "pointer OSD")
Y=$(geom "$W" Y)
[ "$Y" -ge 800 ] || fail "pointer mode ignored the pointer (y=$Y)"
echo "PASS multihead-smoke"
BODY
chmod +x "$T/body.sh"
dbus-run-session -- "$T/body.sh" "$BIN"
