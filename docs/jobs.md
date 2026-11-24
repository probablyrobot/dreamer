# Jobs

A job is an exact program that runs in a tmux window of its own, for work
that should outlive everything else: builds, long tests, migrations,
agent runs you start by hand. The daemon starts it; tmux owns it.

```sh
dream jobs start --name build -- cargo build --release
dream jobs                 # every job and its state
dream jobs artifacts ID    # files the job left behind
dream jobs cancel ID
```

## Starting and following

`jobs.start` (on the command line, `dream jobs start [--cwd DIR] [--name
NAME] -- PROGRAM [ARGS...]`) takes the exact `argv` (no shell), an
optional `cwd`, environment variables to add, a window `name` (default:
the program's name), and a `session` (default the first). The command
line sets only `cwd` and `name`. The window opens in the background, so no
attached client is moved to it. The program gets two extra variables:

| Variable | Value |
| --- | --- |
| `DREAMER_JOB_ID` | The job's id |
| `DREAMER_JOB_ARTIFACTS` | A directory for files the job leaves behind |

`jobs.get`, `jobs.list`, and `dream jobs` show each job's state:
`running`, `succeeded` (exit 0), `failed` (another exit status), `killed`
(a signal), or `lost` (its pane is gone and it left no exit status), with
its exit code or signal, start and end times, and pane. The pane stays
after the program exits, with its output, until it is closed: read it
with `terminal.read` or `dream read`.

`jobs.artifacts` (`dream jobs artifacts ID`) lists the files in its
artifacts directory (at most 1000). `jobs.cancel` (`dream jobs cancel ID`)
sends SIGTERM to the program and marks the job `cancelled`; it then ends
`killed` (or however the program chose to exit).

A `jobs.state` event is recorded when a job starts and when it ends
([events.md](events.md)).

## How it survives

The window runs `dream job-run`, a small wrapper that starts the program,
records its pid, waits for it, and writes its exit status, all in the
job's directory, `<state dir>/jobs/<id>/`. The wrapper needs no daemon.

So:

- **Daemon stops or crashes:** the job keeps running in tmux, and its
  status is still recorded when it ends. A daemon started later finds
  every job in the state directory and reports them as before.
- **tmux server stops, or the window is killed:** the job ends with it. If
  the wrapper could not record a status, the job is `lost`.

Starting a job opens a window, and cancelling one signals a process, so
both need `all_panes` in a narrow grant ([grants.md](grants.md)).

## Jobs and extensions

| | Jobs | Supervised extensions ([extensions.md](extensions.md)) |
| --- | --- | --- |
| Owned by | tmux | The daemon |
| When the daemon stops | Keep running; status still recorded | Asked to stop, then stopped |
| When the daemon starts | Found again from their records | Started again from `extensions.yaml` |
| When it ends or crashes | Ends; the record says how | Restarted with backoff |
| Talks to the daemon | Only if it chooses to (as any client) | Always, over its standard input and output |
