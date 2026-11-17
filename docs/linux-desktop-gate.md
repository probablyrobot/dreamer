# Linux desktop gate

Hosted CI can build and test Dreamer on Linux, but it cannot show that a
window renders on a real display. That evidence comes from a person running
`cargo xtask gate linux-desktop` in a graphical session on a Linux machine.
It is required to close milestones M0, M2, M3, and M8.

## What it does

1. Prepares the pinned native dependencies and builds `dreamer`.
2. For each display backend:
   - **Wayland:** runs with the session's `WAYLAND_DISPLAY`. This must be a
     native Wayland compositor, not XWayland.
   - **X11:** runs with `WAYLAND_DISPLAY` removed so GPUI connects to
     `DISPLAY`. Under a Wayland desktop that is XWayland, which counts as X11
     evidence only.
3. Launches `dreamer --smoke` on the built-in demo, on every snapshot
   fixture, and on the renderer text matrix in
   `crates/terminal-render/fixtures`. Each matrix fixture must also paint
   the statistics listed in its `expected.txt` (runs, wide cells,
   decorations, backgrounds, and cursor shape). A run passes only if the app reports the expected backend and
   presents a frame. A session without a display server fails instead of
   drawing nothing.
4. Runs the input smoke (`dreamer --smoke-input`, see `docs/input.md`) on
   each backend against an isolated tmux server and daemon. It types and
   commits an IME composition, then clicks and scrolls; the run passes only
   if the shell in the pane received exactly that input.
5. Runs the workspace smoke (tabs, panes, divider drag, palette, restore)
   and the remote smoke: a host's session through the local daemon's
   coordinator and a stand-in `ssh`. Typing must reach the host;
   disconnecting must mark the window stale with the pane's content kept;
   reconnecting must resume it ([desktop.md](desktop.md#remote-hosts)).
   The workspace smoke's click, keyboard, sizing, and drag checks need a
   window that receives new frames. When the display does not refresh
   (on Wayland, an output turned off by idle power saving, or a locked
   session), the run is reported as NOT RUN with `"not_run": true` in the
   report, not as a failure. The gate still exits non-zero, because the
   evidence is incomplete: wake the monitor and run it again.
6. Runs the clipboard smoke (`dreamer --smoke-clipboard`, see
   [desktop.md](desktop.md#clipboard-check)) on each backend: copy and
   paste in a terminal pane, through tmux's buffers, in the harness
   drawer, and in a native app pane, with an in-memory clipboard. On X11
   the run must also show that a finished selection became the primary
   selection. A window that does not refresh is reported as NOT RUN, as
   for the workspace smoke.
7. Writes `target/gate/linux-desktop/<commit>.json` with distribution,
   kernel, desktop, session type, compositor, which X server answered the
   X11 runs (`xwayland` or `xorg`), whether the run was over SSH, and each
   run's result and output.

Both backends are required. If the machine cannot offer one (for example a
pure X11 session), pass `--allow-missing wayland` or `--allow-missing x11`;
the report then shows the missing backend, and the evidence is incomplete.

## Running it

From a terminal inside the graphical session:

```sh
git clone https://github.com/probablyrobot/dreamer && cd dreamer
git checkout <revision under review>
cargo xtask doctor
cargo xtask gate linux-desktop
```

Attach the JSON report to the milestone issue or pull request.

### Over SSH

An SSH login has no display of its own, so both backends report "not
available". Someone must be logged in to the desktop on that machine; then
point the SSH shell at that session before running the gate (zsh-safe):

```sh
export XDG_RUNTIME_DIR=/run/user/$(id -u)
export WAYLAND_DISPLAY=$(basename "$(ls $XDG_RUNTIME_DIR/wayland-* | grep -v lock | head -1)")
export DISPLAY=:0   # see `ls /tmp/.X11-unix`: X0 is :0
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus
# X11 access file: GNOME, KDE, or the classic location; empty is fine for
# compositors whose XWayland does not require one (for example Hyprland).
export XAUTHORITY=$(sh -c 'ls "$1"/.mutter-Xwaylandauth.* "$1"/xauth_* ~/.Xauthority 2>/dev/null | head -1' - "$XDG_RUNTIME_DIR")
cargo xtask gate linux-desktop
```

The windows open on the machine's own screen. The report records
`"remote": true` and the compositor found from the running processes.

## Build packages

The Rust toolchain installs itself from `rust-toolchain.toml`. Zig and
libghostty-vt are fetched and verified by `cargo xtask deps prepare`.
Besides those, building the desktop app needs a C toolchain, `pkg-config`,
and the windowing and font development packages:

| Distribution | Packages |
| --- | --- |
| Arch | `base-devel pkgconf wayland libxkbcommon libx11 libxcb fontconfig freetype2 vulkan-icd-loader` |
| Ubuntu 24.04 | `build-essential pkg-config libfontconfig-dev libfreetype-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libvulkan-dev` |

A working Vulkan driver for the GPU is needed at run time.
