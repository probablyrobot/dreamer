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
| `png` | `=0.18.1` (pure Rust) | `terminal-ghostty`: Ghostty's PNG decode callback for Kitty images; `dreamer-app`: cropping a pane's capture for `dream app preview --png` | MIT OR Apache-2.0 |
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
| `rquickjs` (default features off; `std`) | `=0.14.0`, with its vendored QuickJS (`rquickjs-sys` 0.14.0) | `app-script`: script apps (`app.ts`) in `dreamer-termapp` | MIT (QuickJS: MIT) |
| `swc_ts_fast_strip` | `=60.0.1` (with `swc_common` 27.0.0 and the swc parser crates it pins) | `dream app check`, `preview`, `install`: strips an `app.ts`'s types | Apache-2.0 |
| `swc_ecma_parser` (default features off; `typescript`), `swc_ecma_ast`, `swc_ecma_visit`, `swc_ecma_transforms_base` | `=47.0.2`, `=30.0.2`, `=30.0.0`, `=51.0.3`: the versions `swc_ts_fast_strip` 60.0.1 already pins | `dream app check`, `install`: parse an `app.ts` and check its uses of `dreamer`, `ui`, and `app` | Apache-2.0 |

All are pure Rust except Luau (C++, built by `mlua` from its vendored
source) and QuickJS (C, built by `rquickjs-sys` with the `cc` crate from
its vendored source, using the bindings it ships for each of our three
targets, so no `bindgen` or libclang). None pulls in a UI framework,
windowing, GPU, or font crate, so headless crates may use them (`cargo
xtask headless --check`).

Why these two: QuickJS through `rquickjs` was chosen over Boa for a
script app's engine because it has an interrupt handler (a time limit
per call, where Boa has only a loop-iteration cap) and a memory limit,
starts a context in under a millisecond, and adds about 1.5 MB to a
stripped binary. Only `dreamer-termapp` links it; the server and the
desktop do not. `swc_ts_fast_strip` in strip-only mode replaces types
with spaces, so positions in the JavaScript are positions in the
TypeScript without a source map; it lives in the CLI only, so the app
host carries no TypeScript tooling.

## HTTP client (`fetch` in script apps)

| Crate | Version | Used by | License |
| --- | --- | --- | --- |
| `ureq` (default features off; `rustls`) | `=3.5.2`, with `ureq-proto` 0.7.1 | `app-script`: `fetch` in script apps, in `dreamer-termapp`; `dreamer-server` (with the `kosmonaut` feature): the `openai` memory embedder | MIT OR Apache-2.0 |
| `rustls` (through `ureq`, provider `ring`) | `0.23.46`, with `rustls-webpki` 0.103.17 and `rustls-pki-types` 1.16.0 | same | Apache-2.0 OR ISC OR MIT (`rustls-webpki`: ISC) |
| `ring` | `0.17.14` | same: rustls's cryptography | Apache-2.0 AND ISC |
| `webpki-roots` | `1.0.12` | same: the root certificates | CDLA-Permissive-2.0 |

All four were already in `Cargo.lock` (CEF's build tooling resolves the
same versions), so adding `fetch` added no new package to the lock.
`ureq` is pinned exactly; the others are pinned by the lock and move only
with it.

Why these:

- **A small blocking client, off the app's thread.** A script app's
  engine runs on a thread of its own with a single-threaded Tokio runtime
  for its timers. `fetch` sends each request on a thread of its own and
  hands the answer back to the engine, as a call's answer comes back
  from the daemon, so neither a slow server nor the client's I/O can
  hold up the app or count against its 250 ms per entry. A blocking
  client fits that with no second async runtime: `ureq` is HTTP/1.1 only,
  has a timeout for the whole exchange (`timeout_global`, which covers
  connecting, redirects as we follow them, and reading the body), a body
  size limit, and can stop following redirects (`max_redirects(0)`) so
  `fetch` checks each one against the manifest's hosts before connecting.
  `reqwest` and `hyper` would bring a multi-threaded runtime, HTTP/2, and
  several times the code for features `fetch` does not offer.
- **rustls with `ring`.** Pure Rust TLS with a small amount of C and
  assembly in `ring`, built by the `cc` crate with the C compiler every
  target already needs (QuickJS and Luau are built the same way). The
  other rustls provider, `aws-lc-rs`, builds a much larger C library and
  can need CMake (and, for some configurations, NASM or Go), which the
  headless builds, the ARM64 Linux container included, do not otherwise
  need. No OpenSSL or system TLS library is linked, so the
  macOS and Linux builds behave the same.
- **`webpki-roots`, not the platform verifier.** The roots are Mozilla's
  set, compiled in: the same on every machine, with nothing read from the
  system at run time and no Security.framework or OpenSSL probing.
  The cost is that a certificate from a private or corporate CA that the
  system trusts is refused, and the roots move only when Dreamer is
  updated (`webpki-roots` is part of the lock). `rustls-platform-verifier`
  would accept those CAs but adds a platform-specific dependency tree per
  target; it can be switched to if apps need it.

