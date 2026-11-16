#!/bin/sh
# Dreamer installer for Linux: installs into your home directory, no root.
#
#   curl -fsSL https://raw.githubusercontent.com/probablyrobot/dreamer/main/packaging/install.sh | sh
#   sh install.sh --from dreamer-x86_64-unknown-linux-gnu.tar.gz
#   sh install.sh --uninstall
#
# Options:
#   --from FILE        install this tarball instead of downloading a release
#   --version V        install release V (default: the latest release)
#   --prefix DIR       install under DIR (default: ~/.local)
#   --enable-service   also enable the dreamerd systemd user service
#   --uninstall        remove what this script installed
#
# Environment: DREAMER_VERSION, DREAMER_PREFIX mirror the options.
#
# Layout:
#   PREFIX/share/dreamer/versions/<version>/   unpacked release
#   PREFIX/share/dreamer/current               -> the active version
#   PREFIX/bin/{dreamer,dreamerd,dream}        -> current/bin/...
#   XDG_DATA_HOME/fonts/dreamer/               JetBrains Mono Nerd Font
#   XDG_DATA_HOME/applications/dev.dreamer.Dreamer.desktop
#   XDG_DATA_HOME/icons/hicolor/*/apps/dev.dreamer.Dreamer.{png,svg}
#   XDG_CONFIG_HOME/systemd/user/dreamerd.service
set -eu

REPO="probablyrobot/dreamer"
PREFIX="${DREAMER_PREFIX:-$HOME/.local}"
VERSION="${DREAMER_VERSION:-latest}"
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
    --enable-service) SERVICE=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) sed -n '2,25p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"
HOME_DIR="$PREFIX/share/dreamer"
DESKTOP="$DATA/applications/dev.dreamer.Dreamer.desktop"
ICONS="$DATA/icons/hicolor"
FONTS="$DATA/fonts/dreamer"
UNIT="$CONFIG/systemd/user/dreamerd.service"

if [ "$UNINSTALL" = 1 ]; then
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
  say "removed. Your settings in $CONFIG/dreamer and any tmux sessions are untouched."
  exit 0
fi

[ "$(uname -s)" = Linux ] || die "this installer is for Linux; on macOS use Dreamer.app"
case "$(uname -m)" in
  x86_64|amd64) TRIPLE=x86_64-unknown-linux-gnu ;;
  aarch64|arm64) TRIPLE=aarch64-unknown-linux-gnu ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT INT TERM

if [ -n "$FROM" ]; then
  [ -f "$FROM" ] || die "no such file: $FROM"
  TARBALL="$FROM"
  if [ -f "$FROM.sha256" ]; then
    [ "$(sha256 "$FROM")" = "$(cut -d' ' -f1 "$FROM.sha256")" ] || die "checksum mismatch for $FROM"
    say "checksum verified"
  fi
else
  command -v curl >/dev/null 2>&1 || die "curl is needed to download a release"
  NAME="dreamer-$TRIPLE.tar.gz"
  if [ "$VERSION" = latest ]; then
    URL="https://github.com/$REPO/releases/latest/download/$NAME"
  else
    URL="https://github.com/$REPO/releases/download/v${VERSION#v}/$NAME"
  fi
  say "downloading $URL"
  curl -fsSL -o "$WORK/$NAME" "$URL" || die "download failed (is the release public?)"
  curl -fsSL -o "$WORK/$NAME.sha256" "$URL.sha256" || die "checksum download failed"
  [ "$(sha256 "$WORK/$NAME")" = "$(cut -d' ' -f1 "$WORK/$NAME.sha256")" ] || die "checksum mismatch"
  say "checksum verified"
  TARBALL="$WORK/$NAME"
fi

mkdir -p "$WORK/unpacked"
tar -xzf "$TARBALL" -C "$WORK/unpacked"
SRC="$(find "$WORK/unpacked" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
[ -n "$SRC" ] && [ -x "$SRC/bin/dreamer" ] || die "the tarball does not look like a Dreamer release"
VER="$(cat "$SRC/VERSION" 2>/dev/null || echo unknown)"

# Unpack the version, then switch the `current` link to it.
mkdir -p "$HOME_DIR/versions"
rm -rf "$HOME_DIR/versions/$VER"
mv "$SRC" "$HOME_DIR/versions/$VER"
ln -sfn "versions/$VER" "$HOME_DIR/current"
mkdir -p "$PREFIX/bin"
for b in dreamer dreamerd dream dreamer-termapp; do
  ln -sfn "$HOME_DIR/current/bin/$b" "$PREFIX/bin/$b"
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
case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) say "add $PREFIX/bin to your PATH, for example: echo 'export PATH=\"$PREFIX/bin:\$PATH\"' >> ~/.profile" ;;
esac
command -v tmux >/dev/null 2>&1 || say "note: Dreamer runs on tmux; install tmux with your package manager"
say "run \`dreamer\` (or find Dreamer in your app launcher)"
