#!/bin/sh
# NIRUNOTIFY standalone installer.
#
#   ./install.sh             install the released binary (downloads it)
#   ./install.sh --build     build from this checkout and install the result
#   ./install.sh --uninstall remove the binary; configuration is kept
set -eu

VERSION="0.1.0"
REPO="${NIRUNOTIFY_REPO:-nirucon/nirunotify}"
PREFIX="${NIRUNOTIFY_PREFIX:-$HOME/.local}"
BINDIR="$PREFIX/bin"
CONFDIR="${XDG_CONFIG_HOME:-$HOME/.config}/nirunotify"

say() { printf '%s\n' "$*"; }
die() { say "nirunotify install: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1; }
sha256() {
  if need sha256sum; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1
  fi
}
fetch() {
  if need curl; then curl -fsSL --retry 3 -o "$2" "$1"
  elif need wget; then wget -qO "$2" "$1"
  else die "curl or wget is required"
  fi
}
install_config() {
  mkdir -p "$CONFDIR"
  if [ ! -e "$CONFDIR/config.toml" ]; then
    install -m 644 "$(dirname "$0")/config.example.toml" "$CONFDIR/config.toml"
    say "Created $CONFDIR/config.toml"
  else
    say "Keeping existing $CONFDIR/config.toml"
  fi
}

case "${1:-}" in
  --build)
    need cargo || die "cargo is required for --build"
    ( cd "$(dirname "$0")" && cargo build --release --locked )
    install -d "$BINDIR"
    install -m 755 "$(dirname "$0")/target/release/nirunotify" "$BINDIR/nirunotify"
    install_config
    say "Installed $("$BINDIR/nirunotify" --version) in $BINDIR"
    ;;
  --uninstall)
    rm -f "$BINDIR/nirunotify"
    say "Removed $BINDIR/nirunotify. Configuration in $CONFDIR was kept."
    ;;
  "")
    tag="${NIRUNOTIFY_VERSION:-$VERSION}"
    base="${NIRUNOTIFY_BASE_URL:-https://github.com/$REPO/releases/download/v$tag}"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    say "Downloading NIRUNOTIFY $tag for x86_64 ..."
    fetch "$base/nirunotify" "$tmp/nirunotify" || die "cannot download the binary"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "cannot download checksums"
    expected=$(awk '$2 == "nirunotify" {print $1}' "$tmp/SHA256SUMS")
    [ "${#expected}" = 64 ] || die "no checksum for nirunotify"
    [ "$expected" = "$(sha256 "$tmp/nirunotify")" ] || die "checksum mismatch"
    chmod 755 "$tmp/nirunotify"
    got=$("$tmp/nirunotify" --version | awk '{print $2}')
    [ "$got" = "$tag" ] || die "expected NIRUNOTIFY $tag, got $got"
    install -d "$BINDIR"
    install -m 755 "$tmp/nirunotify" "$BINDIR/nirunotify"
    install_config
    say "Installed NIRUNOTIFY $got in $BINDIR"
    ;;
  *)
    die "unknown option: $1"
    ;;
esac
