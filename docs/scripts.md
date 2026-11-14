# Scripts

Scripts are small programs in [Luau](https://luau.org) that run inside
Dreamer: filters, triggers, commands, and the logic of small pane apps.
They are the extension tier between declarative files and supervised
processes ([extensions.md](extensions.md)). See
[ADR 0001](adr/0001-luau-script-runtime.md) for why Luau.

## The sandbox

Each script instance has its own Luau VM:

- Only Luau's safe standard libraries: no `io`, no `os.execute` or
  `os.getenv`, no `require` of files, no native modules.
- Built-in libraries and globals are read-only. A script's own globals are
  private to it.
- The only way out is the `dreamer` module.

## The `dreamer` module

`dreamer` is generated from the API schema
(`crates/api/generated/dreamer.luau`), one function per method:

```lua
local text = dreamer.terminal.read({ pane = 3, history = 0 })
local ok, err = pcall(dreamer.daemon.shutdown)   -- refused outside the grant
```

- Parameters and results are tables following the JSON conventions in
  [api.md](api.md#json-conventions): records have string keys, lists are
  sequences, enums are names, and bytes are base64 strings.
- Numbers are doubles. Integers must be within ±2^53, and larger ones are
  refused rather than rounded. NaN and infinities are refused.
- Every call is performed under the script's grant
  ([grants.md](grants.md)). A refusal, or any other error from the
  daemon, is a Luau error whose message carries the code (for example
  `-32005`); catch it with `pcall`.
- A call is asynchronous: the script yields while it is in flight, and
  other work goes on.

## Budgets

| Budget | Default | On overrun |
| --- | --- | --- |
| Memory per VM | 16 MiB | The call fails with a memory budget error |
| Running time per call into the script | 50 ms | The interrupt hook stops it with a time budget error |
| Each host call | 5 s | The call fails with `-32004` (no answer within the deadline) |

The running-time budget counts only time the script itself runs. Time
spent waiting for host calls does not count, and each host call has its
own deadline instead. After an overrun the VM stays usable. What happens
next (terminate, report, restart) is up to whoever hosts the script.
