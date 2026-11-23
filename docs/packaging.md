# Packaging

| Platform | Command | Output |
| --- | --- | --- |
| macOS (Apple Silicon) | `cargo xtask package --platform macos` | `target/package/Dreamer.app` and `dreamer-<version>-aarch64-apple-darwin.zip` with its `.sha256`; and the [Dreamer Web](#dreamer-web) component, `target/package/dreamer-web/` and `dreamer-web-<version>-aarch64-apple-darwin.zip` with its `.sha256` |
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
of macOS 15.0. Each helper is signed, then the bundle is signed over
them. `cargo xtask package` verifies all of this, and runs
`dreamerd --self-check`, `dream --version`, and `dreamer-termapp --version`
from inside the bundle.

To install, unzip into `/Applications`. To use the CLI from a shell:

```sh
ln -s /Applications/Dreamer.app/Contents/MacOS/dream /usr/local/bin/dream
```

By default the bundle is ad hoc signed, which runs on the Mac that built
it. Beside the zip, `<zip>.json` records what was built and how it is
signed: size and SHA-256, the commit, the target and minimum macOS, the
executables, and the signing kind (`adhoc` or the signing authority),
team, and whether a notarization ticket is stapled. An ad hoc zip opened
on another Mac needs its first launch confirmed: Control-click the app,
choose Open, then confirm.

### Signing for distribution

To ship to other Macs, sign with a Developer ID Application certificate
and notarize:

```sh
cargo xtask package --platform macos \
  --sign "Developer ID Application: NAME (TEAMID)" \
  --notarize PROFILE
```

- `--sign` re-signs every executable and then the bundle under the
  hardened runtime with a secure timestamp. It refuses an identity that
  is not a Developer ID Application one; `security find-identity -v -p
  codesigning` lists yours.
- `--notarize` submits the bundle to Apple's notary service, waits for
  it (usually a few minutes), staples the ticket, and checks that
  Gatekeeper accepts the app as notarized. When notarization fails, the
  service's log is printed. It needs `--sign`.
- Dreamer Web is signed the same way, from the inside out: the
  framework's libraries, the framework, then each helper app with the
  entitlements Chromium gives its own helpers. The base, renderer, and
  GPU helpers may compile code (`allow-jit`; Chromium starts every child
  process from the base helper, so V8 needs it there too); the plugin
  helper may run unsigned executable memory and load libraries signed by
  others. `cargo xtask web smoke --packaged` draws pages with the
  packaged app and component, to check a signed build. Its
  helper apps are stapled; the framework cannot hold a ticket, and
  Gatekeeper finds its ticket online.
- The zips, their `.sha256` and their records are made after signing,
  so the records say `Developer ID Application: …` and `notarized: true`.

`PROFILE` is a keychain profile you store once. It asks for an
app-specific password, which you make at account.apple.com under
Sign-In and Security:

```sh
xcrun notarytool store-credentials PROFILE --apple-id APPLE_ID --team-id TEAMID
```

The gate always signs ad hoc and never needs the keychain or Apple's
servers. Every release is signed and notarized (see below).

The macOS gate builds a debug bundle on every commit (`bundle`), and the
release zip on every push (`package`). Both are launched outside the
source tree.

### Dreamer Web

Chromium, which draws [web panes](desktop.md#web-panes), is not in
`Dreamer.app`. It is a second artifact, the Dreamer Web component, that
users add with `dream web install` ([desktop.md](desktop.md#dreamer-web)).
The app is built with web panes (its `web` feature), so the same
`Dreamer.app` works with or without the component; `cargo xtask package`
fails if the framework or a helper app ends up in the bundle.

`cargo xtask package --platform macos` builds both. The component needs
the pinned CEF distribution, which `cargo xtask deps web` provides; the
package step fetches it (checksum-verified) when it is missing.

| Path in `dreamer-web/` | Content |
| --- | --- |
| `Chromium Embedded Framework.framework` | CEF's framework, from the pinned distribution, signed like the app |
| `dreamer Helper.app`, `dreamer Helper (GPU).app`, `dreamer Helper (Renderer).app`, `dreamer Helper (Plugin).app`, `dreamer Helper (Alerts).app` | The helper apps Chromium runs its renderer, GPU, and utility processes from, each holding `dreamer-web-helper` and signed like the app |
| `LICENSE.txt`, `CREDITS.html` | CEF's license and Chromium's third-party notices |
| `component.json` | The component's Dreamer version, CEF release, and target |

The zip, `dreamer-web-<version>-aarch64-apple-darwin.zip`, unpacks to
`dreamer-web/`. Beside it, `<zip>.sha256` and `<zip>.json` (size and
SHA-256, the commit, the CEF release, the contents, and the signing
state) are written as for the app's zip. `cargo xtask package` checks
the component before zipping it: the manifest, every file, `codesign
--verify --strict --deep` of the framework and each helper app, and a
helper loading the framework from where it sits.

`cargo xtask package --fresh --platform macos` then adds the component
the way a user does, away from the tree: it unpacks the app's zip into a
new temporary directory and, with an empty data directory, checks that
its `dream web status` says Dreamer Web is missing and how to add it,
installs the component from a copy of its zip with `dream web install`,
checks `dream web status` again, and removes it. It writes
`target/package/fresh-macos.json`, which the build report matches to the
zip by checksum. This runs on the development Mac, not a fresh one. The
gate's `package` stage runs the same check on every push, and its `web`
stage installs the debug component and draws pages with it
([dependency-baseline.md](dependency-baseline.md#web-engine)).

The Linux component (an optional `dreamer-web` archive) is not built
yet: web panes are macOS only so far.

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

**Runtime packages.** Besides the C and C++ runtimes, the desktop app
needs xcb and xkbcommon, which it links, and Wayland, Vulkan, and
fontconfig, which it loads at run time. On Ubuntu 24.04:

```sh
sudo apt install libxcb1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 \
  libvulkan1 mesa-vulkan-drivers libfontconfig1
```

The server archive needs none of them.

**Checked on fresh machines.** Two commands build and check the archives:

```sh
cargo xtask package --platform linux --container [--arch arm64|amd64]
cargo xtask package --fresh [--arch arm64|amd64]
```

- **`--container`** builds the desktop tarball and the server archive in
  an `ubuntu:24.04` container. On an Apple Silicon Mac this is how the ARM64
  archives are made.
- **`--fresh`** checks the archives on fresh `ubuntu:24.04` containers:
  - the server archive on a bare image with nothing added, where each
    executable runs;
  - the desktop tarball, installed by `install.sh` for a new user with
    only the packages above, then `dreamer --smoke` on X11 (Xvfb, with
    Mesa's software Vulkan), which must present a frame.

  The weekly Linux workflow (`.github/workflows/full.yml`, also run on
  version tags and on demand) and the release workflow run it for x86-64.

The repository is private for now. Until it is public, the `curl` line and
release downloads need GitHub access; use `--from` with a tarball from a CI
run or from `gh release download`.

## Ubuntu and Debian (.deb), Fedora and RHEL (.rpm)

Each release has a `.deb` and an `.rpm` for x86-64 and ARM64, built from
the same glibc 2.39 build as the tarball. Install one with your package
manager, which also installs tmux and the libraries it needs:

```sh
sudo apt install ./dreamer-<version>-x86_64-unknown-linux-gnu.deb   # Ubuntu 24.04+, Debian 13+
sudo dnf install ./dreamer-<version>-x86_64-unknown-linux-gnu.rpm   # Fedora 40+, RHEL/Alma/Rocky 10
```

RHEL 9 and older, Ubuntu 22.04, and Debian 12 have an older glibc and
cannot run it.

| Path | Content |
| --- | --- |
| `/usr/bin/dreamer`, `dreamerd`, `dream`, `dreamer-termapp` | The executables |
| `/usr/share/dreamer/fonts/` | Dreamer's own copy of its font |
| `/usr/share/applications/dev.dreamer.Dreamer.desktop` | Launcher entry |
| `/usr/share/icons/hicolor/*/apps/dev.dreamer.Dreamer.{png,svg}` | Application icon |
| `/usr/lib/systemd/user/dreamerd.service` | Optional user service: `systemctl --user enable --now dreamerd` |
| `/usr/share/doc/dreamer/`, `/usr/share/licenses/dreamer/` | These documents, the license and notices |

- **Dependencies.** Both declare `tmux` and the libraries the desktop
  loads at run time (Wayland, Vulkan, fontconfig, xkbcommon). They
  recommend Mesa's Vulkan drivers. The `.rpm` also gets the libraries the
  executables link from rpm's own scan.
- **Building them.** On Linux, with `dpkg-deb` and `rpmbuild` installed
  (`rpm` on Ubuntu):

  ```sh
  cargo xtask package --platform deb
  cargo xtask package --platform rpm
  ```

  Both write to `target/package/linux/` with a `.sha256` beside them.
  `--container` builds them along with the tarball.
- **Checked on fresh machines.** `--fresh` installs the `.deb` with apt on
  `ubuntu:24.04`, and the `.rpm` with dnf on `fedora:44` and
  `almalinux:10`. On each it runs the command-line tools. On Ubuntu and
  Fedora it also runs `dreamer --smoke` on X11 (Xvfb), which must present
  a frame. RHEL 10 ships no X server, Xvfb included, so no frame is drawn
  on AlmaLinux.

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

The weekly Linux workflow builds and uploads `x86_64-unknown-linux-gnu`
and `aarch64-unknown-linux-gnu` as workflow artifacts; the Mac builds
`aarch64-apple-darwin`.

## Releases

One command on the development Mac makes a release with both lanes'
artifacts and the build report:

```sh
cargo xtask release vX.Y.Z
```

It refuses to start unless:
- you are on `main`, at `origin/main`, with no uncommitted changes;
- the workspace version is `X.Y.Z`;
- the tag does not exist yet;
- this tree has passed the full macOS gate (`cargo xtask gate --full`, which
  pushing runs);
- the keychain has a Developer ID Application identity (or `--sign`
  names one), and the notary profile works: `dreamer-notary`, or the one
  `--notarize` names ([Signing for distribution](#signing-for-distribution)).

Then:

1. **The Mac's artifacts, built from nothing.**
   - The macOS zip and the Dreamer Web zip, Developer ID signed,
     notarized, and stapled, with their `.sha256` and signing records.
   - Web pages drawn with that app and component (`cargo xtask web smoke
     --packaged`), so keep the screen unlocked while it runs.
   - The ARM64 desktop tarball, `.deb`, `.rpm`, and server archive, built
     in an ARM64 container.
   - All of them run on fresh containers (`--fresh`).
2. **The tag,** created on GitHub. It starts
   `.github/workflows/release.yml`, which builds the x86-64 desktop
   tarball, `.deb`, `.rpm`, and server archive, runs them on fresh
   containers, and drafts the release with them and `install.sh`.
3. **The report.**
   - It waits for that run and takes its x86-64 archives back.
   - It writes the build report with the run attached.
   - It adds the Mac's artifacts, the fresh-check records, and the report
     to the draft.

The release stays a draft until you publish it. Every file has a `.sha256`
beside it, and the license and notices are inside every archive.

**The build report** (`cargo xtask build-report [--ci-run ID]`, spec
17.6) is written to `target/package/build-report.md` and `.json`. It
covers:
- the commit and tree, and whether the work tree was clean;
- the Rust toolchain, Zig, and macOS SDK;
- the pinned libghostty-vt and tmux baseline;
- the compatibility baseline;
- each artifact: size, SHA-256 (checked against its `.sha256` file),
  target, signing state, and runtime check;
- the macOS gate for this tree, stage by stage;
- the CI run's jobs;
- everything not run.

An artifact with no matching check is listed as not checked. A tree with no
full macOS gate report is marked "macOS runtime verification incomplete".

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

The weekly Linux workflow (`.github/workflows/full.yml`, also run on
version tags and on demand) builds the package in an `archlinux:base-devel`
container. It then installs the package with `pacman -U` and runs the
installed binaries:
- the daemon's self-check;
- the CLI's version;
- the app's no-display exit;
- `dream ls` against an isolated tmux server.
