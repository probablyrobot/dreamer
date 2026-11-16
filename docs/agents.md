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
`pi auth check --provider P --no-refresh --json`, for the `openai`,
`openai-codex`, and `anthropic` providers. API key variables in the
environment are left out of that check, so only pi's own login counts.
When none is ready, it prints the command that logs in:

```sh
PI_CODING_AGENT_DIR=~/.local/share/dreamer/pi-profile \
  ~/.local/share/dreamer/agents/pi/current/pi auth login openai
```

or run `/login` inside pi.

What pi's `/login` offers for subscriptions, in pi 1.5.2:

| pi provider | `/login` entry | Subscription |
| --- | --- | --- |
| `openai` | OpenAI, "Sign in with ChatGPT" | ChatGPT; pi's preferred way to use OpenAI's models on a subscription |
| `openai-codex` | OpenAI Codex ("OpenAI (ChatGPT Plus/Pro)") | ChatGPT Plus or Pro, through the Codex backend |
| `anthropic` | Anthropic ("Claude Pro/Max") | Claude Pro or Max |

`openai` and `anthropic` also take API keys; `dream agents pi` does not
pass `OPENAI_API_KEY`, `CODEX_API_KEY`, `ANTHROPIC_API_KEY`, or
`ANTHROPIC_AUTH_TOKEN` to pi, so only a login you made in pi is used.

## Dreamer's pi profile

pi reads its settings, extensions, skills, and prompt templates from an
agent directory (`PI_CODING_AGENT_DIR`). Dreamer's pi sessions use one
that Dreamer owns, `<data dir>/pi-profile/`, not your `~/.pi`. Setting
`agents.pi.agent_dir` uses that directory instead (then Dreamer adds its
extension with `--extension` and leaves the directory alone).

### What Dreamer puts in it

| Path | What it is |
| --- | --- |
| `extensions/dreamer/` | Dreamer's extension: the terminal tools as native pi tools (below) |
| `skills/dreamer-config/`, `skills/dreamer-apps/` | The skills `dream skills` also installs for Claude Code and Codex |
| `skills/dreamer-pi-profile/` | How the agent changes its own profile |
| `prompts/panes.md`, `prompts/collect.md`, `prompts/apps.md` | `/panes` (what is running), `/collect` (gather output over time), `/apps` (build a pane app) |
| `APPEND_SYSTEM.md` | Added to pi's system prompt: the terminals, the approval rules, conciseness |
| `settings.json` | `defaultProvider: openai`; no model, so pi picks that provider's default. `/login` and `/model` change it |
| `.gitignore` | Keeps only the profile's resources in git |

These files are embedded in `dream`, so laying out the profile needs no
source tree. `dream agents profile init` lays it out, and so does
`dream agents update` when the profile is not there yet.

### Dreamer's tools inside pi

