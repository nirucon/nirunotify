#!/bin/sh
# "While you were away": after a lock of at least away_after_minutes, what
# came meanwhile is one card; an urgent notification still shows on its own.
# Real D-Bus and X11 in a private Xvfb. Takes a little over a minute.
#
#   tests/away-smoke.sh BIN [SCREENSHOT.png]
set -eu
BIN=${1:?usage: away-smoke.sh BIN [SCREENSHOT]}
BIN=$(CDPATH='' cd -- "$(dirname "$BIN")" && pwd)/$(basename "$BIN")
SHOT=${2:-}
T=$(mktemp -d)
XVFB_PID=""
cleanup() {
  [ -z "$XVFB_PID" ] || kill "$XVFB_PID" 2>/dev/null || :
  rm -rf "$T"
}
trap cleanup EXIT INT TERM
mkdir -p "$T/home/Downloads" "$T/runtime"
chmod 700 "$T/runtime"
export T SHOT
Xvfb :97 -screen 0 1280x800x24 -nolisten tcp >"$T/xvfb.log" 2>&1 &
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
mkdir -p "$XDG_CONFIG_HOME/nirunotify" "$XDG_CONFIG_HOME/niruwm"
printf 'away_after_minutes = 1\nmax_visible = 5\n' > "$XDG_CONFIG_HOME/nirunotify/config.toml"
printf 'theme = "niru-noir"\n' > "$XDG_CONFIG_HOME/niruwm/config.toml"
count() { xdotool search --class NIRUNOTIFY 2>/dev/null | wc -l | tr -d ' '; }
"$BIN" >"$T/daemon.log" 2>&1 &
DAEMON=$!
trap 'kill "$DAEMON" "${XMESSAGE:-}" 2>/dev/null || :' EXIT INT TERM
sleep 1
xmessage -name lock-test lock-test >/dev/null 2>&1 &
XMESSAGE=$!
i=0
until LOCKWIN=$(xdotool search --name lock-test 2>/dev/null | head -1) && [ -n "$LOCKWIN" ]; do
  i=$((i + 1)); [ "$i" -gt 100 ] && { echo "FAIL xmessage test window"; exit 1; }
  sleep 0.05
done
xprop -root -f _NIRU_LOCK_READY 32x -set _NIRU_LOCK_READY "$LOCKWIN"
sleep 0.5
notify-send -a Discord "Anna" "hej"
notify-send -a Thunderbird "Re: möte" "imorgon"
notify-send -a Discord "Bo: kommer du?" ""
notify-send -a Battery -u critical "Battery low" "5 % left"
: > "$HOME/Downloads/report.pdf"
: > "$HOME/Downloads/big.iso.part"
sleep 0.5
[ "$(count)" -eq 0 ] || { echo "FAIL notification appeared above the lock"; exit 1; }
sleep 61
xprop -root -remove _NIRU_LOCK_READY
i=0
while [ "$(count)" -lt 2 ]; do
  i=$((i + 1)); [ "$i" -gt 60 ] && break
  sleep 0.05
done
sleep 0.5
n=$(count)
[ -z "$SHOT" ] || import -window root "$SHOT"
[ "$n" -eq 2 ] || { cat "$T/daemon.log"; echo "FAIL expected the card and the urgent one, saw $n windows"; exit 1; }
[ "$("$BIN" --history | wc -l)" -eq 4 ] || { echo "FAIL history lost notifications"; exit 1; }
echo "PASS away-smoke"
BODY
chmod +x "$T/body.sh"
dbus-run-session -- "$T/body.sh" "$BIN"
