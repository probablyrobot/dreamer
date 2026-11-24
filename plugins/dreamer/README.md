# Dreamer plugin for Claude Code and Codex

Lets your coding agent work with the [Dreamer](https://github.com/probablyrobot/dreamer)
terminal:

- **Three skills.** `dreamer-config` changes settings, themes, hosts, and
  workspaces. `dreamer-manage` installs, updates, and checks Dreamer.
  `dreamer-apps` builds apps that live in panes.
- **An MCP server.** `dream mcp --level elsewhere` gives the agent tools to
  read panes, run commands in new panes, watch for output, and arrange the
  layout. Anything it does in a pane it did not create waits for your
  approval.

Install Dreamer first; the plugin runs the `dream` on your `PATH`.

Claude Code:

```sh
claude plugin marketplace add probablyrobot/dreamer
claude plugin install dreamer@dreamer
```

Codex:

```sh
codex plugin marketplace add probablyrobot/dreamer
```

Then install `dreamer` from `/plugins`.

To change what the agent may do, edit the `--level` in `.mcp.json`
(Claude Code) or `mcp.json` (Codex): `observe`, `own`, or `elsewhere`. See
[the MCP page](https://github.com/probablyrobot/dreamer/blob/main/docs/mcp.md).

The skills here are copies of the ones `dream skills install` writes.
`cargo xtask plugin` refreshes them from the source.
