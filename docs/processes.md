# Processes and agent runs

`processes.list` reports the user's processes on the daemon's host: which
tmux pane each belongs to and which are agent runs (Claude Code, Codex),
each with the evidence behind it. It reports only what the system says.
A field it cannot know is absent, not guessed.

## Identity

A process is named by its pid **and** when it started (`started_unix_ms`,
to the second). A pid the system reused belongs to a different process:
compare both before treating two records as the same process.

## Panes

| `pane_evidence` | Meaning |
| --- | --- |
| `ancestry` | The process descends from the pane's program |
| `terminal` | It only shares the pane's terminal (for example, it was reparented) |

A process with neither has no `pane`.

## Runs

A process is a run of a provider when its program says so:

| `evidence` | `confidence` |
| --- | --- |
| The program is the provider's CLI (`claude`, `codex`) | `high` |
| A runtime (`node`, `bun`, `deno`) runs the provider's package (`@anthropic-ai/claude-code`, `@openai/codex`) | `high` |
| A runtime runs a script named like the CLI | `medium` |

A run outside any pane is at most `medium`. Only the topmost process of a
run counts as the run. Processes it started (tools, shells, MCP servers)
are reported with `helper_of` set to the run's pid and are never runs
themselves. A run's `cwd` is reported when the system gives it (`/proc`
on Linux, `lsof` on macOS).

Pane titles, CPU use, and terminal output are not evidence of what a run
is doing: the registry does not say whether a run is thinking, waiting,
or done. Structured run state comes from provider instrumentation, not
from this list.

## Querying

`ProcessQuery` takes `panes` (only processes in those panes), `runs_only`,
and `provider` (`claude` or `codex`). Command lines can contain anything,
so listing processes outside given panes needs `all_panes`
([grants.md](grants.md)).

The list comes from `ps` with fields that read the same on Linux and
macOS (pid, parent, process group, uid, terminal, elapsed time, command
line), filtered to the daemon's user.

## Agent runs across hosts

`agents.runs` lists the runs from this host's registry and, with `hosts`
(or `*` for every live host), from connected hosts through the
coordinator. Each run has its `host` (absent for this one), pid and start
time (together its identity), provider, pane, cwd, age, CPU (tenths of a
percent of one core, as `ps` averages it) and resident memory, its
confidence and evidence, and `helpers`: how many processes serve it.
Helpers are never runs.

`instrumentation` is `passive`: the run was found from its process alone,
so `state` is `running` while that process lives and says nothing about
what the run is doing. A host that cannot be asked is a note, not a
failure. Listing runs needs `all_panes`.

## Instrumented runs (Claude Code hooks)

With Claude Code's hooks reporting to the daemon, a run's `instrumentation`
is `hooks` and its `state` comes from those reports:

| Report | `state` |
| --- | --- |
| `SessionStart` | `active` |
| `UserPromptSubmit`, `PreToolUse`, `PostToolUse` | `working` |
| `PermissionRequest`, `Notification` (permission prompt) | `waiting_for_permission` |
| `Stop`, `Notification` (idle prompt) | `idle`: a turn ended; the CLI may still be running |
| `SessionEnd` | `ended` |

A run whose reports stop for ten minutes before it ended is `stale`, not
still in its last state. The report also gives the run its `session_id`.

**Setting up.** `dream agents hooks` prints the settings to add to Claude
Code (for example `~/.claude/settings.json`). `dream agents hooks --write
FILE` adds them to that file: it keeps everything already there, adds each
hook only once, and first copies the file to `FILE.bak`. Nothing changes a
settings file unless asked this way.

**The hook helper.** Each hook runs `dream hook claude`. It reads the
report from standard input, sends it to the local daemon within 0.8 s,
and exits 0 every time, whether the daemon runs or not. It never starts
the daemon, never writes to standard output, and so never blocks or
delays Claude Code. The daemon ties a report to its run by walking up
from the helper's process to the `claude` process. It does this once per
session, and never mistakes the helper for the run.