The server uses the same client, at the same pin and features, for the
`openai` memory embedder (`memory.kosmonaut.embedder.kind: openai`,
[memory.md](memory.md#openai)): one blocking POST per batch of texts, on
the thread a memory call already runs on off the event loop, with
`timeout_global` for each request, no redirects, and no proxy. It is an
optional dependency that the `kosmonaut` feature turns on, so a server
built without that feature does not link it. Adding it changed only the
server's own dependency list in `Cargo.lock`.

`cargo xtask policy` allows `ureq` in two product files and nowhere else:
`crates/app-script/src/fetch.rs` (app `fetch`, to the hosts a manifest
lists) and `crates/server/src/memories/openai.rs` (the embedder, to the
API base its settings name: `https`, or `http` to this machine only).
Neither opens sockets of its own. None of these crates pulls in a UI
framework, GPU, or font crate (`cargo xtask headless --check`).
`dreamer-termapp` links them, and so does `dreamerd` built with the
`kosmonaut` feature; the CLI and the desktop do not.

## Kosmonaut

| Item | Value |
| --- | --- |
| Source | `https://github.com/DeytaHQ/kosmonaut` (private; crate `kosmonaut` and its `ram-*` crates) |
| Tag | `v0.24.0` |
| Commit | `dbd5af275a7efa3f2ff3ca45afc45b40a214430e` (recorded in `Cargo.lock`) |
| Used by | `memory` and `dreamer-server`, behind their `kosmonaut` feature: the embedded `kosmonaut` memory provider ([memory.md](memory.md#kosmonaut)) |
| Server | `kosmonaut-server` from the same tag, built with its `grpc` feature: what [server mode](memory.md#server-mode) talks to, and what CI runs the memory tests against |
| Kosmonaut toolchain | Rust 1.95 at least (its own pin 1.98.1); builds with our 1.99.1 |
| Features | Default: no cloud backends, no Arrow; `object_store` with the local file system only |
| License | Apache-2.0. What it adds to `Cargo.lock` (Tantivy, `object_store`, zstd, and others) is MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, Unlicense, or zlib, alone or as choices |

Not in release archives: the feature is off by default, and packaging
builds without it. It is pure Rust apart from zstd (C, built by
`zstd-sys` with the `cc` crate), and pulls in no UI, windowing, GPU, or
font crate, so the headless crates may use it.

**Access.** The repository is private, and Cargo fetches every git
source in `Cargo.lock` (optional ones too) before it builds anything, so
every build needs read access to it, with or without the feature:

- On a development machine, the owner's own git login. Set
  `CARGO_NET_GIT_FETCH_WITH_CLI=true` so Cargo fetches through `git` and
  its credential helper (for example `gh auth setup-git`). Dreamer never
  reads a credential store itself.
- In GitHub Actions, the `KOSMONAUT_TOKEN` secret (a fine-grained,
  read-only token): each job rewrites `https://github.com/DeytaHQ/` to an
  address that carries it, before any `cargo` command, and sets
  `CARGO_NET_GIT_FETCH_WITH_CLI`.
- In the containers the gate, `cargo xtask package --container`, and
  `cargo xtask bench` run, which have no GitHub login: the host runs
  `cargo fetch --locked`, copies the locked commit from its cargo cache
  into a bare mirror under `target/private-git/`, and mounts it read-only;
  git in the container reads the repository's URL from the mirror
  (`url.<mirror>.insteadOf`). No token enters a container
  (`xtask/src/private_git.rs`).

## gRPC client (Kosmonaut server mode)

| Crate | Version | Used by | License |
| --- | --- | --- | --- |
| `tonic` (default features off; `channel`, `codegen`, `tls-ring`, `tls-webpki-roots`) | `0.14.9` | `memory` (with the `kosmonaut` feature): the Kosmonaut provider's `mode: server` | MIT |
| `tonic-prost` | `0.14.9` | same: the protobuf codec | MIT |
| `prost` | `0.14.5` | same: the messages | Apache-2.0 |
| `hyper`, `hyper-util`, `h2`, `tower`, `tokio-rustls`, `tokio-stream`, and their small helpers (`http-body-util`, `hyper-timeout`, `sync_wrapper`, `tower-layer`, `tower-service`, `try-lock`, `want`) | as `Cargo.lock` records them | same: through `tonic` | MIT, or MIT OR Apache-2.0 (`sync_wrapper`: Apache-2.0) |

`prost`, `rustls` (provider `ring`), `webpki-roots`, and Tokio were
already in the lock; the rest are new, and only a build with the
`kosmonaut` feature links them.

Why these:

- **gRPC is Kosmonaut's server protocol** at the pinned release, next to
  an HTTP JSON API with the same statements and sessions. The design
  names gRPC, and `tonic` is the client Kosmonaut's own server is built
  on, at the same major version.
- **No code generation.** The handful of messages Dreamer uses
  (`QueryRequest`, `QueryResponse`, the session and table-listing
  messages) are written out with `prost`'s derive, with the field
  numbers of Kosmonaut's `.proto` files, so the build runs no `protoc`
  and has no build script. Fields Dreamer does not use are skipped when a
  reply is decoded.
- **TLS as for the HTTP client**: rustls with `ring` and the
  `webpki-roots` certificates, so nothing is read from the system and no
  C library beyond `ring` is built.
- **One small runtime.** Memory calls are synchronous and run on the
  daemon's blocking threads; the client keeps a one-thread Tokio runtime
  of its own and each call waits for its answer, bounded by a timeout.

`cargo xtask policy` allows `tonic` in one product file,
`crates/memory/src/kosmonaut/server.rs`.

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

## Web engine

Web panes on the desktop ([desktop.md](desktop.md#web-panes)) embed
Chromium through the Chromium Embedded Framework (CEF). Only `dreamer`
built with its `web` feature uses it, and it links only CEF's C++ wrapper:
the framework itself is loaded at run time from the Dreamer Web component
([packaging.md](packaging.md#dreamer-web)), which ships apart from the
app. The macOS release package builds `dreamer` with the feature; the
default build and the Linux CI build do not. `cargo xtask headless
--check` fails if `dreamerd`, `dream`, `dreamer-termapp`, or any library
of theirs could depend on `cef`, `cef-dll-sys`, `cef-sys`, or
`web-engine`. (`web-component`, which describes the component for `dream
web install` and the desktop, holds no Chromium and is headless.)

| Item | Pin |
| --- | --- |
| Bindings | `cef` crate (tauri-apps/cef-rs) `=156.6.0`, with `cef-dll-sys` 156.6.0; default features off (no sandbox, no bundler, no resources) |
| CEF | 156.0.30 (`+g6028375`), Chromium 156.0.8078.96 |
| Distribution (macOS arm64) | `cef_binary_156.0.30+g6028375+chromium-156.0.8078.96_macosarm64_minimal.tar.bz2` from `cef-builds.spotifycdn.com`, SHA-256 `731b599e78c71bf33530d65bfa18963f7d1d59902c1f381f5452188e5116b855` (SHA-1 `6393d8a838d3b29f6b6939b970b5931dde2082aa`, as the CDN's index lists it) |
| cmake | 4.4.5, `cmake-4.4.5-macos-universal.tar.gz` from Kitware's GitHub release, SHA-256 `679e77a96b9ef9484cc6c1445255fc4f84206b48a65420efbedf16bdab55fbff` |
| Ninja | 1.13.2, `ninja-mac.zip` from the ninja-build GitHub release, SHA-256 `c99048673aa765960a99cf10c6ddb9f1fad506099ff0a0e137ad8960a88f321b` |
| License | CEF: BSD-3-Clause; Chromium's own third-party notices are `CREDITS.html` in the distribution |

`cargo xtask deps web` downloads these into the dependency cache, verifies
them, and lays CEF out where the `cef-dll-sys` build script looks for it
(`<cache>/cef/156.0.30/cef_macos_aarch64`, with the `archive.json` that
script checks the version against). Builds with the feature get that
directory as `CEF_PATH` and cmake and Ninja first on `PATH`; the build
script then compiles CEF's C++ wrapper with them. That build script would
otherwise download CEF from a changing index: `.cargo/config.toml` points
`CEF_DOWNLOAD_URL` at an address that cannot resolve, so a missing or
wrong `CEF_PATH` fails the build instead. The archive is 144 MB and the
unpacked distribution 351 MB, most of it the framework.

### Chromium updates

Chrome's stable channel gets a new major version every four weeks, and
security fixes between them, often weekly; CEF publishes a release for
each. Web panes show arbitrary sites, so the pin follows:

- **Each new Chromium stable major:** move to the newest CEF release for
  it, within its first two weeks.
- **A Chromium security fix rated High or Critical** in a stable update
  the pin does not have yet: move to the CEF release that carries it,
  within a week.

Moving the pin is one change:

1. The `[cef]` section of `deps/baseline.toml` (version, Chromium version,
   URL, name, SHA-256, and the CDN's SHA-1) and this section's table.
2. The `cef` crate in `crates/cef-sys/Cargo.toml`: cef-rs publishes one
   release per CEF release, its build metadata naming it
   (`=156.6.0` is `156.6.0+156.0.30`).
3. `web_component::CEF_VERSION` in `crates/web-component/src/lib.rs`. The
   engine does not compile while it differs from the version string of
   the `cef` crate's bindings, and `cargo xtask package` refuses a
   component whose version is not the one `deps/baseline.toml` pins.
4. `cargo xtask deps web`, then the macOS gate. Its `web` stage is the
   gate stage for Chromium: clippy with the `web` feature; the app
   bundled without Chromium and the component built beside it (manifest,
   signatures, a helper loading the framework); the component added with
   `dream web install` to an empty data directory; and pages drawn with
   it in a real window. Its `package` stage builds the release's two zips
   and adds the component from its zip away from the tree.

A Dreamer release ships its app and its Dreamer Web together, at the
same version, and Dreamer Web is only usable with the Dreamer of that
version. Users of a release with a new pin run `dream web install` once
after updating.

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
cargo xtask deps web                         # CEF, cmake, and Ninja for web panes (macOS arm64)
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
