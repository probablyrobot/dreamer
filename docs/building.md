# Building Dreamer

How to build Dreamer from a checkout and run its checks. To install a
release instead, see [Installing Dreamer](install.md).

## Prerequisites

- **Rust**, through [rustup](https://rustup.rs). The toolchain version
  comes from `rust-toolchain.toml`; rustup installs it on first use.
- **tmux 3.8 or later.** If your system's tmux is older, `cargo xtask deps
  tmux` builds the pinned one and prints its path; set `DREAMER_TMUX` to
  it.
- **macOS:** full Xcode with the Metal toolchain:

  ```sh
  xcodebuild -downloadComponent MetalToolchain
  ```

- **Ubuntu 24.04:**

  ```sh
  sudo apt install build-essential pkg-config libfontconfig-dev \
    libfreetype-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev \
    libx11-xcb-dev libxcb1-dev libvulkan-dev
  ```

  Building the pinned tmux also needs `libevent-dev libncurses-dev bison`.

## Build and test

```sh
cargo xtask doctor          # checks the tools above and says what is missing
cargo xtask deps prepare    # fetches and builds the pinned native dependencies
cargo build --workspace --locked
cargo test --workspace --locked
```

`deps prepare` downloads the fonts, Zig, and the libghostty-vt source,
checks each against the SHA-256 pinned in `deps/baseline.toml`, and builds
the library into a cache in your home directory. Later builds are offline.
[The dependency baseline](dependency-baseline.md) lists every pin.

To build only the four executables:

```sh
cargo build --locked -p dreamer-app -p dreamer-server -p dreamer-cli -p dreamer-termapp
```

Run the desktop from the checkout with `cargo run -p dreamer-app`.

Web panes (macOS arm64 and Linux x86-64) are the `web` feature of
`dreamer-app`. On macOS building it needs one more step, `cargo xtask
deps web`, which fetches the pinned Chromium Embedded Framework; on
Linux it builds as is (`cargo build -p dreamer-app --features web`), and
Chromium comes at run time from the Dreamer Web component. `cargo xtask
deps web` then `cargo xtask web bundle` builds the desktop with web panes
and that component ([desktop.md](desktop.md#a-build-with-web-panes)).

## The layout

| Path | What it is |
| --- | --- |
| `crates/app` | `dreamer`, the desktop client |
| `crates/server` | `dreamerd`, the headless daemon |
| `crates/cli` | `dream`, the command-line interface |
| `crates/tui-runtime` | `dreamer-termapp`, the runtime for apps in panes |
| `crates/sdk` | `dreamer-sdk`, for extensions and apps written in Rust |
| `crates/*` | The libraries those are built from |
| `xtask` | Build tooling: `cargo xtask <command>` |
| `packaging` | The install script, package templates, and icons |
| `examples/apps` | Example pane apps |
| `docs` | This documentation |

The headless crates (daemon, CLI, app runtime) build without any GUI,
font, or GPU dependency. `cargo xtask headless --check` enforces that.

## Checks before you push

```sh
cargo xtask hooks install   # once per clone
```

This points git at the hooks in `.githooks/`:

- **pre-commit** runs `cargo xtask gate --staged` on exactly what is
  staged: formatting, lints, tests, and, on a desktop, checks that drive
  real windows.
- **pre-push** runs `cargo xtask gate --full`, which adds the release
  package and, on a Mac with Docker, the headless tests on ARM64 Linux in
  a container.

Reports are written to `target/gate/<platform>/<tree>-<mode>.json`. A
check that needs a display records "not run" when the screen is locked;
it does not pass silently.

Pull requests run formatting, lints, the build, and the tests on x86-64
Linux in GitHub Actions. The slower Linux checks run weekly, on version
tags, and on demand.

## Benchmarks

```sh
cargo xtask bench headless [--panes N] [--history LINES]  # daemon: memory, key to echo, tiers, typing under load
cargo xtask bench headless --panes 1000 --linux           # the same in the gate's ARM64 Linux container
cargo xtask bench busy [--panes N] [--idle M [--sessions S]]  # typing under load, split between tmux and the daemon
cargo xtask bench streams [--panes N] [--corpus NAME]... [--seconds S]  # cost of replayed byte corpora
cargo xtask bench tiers [--panes N] [--stream text|escape] [--rate KIB]  # a pane's cost in each tier
cargo xtask bench memory [--scenario NAME]... [--seconds S] [--warm-memory M]  # memory under pressure
cargo xtask bench extensions [--panes N] [--without PART]  # typing next to slow and stalled extensions
cargo xtask bench remote [--samples N] [--container [--netem SPEC]]  # key to echo through a host
cargo xtask bench gui [--panes N] [--visible N] [--load text|escape|image] [--backend wayland|x11]  # the desktop: launch, paint, key to echo
cargo xtask bench compare [--terminal NAME]... [--frames off|on|always] [--seconds S]  # next to other terminals (Linux)
cargo xtask bench report target/bench/<workload>-*.json   # result files as Markdown tables
```

Each workload builds release binaries and runs them against a private
tmux server and private daemon directories, never yours. It prints a
table and writes `target/bench/<workload>-<time>.json`, which records the
machine (model, CPU, cores, memory, system) and the revision (commit,
uncommitted changes, build profile, toolchain, tmux, and the pinned
libghostty-vt). Quote numbers with that machine and revision.

`--debug` runs the debug binaries instead: a check that the harness works,
never a measurement, and its results say so. CI runs it on every pull
request and discards the numbers.

To see where time goes in a running daemon, turn on its
[diagnostics](daemon.md#diagnostics).

## Packages and releases

[Packaging](packaging.md) covers building the macOS app, the Linux
archives and packages, and making a release.
[The Linux desktop gate](linux-desktop-gate.md) covers collecting evidence
that windows render on a real Linux display.
