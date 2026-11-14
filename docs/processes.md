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
