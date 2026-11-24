#!/bin/sh
# Dreamer installer for macOS and Linux. No root needed.
#
#   curl -fsSL https://github.com/probablyrobot/dreamer/releases/latest/download/install.sh | sh
#
#   sh install.sh --version 0.10.0
#   sh install.sh --from dreamer-x86_64-unknown-linux-gnu.tar.gz
#   sh install.sh --uninstall
#
# Options:
#   --version V        install release V (default: the latest release)
#   --from FILE        install this archive instead of downloading one
#                      (a .tar.gz on Linux, a .zip on macOS)
#   --prefix DIR       install under DIR (default: ~/.local)
#   --app-dir DIR      macOS: where Dreamer.app goes (default: /Applications
#                      if you can write there, else ~/Applications)
#   --enable-service   Linux: also enable the dreamerd systemd user service
#   --uninstall        remove what this script installed
#
# Environment: DREAMER_VERSION, DREAMER_PREFIX and DREAMER_APP_DIR mirror
# the options. Run it again, or `dream update`, to update.
#
# Linux layout:
#   PREFIX/share/dreamer/versions/<version>/   unpacked release
#   PREFIX/share/dreamer/current               -> the active version
#   PREFIX/bin/{dreamer,dreamerd,dream,dreamer-termapp} -> current/bin/...
#   XDG_DATA_HOME/fonts/dreamer/               JetBrains Mono Nerd Font
#   XDG_DATA_HOME/applications/dev.dreamer.Dreamer.desktop
#   XDG_DATA_HOME/icons/hicolor/*/apps/dev.dreamer.Dreamer.{png,svg}
#   XDG_CONFIG_HOME/systemd/user/dreamerd.service
#
# macOS layout:
#   APP_DIR/Dreamer.app                        the signed, notarized app
#   PREFIX/bin/{dream,dreamerd,dreamer-termapp} -> the tools inside the app
set -eu

REPO="probablyrobot/dreamer"
RELEASES="${DREAMER_RELEASES_URL:-https://github.com/$REPO/releases}"
PREFIX="${DREAMER_PREFIX:-$HOME/.local}"
VERSION="${DREAMER_VERSION:-latest}"
APP_DIR="${DREAMER_APP_DIR:-}"
FROM=""
SERVICE=0
UNINSTALL=0

say() { printf 'dreamer: %s\n' "$*"; }
die() { printf 'dreamer: error: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --from) FROM="${2:?--from needs a file}"; shift 2 ;;
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
    --app-dir) APP_DIR="${2:?--app-dir needs a directory}"; shift 2 ;;
    --enable-service) SERVICE=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) sed -n '2,21p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done

OS="$(uname -s)"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"
HOME_DIR="$PREFIX/share/dreamer"
DESKTOP="$DATA/applications/dev.dreamer.Dreamer.desktop"
ICONS="$DATA/icons/hicolor"
FONTS="$DATA/fonts/dreamer"
UNIT="$CONFIG/systemd/user/dreamerd.service"
MAC_TOOLS="dream dreamerd dreamer-termapp"

