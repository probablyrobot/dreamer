# 0001. Luau as the script runtime

- Status: accepted
- Date: 2026-11-12
- Issue: #13

## Context

Dreamer's script tier runs filters, triggers, commands, and small pane apps
written by users and by agents. It needs a language with no compile step,
hot reload, a real sandbox (no ambient file, process, network, or environment
access), enforceable memory and time budgets, and low enough cost to host
hundreds of small scripts in the daemon. The candidates were Luau through
`mlua` and, as a fallback, PUC Lua 5.4 through the same binding.

## Decision

Use Luau through `mlua` 0.12.2 (`luau` and `vendored` features).

- **Isolation unit: one VM per extension package.** A package is a trust and
  grant boundary. Its scripts share that VM, each loaded into its own
  environment table, so they cannot see each other's globals. Packages never
  share a VM.
- **Sandbox:** Luau's safe libraries only, `Lua::sandbox(true)` (read-only
  built-ins and globals), and a read-only `dreamer` host table as the only
  way out. Every host call goes through the typed API under the package's
  grants.
- **Memory budget:** per VM (so per package), a hard allocation ceiling.
- **Time budget:** per call, enforced by the VM interrupt hook against a
  deadline.
- **Over budget:** the call fails with a budget error and the VM stays
  usable. The supervisor decides whether to restart the package with backoff.

PUC Lua 5.4 stays the fallback; nothing in the API schema depends on a
Luau-only feature yet.

## Evidence

`crates/script-runtime`, tests and examples at this commit.

Sandbox and budget tests (`cargo test -p script-runtime`), passing on macOS
arm64:

- `io`, `os.execute`, `os.getenv`, `os.remove`, `os.exit`, `package`,
  `dofile`, `loadfile` are absent; `require('os')` fails.
- Writing to `string.rep` or to the host table fails; script globals stay
  private and writable.
- `while true do end` is stopped by a 30 ms budget and the VM still runs code.
- Allocating past a 4 MiB budget fails with a memory error and the VM still
  runs code after a collection.

Measurements, release build, macOS 27.2 on Apple M3 Max:

| Measure | Result | Source |
| --- | --- | --- |
| Create a sandboxed VM | 84–130 µs | `luau_gate` |
| Idle heap per VM | 436,584 bytes (379,312 with no libraries) | `luau_gate` |
| Extra script in a shared VM | 604 bytes | `luau_shared` |
| Script calling a host function | 28 ns per call | `luau_gate` |
| Rust calling a script handler with a string | 240–260 ns per call | `luau_shared` |
| Runaway loop past a 20 ms budget | about 0.1 ms overrun | `luau_gate` |

The idle VM cost is almost entirely fixed state that exists before any library is
loaded, which is why the isolation unit is the package and not the script:
500 one-script VMs would cost about 220 MB, while 500 scripts across a
handful of packages cost a few megabytes.

Linux x86-64 (CI) and ARM64 (cross-built, emulated tests) results: recorded
in the pull request that adds this crate to the headless build. Emulated
numbers are not performance evidence.

## Consequences

- Scripts in one package can exhaust that package's shared memory budget;
  that is acceptable because they share a grant and an author.
- Per-call time budgets work for shared VMs because a VM runs one call at a
  time on its worker.
- Luau is C++: cross builds need a C++ cross compiler.
- Revisit if a package needs truly isolated scripts (spawn more VMs) or if
  `mlua` gains a cheaper state.
