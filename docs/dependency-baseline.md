# Dependency baseline

Pinned native dependencies and toolchains. The machine-readable source of
truth is [`deps/baseline.toml`](../deps/baseline.toml); this page explains it.
Changing any pin means re-running the parser, renderer, ABI, and native smoke
tests on both verification lanes.

## Rust

| Item | Pin | Where |
| --- | --- | --- |
| Toolchain | 1.99.1 | `rust-toolchain.toml` |
| Crates | exact versions | `Cargo.lock`, always built with `--locked` |

## libghostty-vt

| Item | Value |
| --- | --- |
| Release | 1.0.1 (newest stable library release on 2026-11-12) |
| Tag | `vt-1.0.1` |
| Source commit | `108cacb17ed8990840cd14abfce43d8ed8503ca1` |
| Source tarball | `libghostty-vt-1.0.1.tar.gz`, SHA-256 `333dfce97641ef9b536bb9e08907ad86abaaada50e8f7417b70d9284adef37aa` |
| Zig package tarball | `libghostty-vt-1.0.1-deps.tar.gz`, SHA-256 `91a4e10960a52bf5bf4b15aa2a2cddaff8a89d6cd903d3055bda4fd17de3356d` |
| Signature | `SHA256SUMS.minisig` verified with key `RWQlAjJC23149WL2sEpT/l0QKy7hMIFhYdQOFy0Z7z7PbneUgvlsnYcV` when pinned |
| ABI | C ABI 1.x (SONAME major 1); headers at the pinned release |
| Build | `zig build -Doptimize=ReleaseFast -Dcpu=baseline -Dtarget=<below>`, offline via `--system`, fresh Zig cache |
| Features | Default set: every optional VT feature, including snapshots, render state, input encoding, selection, search, and Kitty graphics |
| Linking | Static (`libghostty-vt.a`) into each executable that needs it |
| License | MIT |

Source tarball and Zig package tarball are the official release files, not
GitHub's tag archive (which builds the Ghostty app instead of the library).

### Zig

| Item | Value |
| --- | --- |
| Required | 0.17.x (from `minimum_zig_version = "0.17.0"`; minor must match exactly) |
| Pinned | 0.17.1 |
| Hosts | `aarch64-macos`, `x86_64-linux`, `aarch64-linux` tarballs with SHA-256 pins |

Zig is downloaded into the dependency cache by `cargo xtask deps prepare`; a
Zig on `PATH` is not used.

### Targets

| Rust target | Zig target | Baseline |
| --- | --- | --- |
| `aarch64-apple-darwin` | `aarch64-macos.15.0` | macOS 15 |
| `x86_64-unknown-linux-gnu` | `x86_64-linux-gnu.2.39` | glibc 2.39 (Ubuntu 24.04) |
| `aarch64-unknown-linux-gnu` | `aarch64-linux-gnu.2.39` | glibc 2.39 (Ubuntu 24.04) |

### Code linked into libghostty-vt

From `deps/libghostty-vt-third-party.json` in the release source:

| Project | Version | License |
| --- | --- | --- |
| highway | 1.2.0 | Apache-2.0 OR BSD-3-Clause |
| simdutf | 9.0.0 | Apache-2.0 OR MIT |
| translate_c | 2.0.0 | MIT |
| uucode | 0.2.0 | MIT AND Unicode-3.0 |
| wuffs | 0.4.0-alpha.10 | Apache-2.0 OR MIT |

`aro` (MIT AND Unicode-3.0) runs only during the build.

## Image decoding

| Crate | Version | Used by | License |
| --- | --- | --- | --- |
| `png` | `=0.18.1` (pure Rust) | `terminal-ghostty`: Ghostty's PNG decode callback for Kitty images | MIT OR Apache-2.0 |
| `resvg` | `=0.46.0` (pure Rust, default features off) | `xtask icons`: renders `packaging/icons/dreamer.svg` into the Linux PNGs and the macOS `icns` | Apache-2.0 OR MIT |

Both are pinned exactly and already in `Cargo.lock` through GPUI's graph.
They have no native code, and headless crates may use `png`; `resvg` is
used only by the build tool.

## Scripts, matching, and pane apps

| Crate | Version | Used by | License |
| --- | --- | --- | --- |
| `mlua` (features `luau`, `vendored`, `async`) | `0.12.2`, with its vendored Luau | `script-runtime`: sandboxed scripts | MIT (Luau: MIT) |
| `regex` | `1.13.1` | `matcher`: the linear-time match stage of queries and filters | MIT OR Apache-2.0 |
| `ratatui` (no default features; `crossterm`) | `=0.30.2` | `dreamer-termapp`: pane app views | MIT |
| `crossterm` | `=0.29.0` | `dreamer-termapp`: raw mode, alternate screen, keys | MIT |
| `rmcp` (features `server`, `macros`, `transport-io`) | `=3.10.0` | `dream mcp`: the official MCP Rust SDK, MCP up to 2026-07-28 | Apache-2.0 |

All are pure Rust except Luau (C++, built by `mlua` from its vendored
source). None pulls in a UI framework, windowing, GPU, or font crate, so
headless crates may use them (`cargo xtask headless --check`).

## GPUI

