---
name: dreamer-manage
description: "Install, update, check, and operate the Dreamer terminal for the user: install or update Dreamer, see whether its daemon is healthy and fix it when it is not, inspect sessions and panes, start long-running jobs, connect remote hosts over SSH and install the server on them, set up the web-pane component, and connect Claude Code or Codex to Dreamer's MCP server. Use when the user asks to install, update, or uninstall Dreamer, says Dreamer or dreamerd is not working, or wants a host, a job, or an agent connection set up. For settings and themes use dreamer-config; for building pane apps use dreamer-apps."
---

# Managing Dreamer

Dreamer is a terminal built on tmux. Four programs make it up: `dreamer`
(the desktop app), `dreamerd` (a per-user daemon that drives tmux and
starts on demand), `dream` (the command line), and `dreamer-termapp` (the
runtime for apps in panes). You manage all of it through `dream`.

tmux owns the user's sessions. Stopping, updating, or reinstalling Dreamer
never ends them. Never kill the user's tmux server, and never edit their
tmux or SSH configuration.

## Install, update, uninstall

Install the latest release (macOS 15+ on Apple Silicon, or Linux x86-64 /
ARM64 with glibc 2.39+). It needs no root and checks the download's
SHA-256:

```sh
curl -fsSL https://github.com/probablyrobot/dreamer/releases/latest/download/install.sh | sh
```

Dreamer needs tmux, which the script does not install. If `tmux` is
missing, tell the user and offer the command for their system
(`brew install tmux`, `sudo apt install tmux`, `sudo dnf install tmux`).

If the script says `~/.local/bin` is not on `PATH`, show the user the line
it printed. Do not edit their shell profile without asking.

Update:

```sh
dream update --check     # is there a newer release?
dream update             # install it over this one
```

`dream update` refuses when a package manager installed Dreamer and says
which package to get. After an update the user quits and reopens Dreamer.

Uninstall (settings and tmux sessions are kept):

```sh
curl -fsSL https://github.com/probablyrobot/dreamer/releases/latest/download/install.sh | sh -s -- --uninstall
```

## Is it healthy?

Run these in order and read the output before changing anything.

| Command | What it tells you |
| --- | --- |
| `dream --version` | The client's version. Works with no daemon. |
| `dream doctor` | Directories, permissions, the lock, the ledger, the daemon. Never starts the daemon. |
| `dream status` | The daemon's process, socket, log file, clients, and its link to tmux. |
| `dream version` | Client and daemon versions. After an update they should be the same. |
| `dream config check` | Whether every settings file is valid. |

Common fixes:

- **The daemon is not running.** `dream ensure-running` starts it. Most
  commands start it themselves.
- **Client and daemon versions differ** (after an update). `dream shutdown`
  stops the old daemon; the next command starts the new one. tmux keeps
  running.
- **Something is stuck.** The log file is named in `dream status`. Read its
  last lines before restarting anything.
- **A settings file is rejected.** `dream config check` names the file, the
  key, and the problem. The dreamer-config skill covers fixing it.

To keep the daemon running across logins, install it as a per-user service
(systemd on Linux, launchd on macOS): `dream service install`, and check it
with `dream service status`. This is optional.

## Sessions and panes

```sh
dream ls                     # sessions, windows, panes (add --json to parse it)
dream read %3                # the text of pane %3, from the daemon's mirror
dream read %3 --history 200  # with scrollback
dream act split ...          # one typed change to tmux: split, resize, select, zoom, rename, send, keys, kill
dream actions                # what was changed, and whether it worked
```

Use `dream act` for changes. Do not run raw tmux commands against the
user's server when a `dream` command exists for it.

A workspace file describes a whole session. `dream workspace list` shows
them, `dream workspace check NAME` validates one, and `dream workspace up
NAME` creates its session.

## Long-running work

A job is a program in a tmux window of its own that outlives the terminal
and the agent that started it:

```sh
dream jobs start --name build --cwd ~/src/project -- cargo build --release
dream jobs                   # every job and its state
dream jobs cancel ID
```

## Remote hosts

Dreamer reaches other machines only through the user's own SSH. A host
must already work with `ssh NAME` and key authentication; Dreamer never
asks for passwords.

```sh
dream hosts connect PROFILE  # a profile from hosts.yaml
dream hosts connect NAME DESTINATION   # or name an SSH destination (user@host, or an SSH config alias)
dream hosts list             # known hosts and their state
dream hosts install NAME     # put the Dreamer server on that host (shows a plan, asks first)
dream hosts upgrade NAME     # move it to this version
dream hosts disconnect NAME
```

Connect first; `install` and `verify` work on a host the daemon knows.
Host profiles live in `hosts.yaml` in the configuration directory.

## Web pages in panes

Web panes need Dreamer Web, a Chromium component installed apart from the
app (macOS only for now):

```sh
dream web status
dream web install
dream web open https://example.com
```

## Connect an agent to Dreamer

Dreamer's MCP server lets an agent read panes, run commands in new panes,
watch for output, and arrange the layout. Register it once:

```sh
claude mcp add dreamer -- dream mcp --level elsewhere
codex mcp add dreamer -- dream mcp --level elsewhere
```

The level is the agent's limit:

| Level | The agent may |
| --- | --- |
| `observe` | Read, search, and watch. Never act. |
| `own` | Also act in panes it created. |
| `elsewhere` | Also act in the user's panes, after the user approves each action. |

Recommend `elsewhere` or lower. `dream mcp` with no level holds the user's
full authority and asks for nothing; do not set that up unless the user
asks for it in those words.

When an action waits for approval, the user answers with `dream approvals`
(list), `dream approvals allow ID`, or `dream approvals deny ID`. Do not
approve your own requests.

`dream skills install` writes Dreamer's skills for Claude Code and Codex.

## Agents inside Dreamer

`dream agents status` shows the pi agent Dreamer manages, and `dream agents
update` installs or updates it. Every agent uses its own login. Never read
an agent's credential files, and never set an API key to work around a
missing login: tell the user to log in with that agent's own command.

## Rules

- Look before you change: `dream doctor` and `dream status` first.
- Ask before anything that removes data or touches another machine:
  uninstalling, `dream hosts install`, deleting a workspace file.
- Never kill or reconfigure the user's tmux server.
- If a command's output disagrees with this skill, trust the command. A
  wrong invocation prints the usage.
