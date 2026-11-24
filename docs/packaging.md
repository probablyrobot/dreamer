# Packaging

How to build, check, and sign Dreamer's packages, and how to make a
release. This page is for maintainers.

End users: see [Installing Dreamer](install.md).

| Package | Command | Output |
| --- | --- | --- |
| macOS app (Apple Silicon) | `cargo xtask package --platform macos` | `target/package/Dreamer.app` and `dreamer-<version>-aarch64-apple-darwin.zip`; the [Dreamer Web](#dreamer-web) component, `target/package/dreamer-web/` and `dreamer-web-<version>-aarch64-apple-darwin.zip`. Each zip has a `.sha256` and a `.json` record |
| Linux tarball (any distribution) | `cargo xtask package --platform linux` | `target/package/linux/dreamer-<target>.tar.gz` and its `.sha256` |
| `.deb` | `cargo xtask package --platform deb` | `target/package/linux/dreamer-<version>-<target>.deb` and its `.sha256` |
| `.rpm` | `cargo xtask package --platform rpm` | `target/package/linux/dreamer-<version>-<target>.rpm` and its `.sha256` |
| Headless server archive | `cargo xtask package --platform server [--target TRIPLE]` | `target/package/server/dreamer-server-<version>-<target>.tar.gz` and its `.sha256` |
| Arch Linux | `cargo xtask package --platform arch` | `target/package/arch/`: source tarball, `PKGBUILD`, and, where `makepkg` is installed, `dreamer-<version>-1-<arch>.pkg.tar.zst` |

`<target>` is a Rust target triple: `aarch64-apple-darwin`,
`x86_64-unknown-linux-gnu`, or `aarch64-unknown-linux-gnu`. The macOS
packages are built on a Mac, and the Linux tarball, `.deb`, and `.rpm` on
Linux (or in a container, see
[below](#build-in-a-container-and-check-on-fresh-machines)). Without
`--platform`, a Mac builds the macOS packages; on Linux you must name one.
`--debug` builds the macOS packages from a debug build.

The desktop packages contain four executables: `dreamer` (desktop app),
`dreamerd` (daemon), `dream` (CLI), and `dreamer-termapp` (the runtime for
[pane apps](termapp.md) and filter panes, which the app and `dream` start
from beside themselves). The macOS app, the Linux tarball, the `.deb`, and
the `.rpm` also carry the bundled fonts: the default, JetBrains Mono Nerd
Font (Mono variant), and the four other families listed in the
[dependency baseline](dependency-baseline.md#bundled-fonts), each with its
license. tmux is not bundled. Dreamer is tested against the tmux baseline
in [dependency-baseline.md](dependency-baseline.md).

## macOS app

```sh
cargo xtask package --platform macos
```

| Path in `Dreamer.app/Contents/` | Content |
| --- | --- |
| `MacOS/dreamer`, `dreamerd`, `dream`, `dreamer-termapp` | The executables. The app starts the daemon from here |
| `Resources/Fonts/` | The bundled fonts and their licenses. The app loads them itself, so nothing is installed system-wide |
| `Resources/Dreamer.icns` | The application icon |
| `Resources/LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.md` | The license and notices |

Each helper executable is signed, then the bundle is signed over them.
Before zipping, `cargo xtask package` checks the bundle:

- `codesign --verify --strict --deep` passes;
- every executable is arm64 with a minimum of macOS 15.0;
- the icon, at least four font files, and the font license are present;
- no part of Chromium is inside (see [Dreamer Web](#dreamer-web));
- `dreamerd --self-check`, `dream --version`, and
  `dreamer-termapp --version` run from inside the bundle.

By default the bundle is ad hoc signed, which runs on the Mac that built
it. An ad hoc zip opened on another Mac needs its first launch confirmed:
Control-click the app, choose Open, then confirm.

Beside each zip, `<zip>.json` records what was built and how it is
signed: size and SHA-256, the commit, the target and minimum macOS, the
executables, and the signing kind (`adhoc` or the signing authority),
team, and whether a notarization ticket is stapled.

The macOS gate builds a debug bundle on every commit (its `bundle`
stage), and the release zips on every push (its `package` stage). Both
are launched outside the source tree. The gate always signs ad hoc and
never needs the keychain or Apple's servers.

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
  process from the base helper, so V8 needs it there too). The plugin
  helper may run unsigned executable memory and load libraries signed by
  others. Its helper apps are stapled; the framework cannot hold a
  ticket, and Gatekeeper finds its ticket online.
- The zips, their `.sha256` and their records are made after signing,
  so the records say `Developer ID Application: …` and `notarized: true`.

`cargo xtask web smoke --packaged` draws pages with the app and component
in `target/package`, to check a signed build.

`PROFILE` is a keychain profile you store once. It asks for an
app-specific password, which you make at account.apple.com under
Sign-In and Security:

```sh
xcrun notarytool store-credentials PROFILE --apple-id APPLE_ID --team-id TEAMID
```

Every release is signed and notarized (see [Releases](#releases)).

### Dreamer Web

Chromium, which draws [web panes](desktop.md#web-panes), is not in
`Dreamer.app`. It is a second artifact, the Dreamer Web component, that
users add with `dream web install` ([desktop.md](desktop.md#dreamer-web)).
The app is built with web panes (its `web` feature), so the same
`Dreamer.app` works with or without the component. `cargo xtask package`
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

To check that a user can add the component, run:

```sh
cargo xtask package --fresh --platform macos
```

It works away from the tree, with the zips already in `target/package`:

1. Unpacks the app's zip into a new temporary directory.
2. With an empty data directory, checks that the app's `dream web status`
   says Dreamer Web is missing and how to add it.
3. Installs the component from a copy of its zip with `dream web install`.
4. Checks `dream web status` again, then removes the component.

It writes `target/package/fresh-macos.json`, which the build report
matches to the zip by checksum. This runs on the development Mac, not a
fresh one. The gate's `package` stage runs the same check on every push,
and its `web` stage installs the debug component and draws pages with it
([dependency-baseline.md](dependency-baseline.md#web-engine)).

For development, `cargo xtask web bundle [--release] [--install]` builds
the app with web panes and the component beside it, in `web-bundle`
under cargo's target directory. `cargo xtask web smoke` runs the gate's
web pane check alone.

The Linux component is not built yet: web panes are macOS only so far.

## Linux tarball

On Linux, after `cargo xtask deps prepare`:

```sh
cargo xtask package --platform linux
```

This writes `target/package/linux/dreamer-<target>.tar.gz` and its
`.sha256`. The file name carries no version, so
`releases/latest/download/dreamer-<target>.tar.gz` always resolves. The
tarball unpacks to one directory, `dreamer-<version>-<target>/`:

| Path | Content |
| --- | --- |
| `bin/dreamer`, `dreamerd`, `dream`, `dreamer-termapp` | The executables |
| `share/dreamer/fonts/` | The bundled fonts and their licenses |
| `share/applications/dev.dreamer.Dreamer.desktop` | Launcher entry |
| `share/icons/hicolor/` | Application icon, 16 to 512 px and scalable |
| `share/systemd/user/dreamerd.service` | Optional user service |
| `share/doc/dreamer/` | These documents |
| `LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.md` | The license and notices |
| `install.sh` | The installer, `packaging/install.sh` |
| `VERSION` | The version |

`packaging/install.sh` installs a tarball into a home directory, with no
root; [Installing Dreamer](install.md) describes what it does. To try a
tarball you built:

```sh
sh packaging/install.sh --from target/package/linux/dreamer-x86_64-unknown-linux-gnu.tar.gz
```

`--from` checks the file against `FILE.sha256` when that file is beside
it.

The tarball is built against glibc 2.39 (Ubuntu 24.04), so it runs on
distributions with that glibc or a newer one.

**Runtime packages.** Besides the C and C++ runtimes, the desktop app
needs xcb and xkbcommon, which it links, and Wayland, Vulkan, and
fontconfig, which it loads at run time. On Ubuntu 24.04:

```sh
sudo apt install libxcb1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 \
  libvulkan1 mesa-vulkan-drivers libfontconfig1
```

The fresh-machine check below installs exactly this list
(`DESKTOP_RUNTIME` in `xtask/src/fresh.rs`), so keep the two in step. The
server archive needs none of these packages.

### Build in a container and check on fresh machines

```sh
cargo xtask package --platform linux --container [--arch arm64|amd64]
cargo xtask package --fresh [--arch arm64|amd64]
```

Both need Docker. `--arch` defaults to the machine's own architecture.

- **`--container`** builds the tarball, the `.deb`, the `.rpm`, and the
  server archive in an `ubuntu:24.04` container. On an Apple Silicon Mac
  this is how the ARM64 packages are made.
- **`--fresh`** checks what is in `target/package` on fresh containers:
  - the server archive on a bare `ubuntu:24.04` image with nothing
    added, where each executable runs;
  - the tarball, installed by `install.sh` for a new user on
    `ubuntu:24.04` with only the packages above, then `dreamer --smoke`
    on X11 (Xvfb, with Mesa's software Vulkan), which must present a
    frame;
  - the `.deb` and the `.rpm`, when they were built (see
    [below](#deb-and-rpm)).

  It writes `target/package/fresh-<arch>.json`, which the build report
  matches to the archives by checksum.

The weekly Linux workflow (`.github/workflows/full.yml`, also run on
version tags and on demand) builds the tarball and the server archive
and runs `--fresh` for x86-64. It also installs the tarball into an
empty home directory, runs it, and uninstalls it. The release workflow
builds all four packages and runs `--fresh` for x86-64.

## .deb and .rpm

On Linux, with `dpkg-deb` and `rpmbuild` installed (the `rpm` package on
Ubuntu):

```sh
cargo xtask package --platform deb
cargo xtask package --platform rpm
```

Each builds the tarball first and packs the same files, so both come
from the glibc 2.39 build. They write to `target/package/linux/` with a
`.sha256` beside them. `--container` builds them along with the tarball.

| Package | For | Not for |
| --- | --- | --- |
| `.deb` | Ubuntu 24.04 and later, Debian 13 and later | Ubuntu 22.04, Debian 12 (older glibc) |
| `.rpm` | Fedora 40 and later; RHEL, AlmaLinux, and Rocky Linux 10 | RHEL 9 and older (older glibc) |

Installed files:

| Path | Content |
| --- | --- |
| `/usr/bin/dreamer`, `dreamerd`, `dream`, `dreamer-termapp` | The executables |
| `/usr/share/dreamer/fonts/` | The bundled fonts, which Dreamer loads itself |
| `/usr/share/applications/dev.dreamer.Dreamer.desktop` | Launcher entry |
| `/usr/share/icons/hicolor/*/apps/dev.dreamer.Dreamer.{png,svg}` | Application icon |
| `/usr/lib/systemd/user/dreamerd.service` | Optional user service: `systemctl --user enable --now dreamerd` |
| `/usr/share/doc/dreamer/`, `/usr/share/licenses/dreamer/` | These documents, the license and notices |

Declared dependencies:

| Package | Depends on | Recommends |
| --- | --- | --- |
| `.deb` | `tmux`, `libc6 (>= 2.39)`, `libgcc-s1`, `libstdc++6`, `libxcb1`, `libxkbcommon0`, `libxkbcommon-x11-0`, `libwayland-client0`, `libvulkan1`, `libfontconfig1` | `mesa-vulkan-drivers` |
| `.rpm` | `tmux`, `libwayland-client`, `vulkan-loader`, `fontconfig`, `libxkbcommon-x11`, and the libraries the executables link, which rpm finds by its own scan | `mesa-vulkan-drivers` |

**Checked on fresh machines.** `cargo xtask package --fresh` installs the
`.deb` with apt on `ubuntu:24.04`, and the `.rpm` with dnf on `fedora:44`
and `almalinux:10`. On each it runs the command-line tools as a new user.
On Ubuntu and Fedora it also runs `dreamer --smoke` on X11 (Xvfb), which
must present a frame. RHEL 10 ships no X server, Xvfb included, so no
frame is drawn on AlmaLinux.

## Headless server archive

For hosts reached over SSH, which need no desktop stack:

```sh
cargo xtask package --platform server [--target TRIPLE]
```

This writes `target/package/server/dreamer-server-<version>-<target>.tar.gz`
and its `.sha256`. Without `--target` it builds for this machine. The
archive unpacks to one directory, `dreamer-server-<version>-<target>/`:

| Path | Content |
| --- | --- |
| `bin/dreamerd`, `bin/dream`, `bin/dreamer-termapp` | The executables |
| `share/systemd/user/dreamerd.service` | The systemd user unit (Linux targets only) |
| `share/doc/dreamer/` | `remote.md`, `daemon.md`, and `cli.md` |
| `LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.md` | The license and notices |
| `VERSION` | Version, commit, and target |

There are no GUI, font, or graphics files.
[remote.md](remote.md#installing-a-server) describes how `dream hosts
install` puts an archive on a host.

After packing, the archive is unpacked in a temporary directory away from
the tree. Each executable runs with `--version`, on this host or through
the target's `CARGO_TARGET_<TRIPLE>_RUNNER` (CI uses `qemu-aarch64`). The
libraries it links are read from its headers (`readelf -d`, `otool -L`),
and the build fails if any is graphical (X11, xcb, Wayland, xkbcommon,
Vulkan, GL, EGL, fontconfig, FreeType, HarfBuzz, AppKit, Metal, CoreText,
CoreGraphics, QuartzCore). An archive for another architecture with no
runner set is packed but not run or checked, and the command says so.

The weekly Linux workflow builds and uploads `x86_64-unknown-linux-gnu`
and `aarch64-unknown-linux-gnu` as workflow artifacts. On a Mac the
command builds `aarch64-apple-darwin`.

## Arch Linux

On an Arch system with `base-devel`:

```sh
cargo xtask package --platform arch
sudo pacman -U target/package/arch/dreamer-*.pkg.tar.zst
```

`cargo xtask package --platform arch`:

1. Writes a `git archive` tarball of the current commit. Uncommitted
   changes are not included.
2. Writes a `PKGBUILD` from `packaging/arch/PKGBUILD.in`, with the
   version and the tarball's checksum.
3. Runs `makepkg`, when it is installed.

Use `--source-only` to stop after the tarball and `PKGBUILD`, for example
to build on another machine.

`makepkg` checks the build dependencies (`rustup`, `git`, `curl`, `xz`,
`pkgconf`) against pacman's database. If you installed `rustup` with its
own installer instead of `pacman -S rustup`, that check fails even though
`cargo` works. Pass `--nodeps` to skip it (`makepkg --nodeps`).

The build fetches the pinned Rust toolchain through `rustup` and
`rust-toolchain.toml`, plus the pinned, checksum-verified Zig and
libghostty-vt sources (`cargo xtask deps prepare`). It needs network access
for both.

Installed files:

| Path | Content |
| --- | --- |
| `/usr/bin/dreamer`, `dreamerd`, `dream`, `dreamer-termapp` | The executables |
| `/usr/share/applications/dev.dreamer.Dreamer.desktop` | Launcher entry |
| `/usr/share/icons/hicolor/*/apps/dev.dreamer.Dreamer.{png,svg}` | Application icon, 16 to 512 px and scalable |
| `/usr/lib/systemd/user/dreamerd.service` | Optional user service: `systemctl --user enable --now dreamerd`. Not needed, since `dreamer` and `dream` start the daemon on demand |
| `/usr/share/licenses/dreamer/` | License and notices |
| `/usr/share/doc/dreamer/` | These documents |

The Arch package ships no fonts. Its runtime dependencies are `tmux`,
`ttf-jetbrains-mono-nerd` (the default font, from Arch's official
repositories), `fontconfig`, `freetype2`, `libxkbcommon`,
`libxkbcommon-x11`, `libx11`, `libxcb`, `wayland`, `vulkan-icd-loader`
(plus a Vulkan driver for your GPU), `gcc-libs`, and `glibc`. The other
four font families are optional dependencies: `ttf-firacode-nerd`,
`ttf-hack-nerd`, `ttf-cascadia-code-nerd`, and `ttf-sourcecodepro-nerd`.

The weekly Linux workflow (`.github/workflows/full.yml`) builds the
package in an `archlinux:base-devel` container. It then installs the
package with `pacman -U` and runs the installed binaries:

- the daemon's self-check;
- the CLI's and the pane-app runtime's version;
- the app's no-display exit;
- `dream ls` against an isolated tmux server.

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
- the keychain has exactly one Developer ID Application identity (or
  `--sign` names one), and the notary profile works: `dreamer-notary`, or
  the one `--notarize` names
  ([Signing for distribution](#signing-for-distribution)).

Then:

1. **The Mac's artifacts, built from nothing.** `target/package` is
   emptied first.
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

Calls to GitHub are retried, and the wait for the workflow survives a
dropped connection. If a release still stops after its tag was made
(the network, or a failed upload), finish it with:

```sh
cargo xtask release vX.Y.Z --resume
```

It checks that the tag is this commit and that `target/package` holds this
release's notarized macOS zip and ARM64 fresh record. It then does step 3
again, without rebuilding or retagging.

The release stays a draft until you publish it. Every archive and package
has a `.sha256` beside it, and the license and notices are inside every
archive.

A finished draft holds these files:

| Asset | From |
| --- | --- |
| `dreamer-<version>-aarch64-apple-darwin.zip`, `.sha256`, `.json` | The Mac |
| `dreamer-web-<version>-aarch64-apple-darwin.zip`, `.sha256`, `.json` | The Mac |
| `dreamer-<target>.tar.gz`, `.sha256` | x86-64 from the workflow, ARM64 from the Mac |
| `dreamer-<version>-<target>.deb`, `.sha256` | x86-64 from the workflow, ARM64 from the Mac |
| `dreamer-<version>-<target>.rpm`, `.sha256` | x86-64 from the workflow, ARM64 from the Mac |
| `dreamer-server-<version>-<target>.tar.gz`, `.sha256` | x86-64 from the workflow, ARM64 from the Mac |
| `install.sh` | The workflow |
| `fresh-amd64.json`, `fresh-arm64.json`, `fresh-macos.json` | The fresh-check records |
| `build-report.md`, `build-report.json` | The Mac |

Here `<target>` is `x86_64-unknown-linux-gnu` or
`aarch64-unknown-linux-gnu`.

### The build report

`cargo xtask build-report [--ci-run ID]` writes
`target/package/build-report.md` and `.json`. `cargo xtask release` runs
it for you. It covers:

- the commit and tree, and whether the work tree was clean;
- the Rust toolchain, Zig, and macOS SDK;
- the pinned libghostty-vt, tmux baseline, and CEF;
- the compatibility baseline;
- each artifact: size, SHA-256 (checked against its `.sha256` file),
  target, signing state, and runtime check;
- the macOS gate for this tree, stage by stage;
- the CI run's jobs;
- everything not run.

An artifact with no matching check is listed as not checked. A tree with no
full macOS gate report is marked "macOS runtime verification incomplete".

### Crates on crates.io

Two groups are published to crates.io, with the workspace version:

- **The SDK:** `dreamer-sdk` and the crates it depends on (`dreamer-api`,
  `dreamer-app-protocol`, `dreamer-view-tree`).
- **The CLI:** `dreamer-cli`, which installs `dream`, and the crates under
  it, each named `dreamer-<directory>` (`dreamer-tmux-backend`,
  `dreamer-ghostty-sys`, and so on).

`cargo install dreamer-cli` needs no Zig: `dreamer-ghostty-sys` takes the
prebuilt libghostty-vt pinned in its `prebuilt.toml` (see
[the dependency baseline](dependency-baseline.md#prebuilt-archives-cratesio-builds)).

Inside the repository every published crate keeps its short dependency
key and library name (`tmux-backend`, `tmux_backend`), declared once in
the root `Cargo.toml` under `[workspace.dependencies]` with an exact
`=X.Y.Z` pin. Bump those pins with the workspace version; a mismatch
fails to resolve. Dev-dependencies stay path-only, so publishing drops
them.

After the release is published, from `main` at the release commit, publish
every crate marked `publish = true`; Cargo orders them by dependency:

```sh
crates=$(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.publish == null) | "-p " + .name')
cargo publish --dry-run --locked $crates
cargo publish --locked $crates
```

A published version can be yanked but never replaced or deleted.
