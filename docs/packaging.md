# Packaging

| Platform | Command | Output |
| --- | --- | --- |
| macOS (Apple Silicon) | `cargo xtask package --platform macos` | `target/package/Dreamer.app` and `dreamer-<version>-aarch64-apple-darwin.zip` with its `.sha256` |
| Any Linux (generic) | `cargo xtask package --platform linux` (on Linux) | `target/package/linux/dreamer-<target>.tar.gz` and its `.sha256`, installed into your home directory by `install.sh` |
| Arch Linux | `cargo xtask package --platform arch` | `target/package/arch/`: source tarball, `PKGBUILD`, and, on Arch, `dreamer-<version>-1-<arch>.pkg.tar.zst` |

The desktop packages contain four executables: `dreamer` (desktop app),
`dreamerd` (daemon), `dream` (CLI), and `dreamer-termapp` (the runtime for
[pane apps](termapp.md) and filter panes, which the app and `dream` start
from beside themselves). Launched with no arguments,
`dreamer` follows the active pane of your default tmux server, starting
`dreamerd` if it isn't running. The default terminal font, JetBrains Mono
Nerd Font (Mono variant), comes with the package (see
[settings.md](settings.md)). tmux itself is not bundled. Dreamer is
tested against the tmux baseline in
[dependency-baseline.md](dependency-baseline.md).

## macOS

`Dreamer.app/Contents/MacOS` holds `dreamer`, `dreamerd`, `dream`, and
`dreamer-termapp`. The
app starts the daemon from there. `Contents/Resources/Fonts` holds the
default font (four styles and its license), which the app loads itself, so
nothing has to be installed system-wide. `Contents/Resources/Dreamer.icns`
is the application icon. Every executable is arm64 with a minimum
of macOS 15.0. Each helper is signed, then the bundle is ad hoc signed over
them. `cargo xtask package` verifies all of this, and runs
`dreamerd --self-check`, `dream --version`, and `dreamer-termapp --version`
from inside the bundle.

To install, unzip into `/Applications`. To use the CLI from a shell:

```sh
ln -s /Applications/Dreamer.app/Contents/MacOS/dream /usr/local/bin/dream
```

The bundle is ad hoc signed, not Developer ID signed or notarized; that is
a separate release step. For a zip downloaded from elsewhere, Gatekeeper
asks you to confirm the first launch: Control-click the app, choose Open,
then confirm.

The macOS gate builds a debug bundle on every commit (`bundle`), and the
release zip on every push (`package`). Both are launched outside the
source tree.

## Generic Linux (any distribution, no root)

Like oh-my-zsh, one command installs into your home directory:

```sh
curl -fsSL https://raw.githubusercontent.com/probablyrobot/dreamer/main/packaging/install.sh | sh
```

The script downloads the latest release tarball for your architecture,
verifies its SHA-256, and installs:

| Path | Content |
| --- | --- |
| `~/.local/share/dreamer/versions/<version>/` | The unpacked release; `~/.local/share/dreamer/current` points to the active one |
| `~/.local/bin/dreamer`, `dreamerd`, `dream`, `dreamer-termapp` | Links to the active version |
| `~/.local/share/fonts/dreamer/` | JetBrains Mono Nerd Font, for other applications too. Dreamer loads its own copy regardless |
| `~/.local/share/applications/dev.dreamer.Dreamer.desktop` | Launcher entry |
| `~/.local/share/icons/hicolor/*/apps/dev.dreamer.Dreamer.{png,svg}` | Application icon, 16 to 512 px and scalable |
| `~/.config/systemd/user/dreamerd.service` | Optional user service. Enable it with `--enable-service` |

Options:

| Option | Effect |
| --- | --- |
| `--from FILE` | Install a tarball you already have (a local build, or a CI artifact). Verified against `FILE.sha256` if that file exists |
| `--version V` | Install a specific release instead of the latest |
| `--prefix DIR` | Install under `DIR` instead of `~/.local` |
| `--uninstall` | Remove everything above. Your settings and tmux sessions are left alone |

`DREAMER_VERSION` and `DREAMER_PREFIX` work in place of `--version` and
`--prefix`.

The script tells you if `~/.local/bin` is not on your `PATH`, and reminds
you to install `tmux` if it is missing.

To build and install a tarball yourself:

