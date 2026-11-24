# AGENTS.md

Instructions for any coding agent working in this repository.

## What this is

Dreamer is a native terminal for macOS (Apple Silicon) and Linux, written in Rust. Each real tmux pane is drawn as its own GUI surface. A headless server keeps working when the GUI closes, and panes can host scripted apps and agents that watch, query, and filter other terminals.

| Executable | Role |
| --- | --- |
| `dreamer` | Desktop client and renderer (GPUI) |
| `dreamerd` | Headless server: tmux control, terminal mirrors, jobs, extensions |
| `dream` | CLI, SSH stdio bridge, MCP adapter |
| `dreamer-termapp` | Runtime for apps that run inside tmux panes |

## Where the plan lives

- **Work items:** GitHub issues, grouped by milestones M0 to M8. Titles start with the ticket ID, for example `[M0-01]`. Work in milestone order and respect each issue's "Depends on" line.
- **Specification:** the maintainers keep the design specification outside this repository. An issue states what it needs; if it refers to something you cannot see, ask on the issue rather than guessing.
- **`docs/` is for documentation of the project as built:** how to build, run, configure, and extend it, plus compatibility tables. It is user and developer documentation only. Do not put specs, plans, ticket lists, design explorations, architecture decision records, or other process documents in the repository, and do not link to them from it. The maintainers keep those outside the repository.

## Architecture rules

These are settled. Changing one needs a written decision record, kept outside the repository, and the owner's agreement first. Open an issue to propose one.

- tmux owns processes, sessions, windows, panes, and layout. Dreamer drives it through control mode (`tmux -C`) and never becomes a second multiplexer.
- Terminal semantics come from the public `libghostty-vt` C API at an exact pinned release. No private Ghostty APIs. The renderer is original code in this repository.
- `unsafe` is confined to the FFI crate, its safe wrapper, and unavoidable platform adapters.
- Headless crates (server, CLI, pane-app runtime) must build with no GPUI, font, windowing, or GPU dependency.
- The GUI thread never blocks on I/O, parses terminal streams, decodes images, runs scripts, or calls models.
- Terminal bytes are ordered and never silently dropped. Only superseded render frames may be coalesced.
- UI code never builds tmux command strings. Use the typed backend operations and the single argument encoder.
- Extensions use the same typed API as the GUI, under grants. Filters, triggers, and daemon scripts are sandboxed Luau. App logic is TypeScript, run sandboxed in the app host's embedded engine (QuickJS) with only the typed API its grant allows; types are stripped when an app is checked or installed. Heavy or OS-facing work runs as a supervised process. Scripts, filters, and collectors never modify what a process wrote or what a normal tmux client sees.
- Apps describe their UI as a tree of host components that the desktop draws natively and the app host draws as a TUI. App code never runs in the desktop process and never touches GPUI or the OS directly.
- Agent harnesses are configuration (`agents:` in `dreamer.yaml`): pi by default, installed and kept at its latest release by Dreamer in Dreamer's own data directory, run with Dreamer's pi profile; Claude Code, Codex, or any ACP agent when configured. Every harness uses its own login. Never fall back to a metered API key automatically, and never read a harness's credential store.
- Every dependency that matters (Rust toolchain, GPUI, Ghostty, Zig, tmux baseline) is pinned and recorded in `docs/dependency-baseline.md`. Builds use `--locked` and do not download changing sources.

## Verification

There are two lanes, and both must pass.

- **Linux:** GitHub Actions and the Mac's container stage. Every pull request runs one x86-64 job (`.github/workflows/ci.yml`): fmt, policy, the `--check` commands, clippy, build, and tests. The slower Linux checks (remote acceptance through real OpenSSH, the ARM64 cross-build under emulation, the Arch package, the tarball through `install.sh`) are in `.github/workflows/full.yml`, which runs weekly, on `v*` tags, and on demand. Before every push, the Mac's full gate runs clippy, the headless tests, and remote acceptance on ARM64 Linux in a container. Never add a `macos-*` runner to a workflow.
- **macOS:** a pre-commit hook on the development Mac runs `cargo xtask gate mac --staged` against the staged tree; pre-push runs the full gate on `HEAD`, reusing the stages a pre-commit run passed for the same tree. Install hooks with `cargo xtask hooks install`.

Rules:

- Never bypass hooks with `--no-verify`. If the gate fails, fix the cause or report it.
- Tests use isolated tmux servers on temporary sockets. Never attach to, reconfigure, or kill the user's own tmux server, and never edit their tmux or SSH configuration.
- Tests that start `dreamerd` give it private `DREAMER_RUNTIME_DIR` and `DREAMER_STATE_DIR`, an isolated `--tmux-socket`, and `DREAMER_FORBID_DEFAULT_TMUX=1`; anything a test spawns (including through `ensure_running`) must receive the same environment. Never let a test daemon use the user's runtime or state directories.
- A compile or a test run is not proof that a window rendered. Keep GUI evidence separate for macOS, Wayland, and X11.

The build interface (`cargo xtask doctor`, `deps prepare`, `gate`, `package`, `build-report`) is created during M0. Until an issue has added a command, it does not exist; check before relying on it.

## Git and pull requests

- Never commit to `main`. Confirm the branch with `git branch --show-current` before each commit.
- One branch and one pull request per issue. Name the branch after the ticket, for example `m0-01-workspace-scaffold`, and reference the issue in the pull request.
- Before pushing to an existing branch, check whether its pull request is already merged. If it is, branch from the latest `main` and open a new one.
- Write commit messages and pull request descriptions as a human author would. Do not add AI attribution, co-author trailers, or "generated with" footers.
- Commit or push only when asked.

## Reporting

- Say what was built, what ran, what passed, and what was not run. Never claim a build, test, benchmark, or platform result without evidence from this revision.
- State supported subsets precisely. If an upstream limitation blocks a requirement, write up the evidence and the options on the issue instead of substituting a weaker feature.
- Performance claims need numbers from the benchmark harness on named hardware.

## Style

- Keep the default UI minimal: terminal content first, chrome on demand.
- Prefer small crates with clear dependency direction over shared utility modules.
- Add a test when it pins product behavior or a hard correctness property. Do not add tests that only restate the implementation.
