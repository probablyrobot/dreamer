# Events

`dreamerd` keeps a log of small events: what changed, not the content.
A client follows it with `events.subscribe` instead of polling, and can
pick up where it left off after a reconnect. Events never contain terminal
output; read that with `terminal.read` or `terminal.subscribe`.

## Kinds

| Kind | When | Fields set |
| --- | --- | --- |
| `tmux.topology` | The local tmux topology changed | `revision`: the new revision (read it with `tmux.topology`) |
| `actions.outcome` | An action submitted to this daemon finished | `subject`: the action id; `state`: its outcome (`completed`, `failed`, `outcome_unknown`, …); `pane`: the pane it acted on, if one |
| `hosts.state` | A remote host's connection changed state | `host`; `state`: `connecting`, `live`, `closed`, or `failed` |
| `apps.registered` | A launched pane app registered | `subject`: the launch id; `pane` |
| `scripts.failed` | A script instance was terminated (a budget overrun, or its connection ended) | `subject`: the script; `state`: why |
| `filters.matched` | A filter with `event` matched | `subject`: the filter; `pane`; `state`: the line (at most 512 bytes) |
| `filters.tripped` | A filter's circuit breaker stopped it | `subject`: the filter; `state`: why |
| `decorations.changed` | A pane's decorations were added or dropped | `pane`; list them with `decorations.list` |
| `jobs.state` | A job started, or ended while this daemon ran | `subject`: the job id; `state`: `running`, `succeeded`, `failed`, `killed`, or `lost`; `pane` |
| `config.changed` | The configuration directory changed | `state`: `applied` (`subject`: the terminal theme in force) or `refused` (`subject`: why; the settings in force are kept) |
| `approvals.requested` | An agent's action waits for the user's approval ([grants.md](grants.md#act-levels-and-approvals)) | `subject`: the approval id; `pane`: the first pane it would act on; `state`: what it would do |
| `approvals.answered` | An approval was answered, or nobody answered in time | `subject`: the approval id; `pane`; `state`: `once`, `pane`, `deny`, or `timeout` |

Every event also has a `sequence` and `time_unix_ms`. Sequences start at 1
when the daemon starts and increase by one per event.

## Subscribing

`events.subscribe` takes:

- `kinds`: kinds or kind prefixes; `actions` matches `actions.outcome`,
  `app` matches nothing. Empty means every kind.
- `panes`: only events about these panes. Empty means every event.
- `after` and `instance`: resume after this sequence of this daemon
  instance. Without `after`, only new events follow.
- `quota`: how many events the subscriber may fall behind before it skips
  ahead (default 1024).

The reply gives the subscription `id`, the daemon `instance`, the `cursor`
it starts after, and the `oldest` sequence still kept. Events then arrive
as `events.batch` pushes, each with the `cursor` to resume after. End the
subscription with `events.unsubscribe`.

## Gaps

The log keeps the most recent 4096 events, and nothing waits for a slow
subscriber. When events a subscriber should have had are not delivered,
the next batch starts with a **gap**: the sequence range `first..=last`
it missed and why.

| Reason | Meaning |
| --- | --- |
| `lagging` | The subscriber fell further behind than its quota, or behind the log, and skipped ahead. Skipped events it did not ask for (by kind or pane) do not count. |
| `expired` | The resume cursor is older than the oldest event kept. |
| `restarted` | The resume cursor came from another daemon instance. `first` and `last` are 0: the old instance's events are gone, and every event this one still keeps follows. |

After a gap, read the current state that depends on the missed events
(for example `tmux.topology` or `actions.list`) instead of assuming it.

## Grants

`events.subscribe` is checked like any method (see [grants.md](grants.md)).
A connection with a narrow grant then receives only events about panes in
its scope; events about no pane (topology, hosts) need `all_panes`.
