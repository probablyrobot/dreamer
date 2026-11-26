# Installing Dreamer

Dreamer runs on macOS 15 or later (Apple Silicon) and on Linux (x86-64 and
ARM64, glibc 2.39 or later: Ubuntu 24.04, Debian 13, Fedora 40, RHEL 10 and
newer).

It needs [tmux](https://github.com/tmux/tmux), which you install yourself:
`brew install tmux`, `sudo apt install tmux`, `sudo dnf install tmux`.
Dreamer is tested with tmux 3.8.

## Install

One command installs the latest release for your system. It does not need
root.

```sh
curl -fsSL https://github.com/probablyrobot/dreamer/releases/latest/download/install.sh | sh
```

The script downloads the release, checks its SHA-256, and installs it:

| | macOS | Linux |
| --- | --- | --- |
| The app | `Dreamer.app` in `/Applications` (or `~/Applications` if you cannot write there) | `~/.local/share/dreamer`, with an entry in your app launcher |
| Commands | `dream`, `dreamerd`, `dreamer-termapp` in `~/.local/bin` | `dreamer`, `dream`, `dreamerd`, `dreamer-termapp` in `~/.local/bin` |

On macOS the app is signed with a Developer ID and notarized by Apple. The
script checks the signature before it installs anything.

If `~/.local/bin` is not on your `PATH`, the script prints the line to add
to your shell profile.

Then open Dreamer from your launcher, or run `open -a Dreamer` (macOS) or
`dreamer` (Linux). [The desktop app](desktop.md) describes what you see.

To read the script before running it:

```sh
curl -fsSLO https://github.com/probablyrobot/dreamer/releases/latest/download/install.sh
less install.sh
sh install.sh
```

### Options

| Option | Meaning |
| --- | --- |
| `--version V` | Install release `V` instead of the latest |
| `--from FILE` | Install an archive you already have: a `.tar.gz` on Linux, a `.zip` on macOS |
| `--prefix DIR` | Install under `DIR` instead of `~/.local` |
| `--app-dir DIR` | macOS: where `Dreamer.app` goes |
| `--enable-service` | Linux: also start `dreamerd` as a systemd user service |
| `--uninstall` | Remove what the script installed |

Pass options after `sh -s --`:

```sh
curl -fsSL https://github.com/probablyrobot/dreamer/releases/latest/download/install.sh | sh -s -- --version 0.13.0
```

`DREAMER_VERSION`, `DREAMER_PREFIX`, and `DREAMER_APP_DIR` in the
environment do the same as the options.

## Update

```sh
dream update            # install the latest release over this one
dream update --check    # only say whether a newer release exists
dream update --version 0.13.0
```

`dream update` runs the new release's installer into the place Dreamer is
already installed. Quit and reopen Dreamer afterwards. Your tmux sessions
keep running through an update: Dreamer draws them, tmux owns them.

On Linux the installer keeps the version you had before, in
`~/.local/share/dreamer/versions/`, and removes older ones.

If you installed a `.deb`, `.rpm`, or Arch package, `dream update` leaves
it alone and tells you so. Install the newer package the same way you
installed the first one.

## Uninstall

```sh
curl -fsSL https://github.com/probablyrobot/dreamer/releases/latest/download/install.sh | sh -s -- --uninstall
```

If you installed with `--prefix` or `--app-dir`, pass the same option. Your
settings in `~/.config/dreamer` and your tmux sessions are not touched.

## Other ways to install

Every release on the
[releases page](https://github.com/probablyrobot/dreamer/releases) has
these files, each with a `.sha256` beside it.

### macOS, by hand

Download `dreamer-<version>-aarch64-apple-darwin.zip`, unzip it, and move
`Dreamer.app` to `/Applications`. The command-line tools are inside the
app. To put `dream` on your `PATH`:

```sh
mkdir -p ~/.local/bin
ln -s /Applications/Dreamer.app/Contents/MacOS/dream ~/.local/bin/dream
```

### Ubuntu and Debian

For Ubuntu 24.04 or later and Debian 13:

```sh
sudo apt install ./dreamer-<version>-x86_64-unknown-linux-gnu.deb
```

Use the `aarch64` file on ARM64. `apt` installs tmux and the libraries
Dreamer needs. Remove it with `sudo apt remove dreamer`.

### Fedora and RHEL

For Fedora 40 or later, and RHEL, AlmaLinux, or Rocky Linux 10:

```sh
sudo dnf install ./dreamer-<version>-x86_64-unknown-linux-gnu.rpm
```

Remove it with `sudo dnf remove dreamer`.

### Arch Linux

There is no prebuilt Arch package yet. Build one from a checkout; see
[Packaging](packaging.md#arch-linux).

### Any Linux, from the tarball

`dreamer-<triple>.tar.gz` is what the install script uses. With the file
already downloaded:

```sh
sh install.sh --from dreamer-x86_64-unknown-linux-gnu.tar.gz
```

On a system where you manage libraries yourself, the desktop needs xcb,
xkbcommon, Wayland, fontconfig, and a working Vulkan driver. On Ubuntu
24.04 those are:

```sh
sudo apt install tmux libxcb1 libxkbcommon0 libxkbcommon-x11-0 \
  libwayland-client0 libvulkan1 mesa-vulkan-drivers libfontconfig1
```

Dreamer runs on Wayland and on X11.

### A server with no display

`dreamer-server-<version>-<triple>.tar.gz` holds `dreamerd`, `dream`, and
`dreamer-termapp` with no graphical dependencies. You do not usually
install it by hand: `dream hosts install NAME` puts it on a host you reach
over SSH. See [Remote hosts](remote.md).

### Rust developers

`dream` and `dreamerd` build from crates.io; neither needs Zig:

```sh
cargo install dreamer-cli dreamer-server
```

The SDK for writing extensions and apps in Rust is on crates.io:

```sh
cargo add dreamer-sdk
```

See [Extensions](extensions.md) and [the app protocol](app-protocol.md).

## Web pages in panes

Web panes need Dreamer Web, a Chromium component that is installed apart
from the app so the app stays small. Dreamer offers to add it the first
time you open a web pane. You can also run:

```sh
dream web install
dream web status
dream web remove
```

It is available for macOS (Apple Silicon) and Linux (x86-64). The x86-64
Linux packages (tarball, `.deb`, `.rpm`, and Arch) are built with web
panes; Linux on ARM64 has none yet. See [Dreamer Web](desktop.md#dreamer-web).

## Set up your agents

Dreamer ships skills that teach Claude Code, Codex, and opencode how to
configure Dreamer and build pane apps for you:

```sh
dream skills install
```

To let an agent see and drive your panes as well, install the Dreamer
plugin, which bundles those skills with Dreamer's MCP server:

```sh
claude plugin marketplace add probablyrobot/dreamer
claude plugin install dreamer@dreamer
```

[MCP](mcp.md#connect-claude-code-codex-or-opencode) covers Codex,
opencode, other clients, and how much the agent may do.

## Build from source

See [Building Dreamer](building.md).