The extension registers these pi tools. Each is one call to the daemon
over its JSON-RPC socket ([api.md](api.md#json-rpc-endpoint)), the same
API as `dream mcp` ([mcp.md](mcp.md#terminal-tools)):

| Tool | API |
| --- | --- |
| `terminals_list` | `terminal.list` |
| `terminal_read` | `terminal.read` |
| `terminal_search` | `terminal.query` |
| `terminal_watch` | `terminal.watch` |
| `terminal_run` | `terminal.run` |
| `terminal_send` | `leases.acquire`, `terminal.input` |
| `layout` (`split`, `select`, `rename`, `move`, `zoom`, `close`) | `actions.submit` |
| `collector_create`, `collector_query`, `collector_list`, `collector_stop` | `collect.*`; registered only when the daemon offers `collect.list` |
| `dreamer_profile_commit` | `dream agents profile commit` (below) |

It also adds the commands `/dreamer` (whether the daemon is reachable,
the level, the profile) and `/dreamer-reload` (commit, then reload).

- **Socket.** `DREAMER_RPC_SOCKET`; else `dreamerd.rpc.sock` beside
  `DREAMER_SOCKET`; else in the runtime directory, found as the daemon
  finds it (`DREAMER_RUNTIME_DIR`, `XDG_RUNTIME_DIR`, `TMPDIR`).
  `dream agents pi` sets `DREAMER_RPC_SOCKET`.
- **Grant.** On first use the extension narrows its connection to an
  agent's grant ([grants.md](grants.md#act-levels-and-approvals)): every
  pane, the level from `DREAMER_AGENT_LEVEL` (`observe`, `own`, or
  `elsewhere`, the default), and the tools' methods plus
  `events.subscribe`. It connects when a session starts or a tool first
  runs, never when pi only loads it.
- **Approvals.** When an action waits for your approval, the extension
  shows it in pi (a notice and a status line, with the `dream approvals`
  command that answers it) and records it in the session as a
  `dreamer-approval` entry. It never answers: an agent's grant cannot
  call `approvals.answer`, so you answer in Dreamer or with
  `dream approvals allow|deny ID`. This keeps an agent from approving its
  own actions through Dreamer. It is not an OS sandbox: pi's own `bash`
  tool runs with your privileges.

### History

When `git` is installed, the profile is a git repository, and every
change is a commit:

- **Dreamer's files:** "dreamer base VERSION" when Dreamer writes or
  updates them.
- **The agent's changes:** the extension commits after each of the
  agent's turns and when pi reloads or quits, with the pi session id as a
  `Pi-Session:` trailer. The agent can also commit with a message of its
  own (`dreamer_profile_commit`), which checks the result too.
- **Anything else:** changes found when pi next starts, as "changes
  outside a pi session".

`.gitignore` keeps only `extensions/`, `skills/`, `prompts/`, `themes/`,
`settings.json`, `keybindings.json`, `APPEND_SYSTEM.md`, `SYSTEM.md`,
`AGENTS.md`, and `.dreamer/`, and within them leaves out `auth.json`,
`.env` files, keys, and `node_modules/`. So `auth.json`, sessions,
caches, `models.json`, and `mcp.json` (which can hold keys) are never
committed. Dreamer never reads them.

Without git the profile works the same, without history: the commands
that need it say so.

### Updates

The base layer updates with Dreamer. `.dreamer/base.json` records what
Dreamer last wrote. When a newer `dream` starts pi:

- a shipped file nobody changed is replaced;
- a shipped file the agent changed is kept as the agent left it, and
  reported when Dreamer's version changed too
  (`dream agents profile status` lists such files);
- a shipped file the agent removed stays removed;
- a file Dreamer no longer ships is removed if unchanged;
- everything the agent added is left alone.

### The check, and automatic restore

`dream agents profile check` starts pi once on the profile
(`pi --mode rpc --no-session --no-approve`, with API key variables left
out) and asks it for its commands. pi exits with an error, before
answering, when an extension fails to load (a syntax error, or an
exception while it registers); an unreadable `settings.json` makes it
start with defaults, which the check also counts as failing. When pi
does not start even with an empty agent directory, the problem is pi,
not the profile, and nothing is restored. Skills and prompt templates
pi skipped (for example invalid front matter) are listed but do not fail
the check.

A commit that loads is marked good (`refs/dreamer/good/<commit>`). When
the check fails, Dreamer restores the files of the newest good commit as
a new commit ("restore COMMIT: pi did not load: ..."), so the broken
change stays in the history, and checks again. `dream agents pi` runs
this before every start, so a change that stops pi loading is undone at
the next start.

### Commands

| Command | What it does |
| --- | --- |
| `dream agents pi [ARGS...]` | Update and check the profile (restoring it if needed), then run pi with `PI_CODING_AGENT_DIR`, `DREAMER_RPC_SOCKET`, `DREAMER_DREAM_BIN`, `agents.pi.model` as `--model`, and `ARGS` |
| `dream agents profile init` | Lay out the profile, or bring Dreamer's files up to date |
| `dream agents profile status [--json]` | Where it is, Dreamer's version of the files, the newest and last good commits, shipped files the agent changed, and uncommitted changes |
| `dream agents profile log [-n N]` | The newest commits; `[loads]` marks good ones |
| `dream agents profile diff [REV]` | A commit's change, or what is not committed yet |
| `dream agents profile revert REV` | Undo one commit with a new one, then check. Refused, with nothing changed, when a later commit depends on it |
| `dream agents profile commit [-m MSG] [--session ID] [--no-check]` | Commit every change, then check and mark it good when pi loads it |
| `dream agents profile check [--no-restore]` | Update Dreamer's files, commit stray changes, check, and restore the last good commit on failure; `--no-restore` only reports |

Each `profile` command takes `--dir DIR` for another profile directory.

## Not yet

- The daily check for a newer pi release; `dream agents update` runs it
  by hand.
- Starting sessions with the configured harness. The settings are parsed
  and checked, and take effect for sessions started from them;
  `dream agents pi` starts pi with them by hand.
- Collector tools follow the daemon's `collect.*` methods; their
  parameters are passed through as the agent gives them.
