# Agent harnesses

An agent harness is the program an agent session runs: pi, Claude Code,
Codex, or another agent that speaks the Agent Client Protocol (ACP).
Which one a new session uses is configuration (`agents:` in
`dreamer.yaml`). A session that is already running keeps the harness it
started with.

| Harness | Program | Installed by |
| --- | --- | --- |
| `pi` (default) | pi | Dreamer, under its data directory |
| `claude` | Claude Code (`claude`) | You; found on `PATH` |
| `codex` | Codex (`codex`) | You; found on `PATH` |
| any other name | the `command` you give, with `protocol: acp` | You |

Every harness uses its own login. Dreamer never reads a harness's
credentials and never gives it an API key; see
[providers.md](providers.md#rules).

## Settings

```yaml
agents:
  default: pi                 # pi (default), claude, codex, or a name below
  pi:
    version: latest           # latest (default), or a release such as 1.5.2
    model: openai/gpt-5.5     # optional; else pi's own default
    agent_dir: ~/.pi/agent    # optional; else Dreamer's pi profile
    command: [pi]             # optional; run this pi instead of Dreamer's install
  claude:
    command: [claude]         # default
    model: opus               # optional
  codex: {}
  gemini:                     # any name: letters, digits, - and _
    protocol: acp             # required for a custom harness
    command: [gemini, --experimental-acp]
```

- `default` must name `pi`, `claude`, `codex`, or a block under `agents`.
- `version` and `agent_dir` apply to pi only. `protocol` applies to custom
  harnesses only, and a custom harness needs both `protocol: acp` and a
  `command`.
- `command` is a program and its arguments. A bare program name is looked
  up on `PATH`.
- An unknown key, or a key that does not apply to that harness, is
  refused with its key path (for example `agents.pi.modle`), and the
  settings in force stay.

`dream config schema` lists every key. `dream config check` also reports
each harness: whether its program is there, and its login state by the
harness's own status command (`pi auth check`, `claude auth status`,
`codex login status`). A custom ACP agent reports its login when a session
starts. These lines are information; they do not change the exit status.

## pi, installed by Dreamer

Dreamer installs pi itself, in its data directory, never in your global
npm packages or `~/.pi`:

```text
~/.local/share/dreamer/agents/pi/
  1.5.1/pi           an unpacked release
  1.5.2/pi
  current -> 1.5.2   the active release
  previous -> 1.5.1  kept for rollback
```

The data directory is `DREAMER_DATA_DIR`, then `$XDG_DATA_HOME/dreamer`,
then `~/.local/share/dreamer`, on macOS and Linux alike. Sessions run
`agents/pi/current/pi`, so the path stays the same across updates.

- **What is downloaded.** pi publishes a standalone executable for each
  platform with every GitHub release (`pi-darwin-arm64.tar.gz`,
  `pi-darwin-x64.tar.gz`, `pi-linux-arm64.tar.gz`, `pi-linux-x64.tar.gz`),
  so no Node.js is needed. The newest release number comes from
  `https://pi.dev/api/installer/releases/latest`, as pi's own installer
  reads it.
- **Checked before use.** The archive must match its line in the
  release's `SHA256SUMS`, and the unpacked `pi --version` must report the
  release that was asked for. Otherwise nothing changes.
- **Two releases are kept:** the active one and the one before it. Older
  ones are removed after an update.
- **One update at a time.** A second `dream agents update` while one runs
  is refused.
- **Needs** `curl` and `tar`, which macOS and common Linux systems have.
- **Mirrors.** `DREAMER_PI_LATEST_URL` replaces the release metadata
  address, and `DREAMER_PI_DOWNLOAD_URL` the download base
  (`<base>/v<version>/<asset>`).

### Commands

| Command | What it does |
| --- | --- |
| `dream agents status [--json]` | Installed releases, the active and kept ones, the version setting, whether `pi --version` runs, the agent directory, and the login state per provider |
| `dream agents update [--version X.Y.Z]` | Install the release (with no `--version`: `agents.pi.version`, else the newest), make it active, and keep the one before it |
| `dream agents rollback` | Make the kept release active again. A second rollback returns to the other |

Each takes `--harness pi`; it is the only harness Dreamer installs.

### Login

pi keeps its own logins in its agent directory. Dreamer's pi sessions use
Dreamer's pi profile, `<data dir>/pi-profile`, unless `agents.pi.agent_dir`
names another, such as your own `~/.pi/agent`.

`dream agents status` asks pi itself, with
`pi auth check --provider P --no-refresh --json`, for the `openai` (a
ChatGPT subscription signs in here) and `anthropic` providers. API key
variables in the environment are left out of that check, so only pi's own
login counts. When none is ready, it prints the command that logs in:

```sh
PI_CODING_AGENT_DIR=~/.local/share/dreamer/pi-profile \
  ~/.local/share/dreamer/agents/pi/current/pi auth login openai
```

or run `/login` inside pi.

## Not yet

- The daily check for a newer pi release; `dream agents update` runs it
  by hand.
- Starting sessions with the configured harness. The settings are parsed
  and checked, and take effect for sessions started from them.
