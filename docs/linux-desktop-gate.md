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
3. Launches `dreamer --smoke` on the built-in demo and on every renderer
   fixture. A run passes only if the app reports the expected backend and
   presents a frame. A session without a display server fails instead of
   drawing nothing.
4. Writes `target/gate/linux-desktop/<commit>.json` with distribution,
   kernel, desktop, session type, and each run's result and output.

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
