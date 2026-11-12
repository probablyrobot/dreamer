# Packaging

| Platform | Command | Output |
| --- | --- | --- |
| macOS (Apple Silicon) | `cargo xtask package --platform macos` | `target/package/Dreamer.app` and `dreamer-<version>-aarch64-apple-darwin.zip` with its `.sha256` |
| Arch Linux | `cargo xtask package --platform arch` | `target/package/arch/`: source tarball, `PKGBUILD`, and, on Arch, `dreamer-<version>-1-<arch>.pkg.tar.zst` |

Both packages contain the three executables: `dreamer` (desktop app),
`dreamerd` (daemon), and `dream` (CLI). Launched with no arguments,
`dreamer` follows the active pane of your default tmux server, starting
`dreamerd` if it isn't running. The default terminal font, JetBrains Mono
Nerd Font (Mono variant), comes with the package (see
[settings.md](settings.md)). tmux itself is not bundled. Dreamer is
tested against the tmux baseline in
[dependency-baseline.md](dependency-baseline.md).

## macOS

`Dreamer.app/Contents/MacOS` holds `dreamer`, `dreamerd`, and `dream`. The
app starts the daemon from there. `Contents/Resources/Fonts` holds the
default font (four styles and its license), which the app loads itself, so
nothing has to be installed system-wide. Every executable is arm64 with a minimum
of macOS 15.0. Each helper is signed, then the bundle is ad hoc signed over
them. `cargo xtask package` verifies all of this, and runs
`dreamerd --self-check` and `dream --version` from inside the bundle.

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
