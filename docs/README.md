# Dreamer documentation

Dreamer is four programs:

| Program | What it is |
| --- | --- |
| `dreamer` | The desktop app. It draws each tmux pane as its own surface. |
| `dreamerd` | The daemon. It drives tmux, mirrors every pane, and runs agents, jobs, and extensions. It starts on demand and keeps working when the window is closed. |
| `dream` | The command line. Everything the desktop does, without a window. |
| `dreamer-termapp` | The runtime for apps that live in panes. |

New here? Read [Installing Dreamer](install.md), then
[the desktop app](desktop.md).

## Using Dreamer

| Page | What it covers |
| --- | --- |
| [Installing Dreamer](install.md) | Install, update, uninstall, packages for each system |
| [The desktop app](desktop.md) | Panes, tabs, windows, keys, the palette, find, web panes |
| [Settings](settings.md) | Every key in `dreamer.yaml`: fonts, themes, links, clipboard |
| [Input](input.md) | Keys, mouse, selection, copy and paste, scrolling |
| [Workspaces](workspaces.md) | Describe a tmux session in a file and bring it up |
| [dream](cli.md) | The command-line interface |
| [Remote hosts](remote.md) | Work on another machine over SSH |
| [Images](images.md) | Images in the terminal (Kitty graphics protocol) |
| [Terminal compatibility](terminal-compatibility.md) | Which terminal features work, and how each is tested |

## Agents

| Page | What it covers |
| --- | --- |
| [Agents](agents.md) | Run pi, Claude Code, Codex, or an ACP agent in a pane |
| [MCP](mcp.md) | Let an agent read and drive your panes through `dream mcp` |
| [Grants](grants.md) | What a connection may do, and how approvals work |
| [Memory](memory.md) | Facts kept across agent sessions |
| [Agent providers](providers.md) | How Dreamer reaches a model through the CLIs you have |
| [Processes and agent runs](processes.md) | Which agents are running where |

## Automating and extending

| Page | What it covers |
| --- | --- |
| [Filters](filters.md) | Act on lines as they appear in panes |
| [Collectors](collectors.md) | Turn terminal output into rows you can query |
| [Scripts](scripts.md) | Small sandboxed Luau programs inside the daemon |
| [Jobs](jobs.md) | Long-running programs in their own tmux windows |
| [Pane apps](termapp.md) | Apps in panes: manifests, TypeScript, components |
| [The app process protocol](app-protocol.md) | Apps that draw their own view, and the Rust SDK |
| [Extensions](extensions.md) | Programs the daemon supervises, in Rust or any language |

## Reference

For people writing a client, an extension, or Dreamer itself.

| Page | What it covers |
| --- | --- |
| [dreamerd](daemon.md) | The daemon: lifecycle, socket, logs, service |
| [API schema](api.md) | The typed API every client uses |
| [Wire protocol](wire.md) | How clients talk to the daemon |
| [Topology](topology.md) | Sessions, windows, and panes as the daemon reports them |
| [Actions](actions.md) | Typed changes to tmux, and the action ledger |
| [Events](events.md) | The log of what changed |
| [Messages](messages.md) | Structured messages between connections |
| [Effects and replies](effects.md) | Titles, bells, clipboard writes, and query replies |
| [Pane mirrors](mirrors.md) | The daemon's copy of every pane's terminal state |
| [Taps](taps.md) | Bounded views of panes for monitors and agents |
| [tmux cold attach](tmux-bootstrap.md) | How a mirror starts from a pane already running |

## Developing Dreamer

| Page | What it covers |
| --- | --- |
| [Building Dreamer](building.md) | Prerequisites, build, tests, the repository layout |
| [Packaging](packaging.md) | Building packages, signing, and making a release |
| [Dependency baseline](dependency-baseline.md) | Every pinned toolchain and native dependency |
| [Design](design.md) | The look: colors, type, spacing, reference scenes |
| [Desktop internals](desktop-internals.md) | Frame scheduling, the renderer, and window checks |
| [Linux desktop gate](linux-desktop-gate.md) | Evidence that windows render on a real Linux display |
| [Performance](performance.md) | Measured results from the benchmark harness |