if [ "$UNINSTALL" = 1 ]; then
  if [ "$OS" = Darwin ]; then
    for b in $MAC_TOOLS; do
      case "$(readlink "$PREFIX/bin/$b" 2>/dev/null || true)" in
        */Dreamer.app/Contents/MacOS/*) rm -f "$PREFIX/bin/$b" ;;
      esac
    done
    # Only the directory you named, or else the two default places.
    if [ -n "$APP_DIR" ]; then set -- "$APP_DIR"; else set -- /Applications "$HOME/Applications"; fi
    for d in "$@"; do
      if [ -d "$d/Dreamer.app" ]; then rm -rf "$d/Dreamer.app"; say "removed $d/Dreamer.app"; fi
    done
  else
    if command -v systemctl >/dev/null 2>&1 && [ -f "$UNIT" ]; then
      systemctl --user disable --now dreamerd >/dev/null 2>&1 || true
    fi
    for b in dreamer dreamerd dream dreamer-termapp; do
      if [ -L "$PREFIX/bin/$b" ]; then rm -f "$PREFIX/bin/$b"; fi
    done
    rm -rf "$HOME_DIR" "$FONTS"
    rm -f "$DESKTOP" "$UNIT"
    find "$ICONS" -name 'dev.dreamer.Dreamer.*' -type f -exec rm -f {} + 2>/dev/null || true
    command -v fc-cache >/dev/null 2>&1 && fc-cache -f "$DATA/fonts" >/dev/null 2>&1 || true
  fi
  say "removed. Your settings in $CONFIG/dreamer and any tmux sessions are untouched."
  exit 0
fi

case "$OS-$(uname -m)" in
  Linux-x86_64|Linux-amd64) TRIPLE=x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64) TRIPLE=aarch64-unknown-linux-gnu ;;
  Darwin-arm64) TRIPLE=aarch64-apple-darwin ;;
  Darwin-*) die "Dreamer needs a Mac with Apple Silicon" ;;
  *) die "unsupported system $OS $(uname -m)" ;;
esac
if [ "$OS" = Darwin ]; then
  MAJOR="$(sw_vers -productVersion | cut -d. -f1)"
  [ "$MAJOR" -ge 15 ] || die "Dreamer needs macOS 15 or later (this is $(sw_vers -productVersion))"
fi

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT INT TERM

if [ -n "$FROM" ]; then
  [ -f "$FROM" ] || die "no such file: $FROM"
  ARCHIVE="$FROM"
  if [ -f "$FROM.sha256" ]; then
    [ "$(sha256 "$FROM")" = "$(cut -d' ' -f1 "$FROM.sha256")" ] || die "checksum mismatch for $FROM"
    say "checksum verified"
  fi
else
  command -v curl >/dev/null 2>&1 || die "curl is needed to download a release"
  if [ "$VERSION" = latest ]; then
    # Where releases/latest redirects to names the version.
    LANDED="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$RELEASES/latest")" \
      || die "could not reach $RELEASES/latest"
    case "$LANDED" in
      */tag/v*) VERSION="${LANDED##*/tag/v}" ;;
      *) die "no published release found at $RELEASES" ;;
    esac
  fi
  VERSION="${VERSION#v}"
  if [ "$OS" = Darwin ]; then NAME="dreamer-$VERSION-$TRIPLE.zip"; else NAME="dreamer-$TRIPLE.tar.gz"; fi
  URL="$RELEASES/download/v$VERSION/$NAME"
  say "downloading $URL"
  curl -fSL --progress-bar -o "$WORK/$NAME" "$URL" || die "download failed: $URL"
  curl -fsSL -o "$WORK/$NAME.sha256" "$URL.sha256" || die "checksum download failed"
  [ "$(sha256 "$WORK/$NAME")" = "$(cut -d' ' -f1 "$WORK/$NAME.sha256")" ] || die "checksum mismatch"
  say "checksum verified"
  ARCHIVE="$WORK/$NAME"
fi

mkdir -p "$WORK/unpacked"

path_note() {
  case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) say "add $PREFIX/bin to your PATH, for example: echo 'export PATH=\"$PREFIX/bin:\$PATH\"' >> ~/.profile" ;;
  esac
  command -v tmux >/dev/null 2>&1 || say "note: Dreamer runs on tmux; install tmux with your package manager"
}