```sh
cargo xtask package --platform linux
sh packaging/install.sh --from target/package/linux/dreamer-x86_64-unknown-linux-gnu.tar.gz
```

The tarball is built against glibc 2.39 (Ubuntu 24.04), so it runs on
current distributions, Arch included.

The repository is private for now. Until it is public, the `curl` line and
release downloads need GitHub access; use `--from` with a tarball from a CI
run or from `gh release download`.

## Headless server archive

For hosts reached over SSH, which need no desktop stack:

```sh
cargo xtask package --platform server [--target TRIPLE]
```

This writes `target/package/server/dreamer-server-<version>-<target>.tar.gz`
and its `.sha256`. The archive holds `bin/dreamerd`, `bin/dream`, and
`bin/dreamer-termapp`, the systemd user unit on Linux, `remote.md`,
`daemon.md`, and `cli.md`, the license files, and a `VERSION` file
(version, commit, target). There are no GUI, font, or graphics files.

After packing, the archive is unpacked in a temporary directory away from
the tree. Each executable runs with `--version`, on this host or through
the target's `CARGO_TARGET_<TRIPLE>_RUNNER` (CI uses `qemu-aarch64`). The
libraries it links are read from its headers (`readelf -d`, `otool -L`),
and the build fails if any is graphical (X11, xcb, Wayland, xkbcommon,
Vulkan, GL, fontconfig, FreeType, HarfBuzz, AppKit, Metal, CoreText,
CoreGraphics, QuartzCore).

CI builds and uploads `x86_64-unknown-linux-gnu` and
`aarch64-unknown-linux-gnu` as workflow artifacts on every run; the Mac
builds `aarch64-apple-darwin`.

## Releases

Pushing a `v*` tag runs `.github/workflows/release.yml`. It builds the
Linux tarball and attaches it, its `.sha256`, and `install.sh` to a draft
GitHub release.

There are no macOS runners, so the macOS zip is built on the Mac and added
by hand:

```sh
cargo xtask package --platform macos
gh release upload vX.Y.Z target/package/dreamer-*-aarch64-apple-darwin.zip*
```

## Arch Linux

On an Arch system with `base-devel`:

```sh
cargo xtask package --platform arch
sudo pacman -U target/package/arch/dreamer-*.pkg.tar.zst
```

`cargo xtask package --platform arch`:
- writes a `git archive` tarball of the current commit (uncommitted changes
  are not included);
- writes a `PKGBUILD` with its checksum;
- runs `makepkg`.

Use `--source-only` to stop after the tarball and `PKGBUILD`, for example to
build on another machine.

`makepkg` checks the build dependencies against pacman's database. If you
installed `rustup` with its own installer instead of `pacman -S rustup`,
that check fails even though `cargo` works. Pass `--nodeps` to skip it
(`makepkg --nodeps`).

The build fetches the pinned Rust toolchain through `rustup` and
`rust-toolchain.toml`, plus the pinned, checksum-verified Zig and
libghostty-vt sources (`cargo xtask deps prepare`). It needs network access
for both.

Installed files:

| Path | Content |
| --- | --- |
| `/usr/bin/dreamer`, `/usr/bin/dreamerd`, `/usr/bin/dream` | The executables |
| `/usr/share/applications/dev.dreamer.Dreamer.desktop` | Launcher entry |
| `/usr/share/icons/hicolor/*/apps/dev.dreamer.Dreamer.{png,svg}` | Application icon, 16 to 512 px and scalable |
| `/usr/lib/systemd/user/dreamerd.service` | Optional user service: `systemctl --user enable --now dreamerd`. Not needed, since `dreamer` and `dream` start the daemon on demand |
| `/usr/share/licenses/dreamer/` | License and notices |
| `/usr/share/doc/dreamer/` | These documents |

Runtime dependencies: `tmux`, `ttf-jetbrains-mono-nerd` (the default
font, from Arch's official repositories), `fontconfig`, `freetype2`, `libxkbcommon`,
`libxkbcommon-x11`, `libx11`, `libxcb`, `wayland`, and
`vulkan-icd-loader` (plus a Vulkan driver for your GPU).

Linux CI builds the package in an `archlinux:base-devel` container on every
pull request. It then installs the package with `pacman -U` and runs the
installed binaries:
- the daemon's self-check;
- the CLI's version;
- the app's no-display exit;
- `dream ls` against an isolated tmux server.
