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
    model: openai-codex/gpt-5.5  # optional; else the profile's provider and its default model
    agent_dir: ~/.pi/agent    # optional; else Dreamer's pi profile
    delegates: [claude, codex]  # default; the CLIs pi may hand tasks to, [] for none
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
- `version`, `agent_dir`, and `delegates` apply to pi only. `delegates`
  lists `claude`, `codex`, both, or neither (`[]`); see
  [Delegation](#delegation-to-claude-code-and-codex). `protocol` applies to custom
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
  ~/.local/share/dreamer/agents/pi/current/pi auth login openai-codex
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

### No model turn without a login

**pi's own default.** When pi is not told which model to use (no
`--model`, no `defaultModel` with `defaultProvider` in its settings), it
takes the first model it considers available: one of its built-in
default models per provider, in a fixed list of its own (in pi's source
at the time of writing it begins with Amazon Bedrock), else whatever
available model comes first.
`defaultProvider` alone does not pin anything. A provider can count as
available without a login you made (ambient cloud credentials, a
gateway, a local server), so a pi with no login can still send a prompt
over the network: pi 1.5.2, with an empty agent directory, no login, and
no API key variables, sent one to a provider of its choosing, which
answered `402`. Dreamer never lets pi fall back like this.

**The pinned provider.** Dreamer holds pi to one subscription provider
(`openai`, `openai-codex`, or `anthropic`): the one `agents.pi.model` (or
a session's model) names as `provider/id`, else `defaultProvider` in the
agent directory's `settings.json`, else the one in Dreamer's profile.
Any other provider is refused with that reason. A model given as a bare
id is passed as `--provider P --model ID`, so pi looks it up only there.

**The check.** Before each model turn Dreamer starts, it asks pi itself:
`pi auth check --provider P --json` in the agent directory, with
`PI_OFFLINE=1` and no API key variables. pi may renew an expired login
during the check. Only `ready` lets the turn start; Dreamer does not read
pi's credentials to decide.

| Where | Without a login |
| --- | --- |
| `agent.sessions.send` (and `dream agents sessions send`) | Refused with `precondition` and the command that logs in, before the harness is started or sent anything |
| `agent.sessions.create`, `resume` | The harness starts with `PI_OFFLINE=1`; no prompt is sent |
| `dream agents pi`, and the drawer that runs it | The login step comes first: the reason and the `pi auth login P` command. In a terminal, pi's own login runs, and pi starts only once the check says `ready`; otherwise nothing starts. Outside a terminal it exits with an error. So `dream agents pi --print ...` (a scripted prompt) never runs without a login |

**Held to the provider.** In a session, before each prompt Dreamer asks
pi for its current model (`get_state`). When it is not the pinned
provider's, Dreamer switches it (`set_model`) to `agents.pi.model` if
that provider has it, else to the first of that provider's models pi
lists as available (`get_available_models`). If it cannot, the prompt is
not sent and the turn ends with an error. In pi's own terminal interface,
Dreamer's extension does the same before each prompt
(`DREAMER_PI_PROVIDER`, set by `dream agents pi`): it switches to the
pinned provider's model, or holds the prompt back with a notice.

A drawer with `agents.pi.command` set runs that command exactly as
given, without these checks; run `dream agents pi` in it to keep them.

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
| `settings.json` | `defaultProvider: openai-codex`, the provider Dreamer pins pi to ([above](#no-model-turn-without-a-login), and [The base model](#the-base-model)); no model, so the pinned provider's default. `/login` and `/model` change it |
| `.gitignore` | Keeps only the profile's resources in git |

These files are embedded in `dream`, so laying out the profile needs no
source tree. `dream agents profile init` lays it out, and so does
`dream agents update` when the profile is not there yet.

### The base model

The profile's base is OpenAI's Codex models on your ChatGPT subscription:
`settings.json` sets `defaultProvider: openai-codex`, pi's provider for
ChatGPT Plus or Pro through the Codex backend, and no model, so pi uses
that provider's default model.

- **Sign in** once with `/login` inside pi and choose "OpenAI (ChatGPT
  Plus/Pro)", or run the `auth login openai-codex` command above. The
  login is pi's own, in the profile's `auth.json`; Dreamer never reads it.
- **Why not `openai`.** pi's `openai` provider also takes an API key, so a
  key in pi's own settings could bill an API account. `openai-codex` only
  signs in with a subscription.
- **Another model for a session:** `/model` inside pi, for example a
  Claude model once "Claude Pro/Max" is signed in with `/login`.
- **Another model for every session:** `agents.pi.model` in
  `dreamer.yaml`, as pi names it (`provider/model`, for example
  `openai-codex/gpt-5.5`); `dream agents pi` passes it as `--model`.
- **Another default provider:** edit the profile's `settings.json`
  (`defaultProvider`), or let the agent do it. The edit is the agent's
  change in the profile's history, and Dreamer's newer version of the file
  is then not applied.

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
| `send_to_pane` | `leases.acquire`, `terminal.input`: a code block as one paste, Enter only when asked ([below](#pane-context-and-send-to-pane)) |
| `layout` (`split`, `select`, `rename`, `move`, `zoom`, `close`) | `actions.submit` |
| `collector_create`, `collector_query`, `collector_list`, `collector_stop` | `collect.*`, with the parameters of `dream mcp`'s collector tools ([mcp.md](mcp.md#collector-tools)); registered only when the daemon offers `collect.list` |
| `claude_code`, `codex` | `terminal.run` of `dream agents delegate` ([below](#delegation-to-claude-code-and-codex)); each only when `agents.pi.delegates` lists it, and not at the `observe` level |
| `app_new`, `app_check`, `app_preview`, `app_test`, `app_install`, `app_launch` | As `dream mcp`'s app tools ([mcp.md](mcp.md#app-tools)), below |
| `dreamer_profile_commit` | `dream agents profile commit` (below) |

It also adds the commands `/dreamer` (whether the daemon is reachable,
the level, the profile), `/dreamer-reload` (commit, then reload), and
`/pane` ([below](#pane-context-and-send-to-pane)).

- **Socket.** `DREAMER_RPC_SOCKET`; else `dreamerd.rpc.sock` beside
  `DREAMER_SOCKET`; else in the runtime directory, found as the daemon
  finds it (`DREAMER_RUNTIME_DIR`, `XDG_RUNTIME_DIR`, `TMPDIR`).
  `dream agents pi` sets `DREAMER_RPC_SOCKET`.
- **Grant.** On first use the extension narrows its connection to an
  agent's grant ([grants.md](grants.md#act-levels-and-approvals)): every
  pane, the level from `DREAMER_AGENT_LEVEL` (`observe`, `own`, or
  `elsewhere`, the default), and the tools' methods plus
  `events.subscribe` and `drawer.status`. The collector methods are in the grant at every
  level, `observe` included: collectors only read
  ([collectors.md](collectors.md#grants)). It connects when a session
  starts or a tool first runs, never when pi only loads it.
- **Collector rows.** A collector the agent creates with
  `collector_create` (unless `wake` is false) is followed: its
  `collect.row` events are gathered for 1.5 seconds and then sent to the
  session as one `dreamer-collector` message that names the collector, how
  many rows, their ids and panes, and the `collector_query` call that
  reads them. The message starts a turn, or follows the current one, so
  the agent is woken by a failing test in another pane instead of
  polling. A pi without `sendMessage` for extensions gets a notice
  instead. Following lasts for the pi process; `collector_stop` ends it.
- **Approvals.** When an action waits for your approval, the extension
  shows it in pi (a notice and a status line, with the `dream approvals`
  command that answers it) and records it in the session as a
  `dreamer-approval` entry. It never answers: an agent's grant cannot
  call `approvals.answer`, so you answer in Dreamer or with
  `dream approvals allow|deny ID`. This keeps an agent from approving its
  own actions through Dreamer. It is not an OS sandbox: pi's own `bash`
  tool runs with your privileges.

- **Apps.** The app tools build a package in the apps directory
  (`DREAMER_APPS_ROOT`, else pi's working directory; nothing outside it)
  by running `dream app new|check|preview --text|test|install`, with the
  parameters and results of `dream mcp`'s app tools. `app_launch` gets the
  installed app's `apps.launch` request from `dream app launch NAME
  --request` and sends it on the extension's own connection, beside the
  pane pi runs in (`TMUX_PANE`, the drawer) unless told otherwise, so the
  daemon checks it against the session's grant, the session owns the new
  pane, and a split of a pane it did not create waits for your approval.
  An image preview runs `dream app preview DIR --png FILE --level LEVEL`
  at the session's level and returns the PNG as image content.
  `app_install` and `app_launch` are refused at `observe`
  ([grants.md](grants.md#agents-and-apps)).

### Pane context and send to pane

In a prompt to pi, these references attach text from Dreamer. The prompt
is sent as typed, followed by one fenced block per reference:

| Reference | Attaches |
| --- | --- |
| `@pane` | The visible text of the pane you came from: the pane that had focus when the drawer was shown, or when the drawer key moved focus to it |
| `@%N` | The visible text of pane `%N` |
| `@name` | The newest 20 rows of the collector `name` ([collectors.md](collectors.md)): each row's id, time, pane, command, exit status, fields, and its matched line with context, or its captured lines |

- **Command.** `/pane` attaches the pane you came from to your next
  prompt; `/pane %N` attaches pane `%N`. `/pane last` attaches the pane's
  last finished command and its output instead, as a running collector
  with `command_end` (such as the `commands` preset) recorded it; that
  needs the shell's OSC 133 marks. Without such a row it attaches the
  visible text and says so.
- **Labels.** Each block starts with a line naming the pane (`Pane %3`,
  with its title, command, and directory) or the collector.
- **Bounds.** A block holds at most the last 200 lines and 16 KB of text;
  a block that was cut says how many lines it kept. Its fence is longer
  than any run of backticks inside.
- **What counts.** An `@` counts at the start of the prompt or after a
  space, a quote, or an opening bracket, so mail addresses do not.
  `@name` counts only when a collector has that name; anything else stays
  as typed. A reference that cannot be attached (a pane that is gone, or
  `@pane` outside a drawer) is left as typed, with a notice.
- **Where you came from.** `@pane` and `/pane` ask `drawer.status` for
  `from_pane`, for the session in `DREAMER_DRAWER`. pi started outside a
  drawer has no such pane: name one with `@%N`.
- **Reading is not acting.** Attaching reads panes with `terminal.read`
  and collectors with `collect.query`, which every level may call.

`send_to_pane` types a command or code block into a pane for you to run or
edit there. A Markdown fence around the block is removed, the rest is sent
as one paste, and Enter is pressed only when the agent asks for it. It
goes through the same interaction lease and approvals as `terminal_send`:
in a pane the agent did not create it waits for your approval at the
`elsewhere` level and is refused at `observe` and `own`
([grants.md](grants.md#act-levels-and-approvals)); and when you type in
that pane in Dreamer, the lease ends, the agent's next input there is
refused, and it is told to ask you first. Typing through another tmux
client, which the daemon does not see, does not end the lease.

Not available yet: `/selection`, attaching the selection in the desktop.
The desktop keeps its selection to itself; the daemon cannot read it.

### Delegation to Claude Code and Codex

The `claude_code` and `codex` tools hand a task to the Claude Code or
Codex CLI you installed, run headless on that CLI's own login, and return
its answer to pi's agent.

- **What runs.** The tool calls `terminal.run` with
  `dream agents delegate --to claude|codex --cwd DIR -- TASK`. That opens
  a new window named after the tool, which the agent owns (no approval is
  needed), so you can watch the run there. `dream agents delegate` runs
  the CLI as the agent providers do ([providers.md](providers.md#rules)):
  - Claude Code: `claude -p TASK --output-format stream-json --verbose
    --include-partial-messages --no-session-persistence --tools
    Read,Grep,Glob --allowedTools Read,Grep,Glob --strict-mcp-config` with
    no MCP servers;
  - Codex: `codex exec --json --skip-git-repo-check --sandbox read-only
    -- TASK`.

  Both can read files under the directory (pi's working directory unless
  the agent gives `cwd`) and neither can change them.
  `agents.claude.model` and `agents.codex.model` choose the model, and
  `agents.claude.command` and `agents.codex.command` the program (its
  first word; further arguments are not passed).
- **Its own login, never a key.** `ANTHROPIC_API_KEY`,
  `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY`, and `CODEX_API_KEY` are
  removed from the CLI's environment. Before running, the CLI's readiness
  is checked as `dream agents providers` checks it (`claude auth status`,
  `codex login status`). A CLI that is not installed, is logged out, or is
  logged in with an API key is reported to the agent as the tool's error,
  and nothing runs. A Claude Code run that reports an API key from its
  own settings is stopped.
- **What comes back.** The final answer, the window's pane, the model,
  and a summary of the CLI's tool use (for example `Read ×3, Grep`, or for
  Codex `command ×2` for its shell commands). The full result is written
  as JSON to `delegate.json` in the job's artifacts directory.
- **How long.** 30 minutes unless the agent gives `timeout_s` (at most
  7200). Cancelling the tool call in pi cancels the job.
- **Turning it off.** `agents.pi.delegates` lists the tools to offer:
  `[claude, codex]` by default, `[codex]` for one, `[]` for none.
  `dream agents pi` passes the list to the extension as
  `DREAMER_PI_DELEGATES`, and `dream agents delegate` refuses a CLI the
  setting leaves out. A pi started another way, without that variable,
  offers both.
- **The task stands alone.** The delegated CLI does not see pi's
  conversation, so the agent puts what it needs in the task.

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
| `dream agents pi [ARGS...]` | Update and check the profile (restoring it if needed), check pi's login for the pinned provider (the login step first when there is none), then run pi with `PI_CODING_AGENT_DIR`, `DREAMER_RPC_SOCKET`, `DREAMER_DREAM_BIN`, `DREAMER_PI_PROVIDER`, `agents.pi.model` as `--model`, and `ARGS` |
| `dream agents profile init` | Lay out the profile, or bring Dreamer's files up to date |
| `dream agents profile status [--json]` | Where it is, Dreamer's version of the files, the newest and last good commits, shipped files the agent changed, and uncommitted changes |
| `dream agents profile log [-n N]` | The newest commits; `[loads]` marks good ones |
| `dream agents profile diff [REV]` | A commit's change, or what is not committed yet |
| `dream agents profile revert REV` | Undo one commit with a new one, then check. Refused, with nothing changed, when a later commit depends on it |
| `dream agents profile commit [-m MSG] [--session ID] [--no-check]` | Commit every change, then check and mark it good when pi loads it |
| `dream agents profile check [--no-restore]` | Update Dreamer's files, commit stray changes, check, and restore the last good commit on failure; `--no-restore` only reports |

Each `profile` command takes `--dir DIR` for another profile directory.
## The harness drawer

The drawer ([desktop.md](desktop.md#the-harness-drawer)) runs the harness
in a tmux pane: its own terminal interface, as you would run it in a
shell. The daemon starts it the first time a session's drawer is asked
for.

- **Which program.** `agents.default` names the harness. With a
  `command`, that program and its arguments run exactly as given (no
  shell). Without one, pi runs only through `dream agents pi`, which
  checks Dreamer's pi profile (restoring the last good one if a change
  broke loading), shows the login step when pi has no login
  ([above](#no-model-turn-without-a-login)), removes API-key variables,
  and passes `agents.pi.model`; it uses the managed install
  (`agents/pi/current/pi`), else `pi` on the daemon's `PATH`. When `dream`
  is not found beside `dreamerd` (or in `DREAMER_DREAM`), the pane says so
  instead of starting pi. `claude`
  and `codex` are looked up on the daemon's `PATH`. A custom ACP agent
  runs its `command`.
- **When it is missing,** the pane says so (for pi: run `dream agents
  update`) and closes on Enter. The next use starts the harness again.
- **Working directory:** that of the focused pane in the window it is
  first shown in.
- **Environment.** The pane gets `DREAMER_RUNTIME_DIR`,
  `DREAMER_STATE_DIR`, `DREAMER_DATA_DIR`, and `DREAMER_CONFIG_DIR`, so
  `dream` and Dreamer's tools inside the harness reach this daemon, and
  `DREAMER_DRAWER` with the session id (`$N`). For pi,
  `PI_CODING_AGENT_DIR` is `agents.pi.agent_dir`, else Dreamer's pi
  profile, `<data dir>/pi-profile`, created if missing.
- **Tagged in tmux.** The pane carries the user option
  `@dreamer_drawer_for` with its session id, so a restarted daemon finds
  it again, and other tmux clients can see which pane it is.
- **Never put to sleep.** While the pane exists the tier policy treats
  it as followed, so a hidden drawer keeps its mirror
  ([settings.md](settings.md#pane-tiers)).
- `drawer.toggle` is for the user's own clients only: an app or agent
  grant cannot call it.

## Sessions

An agent session is a conversation with a harness that the daemon runs,
for apps, scripts, and other headless use. It keeps running when no
client is connected, and its transcript outlives the daemon. The
`agent.sessions.*` methods of the API work with sessions; `dream agents
sessions` does the same from a shell ([cli.md](cli.md#agent-runs)).

| Method | What it does |
| --- | --- |
| `agent.sessions.create` | Start a session: the harness (default `agents.default`), its directory, a model, a name, and a grant |
| `agent.sessions.send` | Send a prompt. Sent while a turn runs, it waits for that turn to end. A detached session is resumed first |
| `agent.sessions.events` | The transcript after a sequence number; with `follow`, then every new event as it happens |
| `agent.sessions.cancel` | Stop the running turn; it ends with stop reason `cancelled` |
| `agent.sessions.approve`, `agent.sessions.deny` | Answer an approval request |
| `agent.sessions.list` | Sessions, open and closed |
| `agent.sessions.resume` | Start a detached (or closed) session's harness again |
| `agent.sessions.close` | End the harness process; the transcript stays |

Only pi runs sessions so far. Creating a session on `claude`, `codex`, or
an ACP harness is refused with that reason.

An app shows a session with a `transcript` and writes to it with a
`composer` ([termapp.md](termapp.md#agent-sessions-transcript-and-composer)):
the session's events go from the daemon to the pane's terminal interface
and to the desktop without passing through the app.

### Events

Every harness's output becomes the same events, shaped after the Agent
Client Protocol. Each has a sequence number (`seq`, from 1), a time, and
one kind:

| Kind | What it holds |
| --- | --- |
| `user_message` | A prompt that was sent |
| `text_delta`, `thinking_delta` | Text and thinking, as they stream |
| `tool_call` | One tool call's `id` and `name`, with `status` `started` (and its `input` as JSON), `updated` (partial `output`), then `finished` or `failed` (its `output`) |
| `approval_request` | The harness asks before going on: an `id`, a `title`, a `message`, `options` to choose from (none for yes or no), and when it stops waiting |
| `approval_answer` | How a request was answered, and by whom |
| `plan` | The agent's plan entries |
| `usage` | Tokens of the latest model response, and its cost when the harness reports one |
| `turn_end` | The turn is over: `end_turn`, `cancelled`, or `error` |
| `error` | What went wrong, in the harness's words |
| `status` | The session's state changed (the harness started, resumed, or ended, or the session closed) |

`agent.sessions.events` with `follow` replies first, then pushes
`agent.sessions.events` notifications: the stored events after `after`,
then each new one, in order and none missing. It ends with
`terminal.unsubscribe`, or with a `terminal.end` notification when the
session closes.

With `for_pane`, `agent.sessions.events`, `agent.sessions.approve`, and
`agent.sessions.deny` are asked for the app in that app pane: the desktop
drawing the app's transcript uses it. The daemon then checks the app's
grant as well as the caller's: the app must hold the method, and the
session must be one the app may see (one it created, unless it holds the
owner grant). `taps.subscribe` takes `for_pane` the same way.

A session's state is `starting`, `idle`, `working`, `waiting` (for an
approval), `detached` (no harness runs), or `closed`.

### Transcripts and restarts

Each event is written to `<state dir>/sessions/<id>.jsonl`, one JSON
object per line, before any client sees it. `<id>.json` beside it holds
the session's harness, directory, model, grant, and the harness's own
session (pi's session id and file). The state directory is
`DREAMER_STATE_DIR`, then `$XDG_STATE_HOME/dreamer`, then
`~/.local/state/dreamer`.

When the daemon stops, it ends every harness, and its sessions become
`detached` ("the daemon stopped"). After it starts again they are listed
with their transcripts. Sending to a detached session, or resuming it,
starts the harness again on its own session, so the conversation goes on
where it was. pi keeps a session on disk once it has a message; a session
that never had one starts afresh.

### The harness process

- A child process of the daemon, in its own process group, started in the
  session's directory. Closing the session closes its input, then sends
  SIGTERM to its group, then SIGKILL.
- Its environment is the daemon's, without `ANTHROPIC_API_KEY`,
  `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY`, or `CODEX_API_KEY`, so it uses
  its own login only. It also gets `DREAMER_RUNTIME_DIR`,
  `DREAMER_SESSION_ID`, and `DREAMER_SESSION_TICKET` (see below).
- pi runs as `pi --mode rpc` (plus `--model` when one is set, with
  `--provider` for a bare model id, and `--session <file>` to resume),
  with `PI_CODING_AGENT_DIR` set to its agent directory,
  `PI_SKIP_VERSION_CHECK=1`, and `PI_OFFLINE=1` when pi reported no login
  as it started. A prompt needs a login
  ([above](#no-model-turn-without-a-login)).

**Starting checks the harness.** Dreamer sends pi `get_state` and `abort`
before the session counts as started. A pi that refuses either, or whose
`get_state` has no session id, is not used, and the error names the pi
version and the command it lacks. Dreamer uses these parts of pi's RPC
mode:

| Direction | Records |
| --- | --- |
| To pi | `get_state`, `abort`, `get_available_models` and `set_model` (holding pi to the pinned provider), `prompt` (with `streamingBehavior: followUp` while a run is going), `extension_ui_response` |
| From pi | `response`, `agent_start`, `agent_settled`, `message_update` (`text_delta`, `thinking_delta`, `toolcall_end`), `message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, `auto_retry_end`, `compaction_end`, `extension_error`, `extension_ui_request` |

pi's `confirm` and `select` dialogs become approval requests. Its `input`
and `editor` dialogs, which a session has no place for, are dismissed.
pi has no plan of its own, so pi sessions have no `plan` events.

### Grants

Each session holds a grant (see [grants.md](grants.md)):

- It must be within the grant of the connection that creates it. Without
  one, it is the terminal tools on every pane at the `observe` level.
- It never holds the owner grant, `agent.*`, `apps.*`, or `approvals.*`:
  a harness neither starts sessions nor answers approvals.
- A program the harness starts reaches Dreamer with that grant: it calls
  `apps.register` with launch id `session:<id>` and the contents of the
  file `DREAMER_SESSION_TICKET` names. The daemon checks that the calling
  process descends from the harness. `dream mcp` does this by itself when
  those variables are set.
- A caller that is not the owner sees and acts on only the sessions it
  created. A session's own connection never answers its approval
  requests.

The harness runs with your privileges, like any program you start; the
grant limits what it does through Dreamer, not what it does on its own.

### Contract test

`crates/providers/tests/pi_session.rs` drives the adapter against a
scripted stand-in for pi (`crates/providers/testdata/fake-pi`), so it
needs no model or login. The stand-in answers `auth check` from a list
of logged-in providers and logs the provider each turn would reach, so
`crates/server/tests/sessions.rs` and `crates/cli/tests/agents_install.rs`
show that no turn starts without a login. Its `pi_contract` test runs against a real pi
when `DREAMER_PI_CONTRACT` names the executable: the start check, pi's
version, resuming a kept session with `--session`, and closing. It sends
no prompt, so it calls no model.

## Not yet

- The daily check for a newer pi release; `dream agents update` runs it
  by hand.
- Sessions over Claude Code, Codex, and ACP harnesses.
- Running the pi contract test in CI against each new pi release.
- `/selection` in pi: attaching the desktop's current selection. (An
  app's `composer` attaches it, as tmux's newest buffer.)
