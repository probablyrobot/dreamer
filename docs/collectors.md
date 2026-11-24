# Collectors

A collector is a named, standing rule that turns terminal output into rows
of data: test failures, commands and their exit status, errors from a
server's log. It runs in `dreamerd`, keeps a bounded table of rows, and
survives daemon restarts, so you, an agent, or an app can ask for what
happened without re-reading scrollback.

A collector only reads. It never changes what a program wrote or what any
tmux client sees, and it never writes to a pane.

## A first collector

Record every failed test from every pane, using the shipped
`test-failures` [preset](#presets), then ask what failed in the last hour:

```sh
dream collect create --preset test-failures
dream collect list
dream collect query test-failures --since 1h
dream collect query test-failures --since 1h --group-by test
dream collect stop test-failures --delete
```

Or write the rule yourself: a name, the panes, and what makes a row.

```sh
dream collect create fails --all --line '^FAILED (?P<test>\S+)' --after 5 --keep-for 7d
```

All the options are under [From the command line](#from-the-command-line).

## A definition

A definition says which panes a collector reads (`from`), what makes a row
(`when`), what a row keeps (`capture`), and how much is kept (`keep`):

```yaml
name: test-failures
from: { all: true }
when: { line: "^(FAILED|error\\[E\\d+\\])\\s*(?P<what>.*)" }
capture: { fields: [{ name: kind, value: "$1" }], before: 2, after: 10 }
keep: { rows: 5000, max_age_s: 604800 }
per_second: 10
```

The same in JSON works too: `dream collect create -f FILE` reads either.
A definition can also start from a shipped one, with `preset`; see
[Presets](#presets).

### `from`: which panes

Exactly one of:

| Key | Panes |
| --- | --- |
| `all: true` | Every pane, including panes created later |
| `session: NAME` | The panes of the session with this name, as they come and go |
| `window: N` | The panes of window `@N` |
| `panes: [3, 7]` | These panes |
| `command: REGEX` | Panes whose current command (tmux's `pane_current_command`, such as `cargo` or `pytest`) matches, while it does. The daemon reads the commands again every 2 seconds, so output in the first moments of a command can be missed |

A pane is followed from the moment its follower starts; what it wrote
before is not looked at. `collect.create` replies once the panes covered
then are followed, so anything they write after the reply is seen.

### `when`: what makes a row

Exactly one of:

| Key | A row when |
| --- | --- |
| `line: REGEX` | A committed line matches (`ignore_case: true` to ignore case) |
| `command_end: true` | A command finishes, as the shell reports it through shell integration |
| `idle_ms: N` | The pane wrote nothing for N ms (at least 100) after writing something; once per quiet period |
| `bell: true` | The pane rang the bell |
| `exit: true` | The pane's process ended: the pane closed, died, or was respawned |
| `every_s: N` | Every N seconds (at least 1) |

A **committed** line is one the cursor has moved past, as for a `text`
tap ([taps.md](taps.md)): output lines, and a command line once Enter was
pressed. The line being typed is not committed, and a full-screen program
(an alternate screen) commits no lines; use `every_s` with `screen` for
those.

Regular expressions are those of `terminal.query`: Rust syntax, linear
time, at most 4 KiB, and lines are matched on their first 8 KiB.

**Command end** needs shell integration: the shell writes OSC 133 marks
around each command (`OSC 133;C` when a command starts, `OSC 133;D;STATUS`
when it ends). Shell-integration scripts that support OSC 133 write them;
a plain shell does not. Without the marks a `command_end` collector
records nothing; there is no guessing from prompts. The row's
`exit_status` is the status the shell reported (absent if it sent none),
`command` is the command line if the shell sent one with the start mark,
and the field `duration_ms` is the time between the two marks as the
daemon saw them. The row's `lines` are read just after the end mark
arrives, so with `lines` they can include a little of what followed it
(the next prompt).

**Exit** records how the pane ended in the field `reason`: `closed` (the
pane is gone), `dead` (tmux keeps it with `remain-on-exit`), or
`respawned`. tmux does not report the exit status of a pane that closed,
so an exit row has no `exit_status`; use `command_end`, or run the program
as a job ([jobs.md](jobs.md)), for the status.

### `capture`: what a row keeps

| Key | Keeps | Applies to |
| --- | --- | --- |
| (always) | The matched line in `line` | `line` |
| Named groups `(?P<name>...)` | A field per group that took part | `line` |
| `fields: [{ name, value }]` | A field per entry; `value` is a template where `$1`, `${1}`, or `${name}` is a group and `$$` a dollar sign. A group that took no part is empty, and a template that comes out empty adds no field | `line` |
| Numbered groups | Fields `1`, `2`, ... when the pattern has no named groups and `fields` is empty | `line` |
| `json: true` | The matched line is a JSON object: each top-level member becomes a field (strings as they are, other values as JSON). A line that is not an object adds no fields | `line` |
| `before: N`, `after: N` | Up to N lines before and after the matched line (at most 100 each). A row waits up to 2 seconds for its `after` lines, then is recorded with what came | `line` |
| `screen: true` | The pane's screen, trailing blank lines removed, in `lines` | every trigger; the default for `every_s` |
| `lines: N` | The pane's last N lines (at most 1000) up to the cursor, in `lines` | every trigger |

A template that names a group the pattern does not have is refused when
the collector is created. A definition has at most 64 `fields`, and a
`json` line adds at most 64. Field values and lines are cut at 8 KiB.

### `keep`: how much

| Key | Default | Limit |
| --- | --- | --- |
| `rows` | 5000 | 100000; the oldest go first |
| `max_age_s` | 7 days | Rows older than this go |

### `per_second`: the rate cap

Each pane may add at most `per_second` rows per second to a collector
(default 10, at most 1000; bursts up to the same number). Rows over the cap
are not recorded and are counted: `dropped` in the collector's status
counts them all, and each row's `dropped` says how many that pane dropped
since its previous recorded row. A row that finds the file's write queue
full (4096 rows) is dropped and counted the same way.

## Presets

Dreamer ships five definitions, used by name: `preset: NAME` in a
definition (`collect.create`), `dream collect create --preset NAME`, or the
`preset` of the agent tool `collector_create`. A preset is a plain
definition like any other. Each part a definition gives (`from`, `when`,
`capture`, `keep`, `per_second`) replaces the preset's part as a whole;
the parts it leaves out are the preset's. The collector is named after the
preset unless the definition names it. `collect.list` shows the definition
as it runs, with `preset` naming where it started.

| Preset | Panes | A row when | Fields |
| --- | --- | --- | --- |
| `errors` | all | A line is an error or a warning: gcc and clang (`file:line:col: error: ...`), tsc (both formats), rustc (`error[E0308]: ...`; the location is in `after`), eslint's stylish format (the file is the heading above), Python and JavaScript exceptions (`ValueError: ...`, pytest's `E   ...`), Go build errors and panics, and log lines with an upper-case level (`ERROR`, `WARN`, `WARNING`, `FATAL`, `CRITICAL`). Keeps 5 lines before and 3 after | `severity` (as written: `error`, `warning`, `Error`, `ERROR`, `panic`, ...), `code` (`E0308`, `TS2322`, an eslint rule, an exception's name), `file`, `line`, `col`, `message` |
| `test-failures` | all | A test failed: cargo test (`test x ... FAILED`), go test (`--- FAIL: TestX`, and `FAIL pkg` for a package), pytest (`FAILED node - message`, and `-v`'s `node FAILED`), jest and vitest (`✕`, `×`, or `✗` marks, and `FAIL file [> test]` lines). Keeps 5 lines after | `test`, `file`, `message` |
| `commands` | all | A command finished (`command_end`; needs OSC 133, see above). Keeps the last 20 lines | `duration_ms`; the row's `exit_status` and `command` |
| `urls` | all | A line holds an `http` or `https` URL (the first one; trailing punctuation is left out) | `url` |
| `paths` | all | A line holds a file path with a line number, `path:line` or `path:line:col`; the file name needs an extension, so times (`12:30:45`) and host ports (`localhost:8080`) are not paths | `file`, `line`, `col` |

A field a format does not have is absent: a Go build error has no
`severity`, a rustc error no `file`. Each format is one alternative of the
preset's pattern, with numbered groups, and each field's template joins
the groups that can hold it; only the matching alternative's groups take
part. `dream collect list --json` shows the full definitions.

Overriding, for example the test failures of one session's panes only, kept
for a day:

```yaml
name: ci-failures
preset: test-failures
from: { session: ci }
keep: { max_age_s: 86400 }
```

## A row

| Field | Meaning |
| --- | --- |
| `id` | Increases by one per row of the collector, across daemon restarts |
| `collector` | The collector's name |
| `time_unix_ms` | When it was recorded |
| `pane`, `window`, `session` | Where; `session` is the session's name |
| `cwd` | The working directory the shell reported (OSC 7), else the pane process's |
| `command` | The command line the shell reported, else the pane's current command |
| `trigger` | `line`, `command_end`, `idle`, `bell`, `exit`, or `interval` |
| `line` | The matched line |
| `fields` | Name and value pairs (see `capture`) |
| `before`, `after` | Context lines |
| `lines` | The screen or last lines, when `capture` asks |
| `exit_status` | `command_end`: the status the shell reported |
| `dropped` | Rows this pane dropped since its previous recorded row |

## Queries

`collect.query` takes the collector's `name` and, all optional:

| Field | Meaning |
| --- | --- |
| `panes` | Only rows from these panes |
| `fields` | Only rows with each of these fields at exactly this value |
| `since_unix_ms`, `until_unix_ms` | A time range |
| `after` | Only rows after this row id, the first `limit` of them: page through a table, or pick up where you left off |
| `limit` | Default 100, at most 10000. Without `after`, the newest `limit` rows |
| `group_by` | Count rows by a field, or by `pane`, `session`, `window`, `cwd`, `command`, or `trigger`, instead of listing them. Built-in names win over fields of the same name |

Rows come oldest first; groups come most rows first. The answer says how
many rows `matched`, whether more matched than were returned
(`truncated`), and the collector's `dropped` count.

## Lifecycle

- `collect.create` checks the definition and starts the collector. Errors
  name the key at fault (`when takes exactly one of ...`, `keep.rows is
  from 1 to 100000`, a pattern that does not compile). A running collector
  of the same name is a conflict; a stopped one is replaced by the new
  definition and keeps its rows and counters.
- `collect.list` shows each collector: its definition, `running` or
  `stopped`, rows kept, rows `collected` and `dropped` since it was
  created, the panes it follows now, and its last error.
- `collect.stop` stops a collector. Its rows stay queryable and it stays
  stopped across restarts. With `delete`, its definition and rows are
  removed.

A collector's `last_error` reports trouble without stopping it: a pane it
could not follow, a file it could not write, or lines it did not see
because more than 200 were committed between two looks at a pane.

## Events

Each row is announced in the event log as a `collect.row` event: `subject`
is the collector, `pane` the pane, and `state` the row's id
([events.md](events.md)). Read the row with `collect.query` and `after`.
An agent can wait on these instead of polling.

## Storage

Each collector keeps `collector.json` (its definition and counters) and
`rows.jsonl` (one row per line, as JSON) in
`<state directory>/collectors/<name>/`, readable only by you. Rows are
appended as they are recorded, by one writer per collector, and the file
is rewritten without the rows that are gone once it holds more than twice
what is kept. A line cut short by a crash is skipped when the daemon
starts. Rows past `keep` go when the daemon starts, and rows older than
`max_age_s` go within a quarter of that age (at most 30 seconds late).

## Cost and the tier policy

A collector follows each pane it covers the way a view does, through a
subscription to the pane's canonical mirror. A covered pane therefore
counts as followed, and the tier policy ([mirrors.md](mirrors.md#tiers))
never puts it to sleep; a collector over `all` panes keeps every pane
awake. The subscription only says that the pane wrote something, and
carries bells and shell-integration marks: the collector reads what is
new from the mirror on its own task, at most once per 100 ms per pane, as
a text tap does. The mirror never waits for a collector, so a collector
cannot slow a pane or another client down.

## Grants

`collect.create` with only `panes` needs those panes in the connection's
grant; any other `from`, a preset without a `from` of its own (every
preset reads all panes), and `collect.list`, `collect.query`, and
`collect.stop`, need `all_panes` (see [grants.md](grants.md)).

Collectors only read, so an agent's grant at any level, `observe`
included, may create, list, and query collectors (`dream mcp --level` and
Dreamer's pi extension give it `collect.create`, `collect.list`, and
`collect.query`). Collectors have no owner, so stopping or deleting one
may undo the user's own: `collect.stop` is an action, given from `own`
up.

## From the command line

```sh
dream collect create -f test-failures.yaml
dream collect create fails --all --line '^FAILED (?P<test>\S+)' --after 5 --keep-for 7d
dream collect create cmds --session main --command-end --lines 20
dream collect create --preset test-failures
dream collect create --preset errors build-errors --session build
dream collect list
dream collect query fails --since 1h --group-by test
dream collect query cmds --field exit_status=1 --json
dream collect stop fails            # --delete also removes its rows
```

`dream collect create NAME` takes one option from each of the first two
rows; the rest are optional. With `--preset PRESET`, the name is optional
too, and each part given replaces the preset's.

| Part | Options |
| --- | --- |
| `from` | `--all`, `--session NAME`, `--window @N`, `--pane %N` (repeatable), or `--command REGEX` |
| `when` | `--line REGEX` (with `--ignore-case`), `--command-end`, `--idle MS`, `--bell`, `--exit`, or `--every SECONDS` |
| `capture` | `--field NAME=TEMPLATE` (repeatable), `--json-line`, `--screen` or `--lines N`, `--before N`, `--after N` |
| `keep` | `--keep-rows N`, `--keep-for DURATION`, `--per-second N` |

`dream collect query NAME` takes `--pane %N` and `--field NAME=VALUE`
(both repeatable), `--since DURATION`, `--after ID`, `--limit N`, and
`--group-by FIELD`. `list` and `query` print JSON with `--json`. A
duration is a number with `s`, `m`, `h`, or `d` (`30s`, `10m`, `7d`).

See also [cli.md](cli.md#collectors).