| Item | Value |
| --- | --- |
| Source | `https://github.com/zed-industries/zed` (crates `gpui`, `gpui_platform` and their platform crates) |
| Revision | `6ceb6fee8738274589b59139df27efc191ec2231` (main, 2026-11-12) |
| Why not crates.io | The published `gpui` 0.2.2 (2025-10) predates the `gpui_platform` split; upstream recommends tracking the repository |
| Features | macOS: `font-kit` (Metal renderer, glyph rasterization); Linux: `wayland`, `x11` |
| Zed toolchain at this revision | Rust 1.98.1; builds with our 1.99.1 |
| License | Apache-2.0 |

Zed's root `Cargo.toml` patches several crates that GPUI relies on. Patches
in a dependency's workspace are ignored by Cargo, so the four that GPUI's
graph uses are repeated in our `[patch.crates-io]` at zed's pinned revisions:
`async-process`, `async-task`, `calloop`, `khronos-egl`. When moving the GPUI
revision, re-copy them from zed's `Cargo.toml` at the new revision and check
that `Cargo.lock` has no `[[patch.unused]]` entries.

Only `crates/app` may depend on GPUI. Headless crates must not (see
`AGENTS.md`).

### macOS build requirements

Full Xcode with the Metal Toolchain component
(`xcodebuild -downloadComponent MetalToolchain`); `cargo xtask doctor` checks
that the Metal compiler actually runs.

### Linux build packages (Ubuntu 24.04)

`libfontconfig-dev libfreetype-dev libwayland-dev libxkbcommon-dev
libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libvulkan-dev`

## Bindings

`crates/ghostty-sys/src/bindings.rs` is generated by bindgen from the pinned
headers with `-DGHOSTTY_STATIC` and checked in. Enums are generated as
constant modules so values added by later 1.x releases stay representable.
Struct layout assertions are compile-time. CI and the local gate run
`cargo xtask bindings --check`, which fails if the headers and the checked-in
file disagree.

## Using the pins

```sh
cargo xtask deps prepare                     # host target
cargo xtask deps prepare --target <triple>   # cross targets
cargo xtask deps path                        # print the install prefix
cargo xtask bindings [--check]
```

The dependency cache is `~/Library/Caches/dreamer/deps` on macOS and
`$XDG_CACHE_HOME/dreamer/deps` (default `~/.cache/dreamer/deps`) on Linux.
Set `DREAMER_DEPS_DIR` to move it. `crates/ghostty-sys/build.rs` only reads
the cache; it never downloads or builds.

## Agent CLIs

Runtime dependencies on the user's machine, not bundled. See
[providers.md](providers.md#versions).

| CLI | Tested | Read by the adapter |
| --- | --- | --- |
| Claude Code (`claude`) | 2.1.325 | Major version 2 |
| Codex (`codex`) | Not run yet | Documented `exec --json` events |
| `curl` (API adapters only) | 8.x (macOS 8.7.1 here) | `--config -`, `--no-buffer`, `--write-out` |

pi is installed by Dreamer, not by the user, and follows its newest release
unless `agents.pi.version` pins one ([agents.md](agents.md#pi-installed-by-dreamer)).
The install was run against pi 1.5.1 and 1.5.2 (darwin-arm64); it reads
`pi --version` and `pi auth check --json`. It needs `curl` and `tar`.

## Pending pins

| Item | Pinned by |
| --- | --- |
| tmux | Baseline 3.8 (runtime dependency, not bundled); CI build of 3.8 lands with the tmux integration tests |

## Terminal color schemes

| Item | Value |
| --- | --- |
| Source | alacritty-theme, <https://github.com/alacritty/alacritty-theme> |
| Commit | `95615789c5eddee1c5407a520cd9959a86fa4716` (2026-10-29) |
| Files | 31 schemes, vendored unchanged in `crates/terminal-core/schemes/` (list and original projects in `SOURCES.md`) |
| Built into | `crates/terminal-core/src/schemes_data.rs` by `cargo xtask schemes`; `--check` runs in the gate and CI |
| License | Apache-2.0 (collection); original projects MIT, MIT/X11, or Apache-2.0 (Nord); see `THIRD_PARTY_NOTICES.md` |

## Bundled fonts

| Family | Archive (Nerd Fonts 3.5.1) | SHA-256 |
| --- | --- | --- |
| JetBrainsMono Nerd Font Mono | `JetBrainsMono.tar.xz` | `04d5e8f903693f9dd13e16f867e994834e681eb3c72c0d337a770dcda09010cf` |
| FiraCode Nerd Font Mono | `FiraCode.tar.xz` | `68e3bd6164864b8b514605bc34e3a87ac401c8c48682fcce6478c70263340207` |
| Hack Nerd Font Mono | `Hack.tar.xz` | `cdd389472e10e2261520140ff1b382b4f8a226af5fd0b2735b975d31151d9c3c` |
| CaskaydiaCove Nerd Font Mono | `CascadiaCode.tar.xz` | `ae598e9401e2846aa3ee364513715de0490b844a4b54f0768991f45f23aa8369` |
| SauceCodePro Nerd Font Mono | `SourceCodePro.tar.xz` | `50ead7bb7addce27cf4b55f4fffb669b5f1899fece675fad623a2a75a478c8c7` |

Pinned in `deps/baseline.toml`. `cargo xtask deps fonts` fetches them and
checks that each file's `name` table carries the family settings use.