if [ "$OS" = Darwin ]; then
  ditto -x -k "$ARCHIVE" "$WORK/unpacked" || die "could not unpack $ARCHIVE"
  APP="$WORK/unpacked/Dreamer.app"
  [ -x "$APP/Contents/MacOS/dreamer" ] || die "the archive does not look like a Dreamer release"
  codesign --verify --deep --strict "$APP" 2>/dev/null || die "Dreamer.app's code signature is not valid; not installing it"
  if ! spctl --assess --type execute "$APP" 2>/dev/null; then
    say "warning: macOS does not accept this build as notarized (a local or ad hoc build?)"
  fi
  VER="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist" 2>/dev/null || echo unknown)"

  if [ -z "$APP_DIR" ]; then
    if [ -w /Applications ]; then APP_DIR=/Applications; else APP_DIR="$HOME/Applications"; fi
  fi
  mkdir -p "$APP_DIR"
  RUNNING=0
  pgrep -x dreamer >/dev/null 2>&1 && RUNNING=1
  # Unpack beside the destination, then swap, so a failed copy leaves the
  # installed app as it was.
  rm -rf "$APP_DIR/.Dreamer.app.new"
  ditto "$APP" "$APP_DIR/.Dreamer.app.new" || die "could not write to $APP_DIR"
  rm -rf "$APP_DIR/Dreamer.app"
  mv "$APP_DIR/.Dreamer.app.new" "$APP_DIR/Dreamer.app"

  mkdir -p "$PREFIX/bin"
  for b in $MAC_TOOLS; do
    ln -sfn "$APP_DIR/Dreamer.app/Contents/MacOS/$b" "$PREFIX/bin/$b"
  done

  say "installed $VER as $APP_DIR/Dreamer.app"
  "$PREFIX/bin/dream" --version >/dev/null 2>&1 || say "warning: $PREFIX/bin/dream does not run on this system"
  path_note
  if [ "$RUNNING" = 1 ]; then
    say "Dreamer is running: quit and reopen it to use $VER (your tmux sessions keep running)"
  else
    say "open Dreamer from Launchpad or Spotlight, or run: open -a Dreamer"
  fi
  exit 0
fi

tar -xzf "$ARCHIVE" -C "$WORK/unpacked"
SRC="$(find "$WORK/unpacked" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
[ -n "$SRC" ] && [ -x "$SRC/bin/dreamer" ] || die "the archive does not look like a Dreamer release"
VER="$(cat "$SRC/VERSION" 2>/dev/null || echo unknown)"

# Unpack the version, then switch the `current` link to it.
mkdir -p "$HOME_DIR/versions"
PREV="$(basename "$(readlink "$HOME_DIR/current" 2>/dev/null || echo none)")"
rm -rf "$HOME_DIR/versions/$VER"
mv "$SRC" "$HOME_DIR/versions/$VER"
ln -sfn "versions/$VER" "$HOME_DIR/current"
mkdir -p "$PREFIX/bin"
for b in dreamer dreamerd dream dreamer-termapp; do
  ln -sfn "$HOME_DIR/current/bin/$b" "$PREFIX/bin/$b"
done
# Keep the version this one replaced, for going back; drop older ones.
for old in "$HOME_DIR"/versions/*; do
  [ -d "$old" ] || continue
  case "$(basename "$old")" in "$VER"|"$PREV") ;; *) rm -rf "$old" ;; esac
done

# Font for other applications too (Dreamer loads its own copy regardless).
mkdir -p "$FONTS"
cp "$HOME_DIR/current/share/dreamer/fonts/"* "$FONTS/"
command -v fc-cache >/dev/null 2>&1 && fc-cache -f "$DATA/fonts" >/dev/null 2>&1 || true

mkdir -p "$(dirname "$DESKTOP")"
sed "s|^Exec=.*|Exec=$PREFIX/bin/dreamer|" "$HOME_DIR/current/share/applications/dev.dreamer.Dreamer.desktop" > "$DESKTOP"

# The icon, in each size, where the desktop file's Icon= finds it.
if [ -d "$HOME_DIR/current/share/icons/hicolor" ]; then
  mkdir -p "$ICONS"
  cp -R "$HOME_DIR/current/share/icons/hicolor/." "$ICONS/"
  touch "$ICONS"
fi

mkdir -p "$(dirname "$UNIT")"
sed "s|^ExecStart=.*|ExecStart=$PREFIX/bin/dreamerd|" "$HOME_DIR/current/share/systemd/user/dreamerd.service" > "$UNIT"
if [ "$SERVICE" = 1 ] && command -v systemctl >/dev/null 2>&1; then
  systemctl --user daemon-reload && systemctl --user enable --now dreamerd && say "dreamerd service enabled"
fi

say "installed $VER into $HOME_DIR"
"$PREFIX/bin/dream" --version >/dev/null 2>&1 || say "warning: $PREFIX/bin/dream does not run on this system"
path_note
say "run \`dreamer\` (or find Dreamer in your app launcher)"
