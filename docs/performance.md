# Performance

Measured results from the benchmark harness, on named hardware, against
the targets in the specification (sections 16.5 and 16.6). A target with
no measurement here is not claimed.

## Running the harness

```sh
cargo xtask bench headless [--panes N] [--history LINES]  # this machine
cargo xtask bench headless --panes 1000 --linux           # the gate's ARM64 Linux container
cargo xtask bench busy [--panes N] [--idle M]             # typing under load, split by process
```

The harness builds `dreamerd` and `dreamer-bench` in release mode. It runs
them against a private tmux server and private daemon directories, never
the user's. It prints a table and writes the results, with the machine they
ran on, to `target/bench/<workload>-<time>.json`.

**What the headless workload does, in order:**

1. **One pane.** Start the daemon and measure its memory.
2. **Key to echo, one pane.** 200 keystrokes, each timed until it is
   visible. The path covers:
   - a client replica sending the key;
   - the daemon encoding it;
   - tmux delivering it;
   - the pty echoing it (`cat` in its own window);
   - tmux sending the echo;
   - the daemon's mirror;
   - the replica's frame, whose cursor must move.

   No GPU or paint is in this path.
3. **N idle panes.** Create N shells and time until every pane has a live
   mirror. Measure memory and the daemon's CPU over 10 s. With
   `--history LINES`, each shell first prints that many lines, so its
   mirror holds a screen and a history as a pane in use does.
4. **Cold.** Demote every pane to Cold. Measure memory and idle CPU again.
5. **Open a Cold pane.** For 100 Cold panes, time a read, which attaches
   the pane again before answering.
6. **Typing under load.** Start 63 panes running `yes` and repeat the
   key-to-echo measurement. Measure the daemon's CPU meanwhile.

**The busy workload** repeats step 6 on its own, first unloaded and then
with N panes (63 by default) running `yes`, after `M` idle shells with
`--idle M`. Each time it also measures:

- tmux's own key to echo: `send-keys` into a `cat` pane, then
  `capture-pane` until the key is in tmux's grid;
- the CPU used by tmux and by the daemon;
- the same tmux measure with the daemon stopped.

The tmux measure includes starting two tmux clients per sample (about
1 ms on Linux and 11 ms on macOS), so compare it with its own unloaded
value.

**Memory** is reported two ways. Resident size (RSS) is what the system
sees. It also counts freed memory the allocator keeps for reuse, so after
demotion it overstates what the daemon holds. Heap in use comes from the
allocator (`dream status` shows it too). It counts what is really held,
but not the terminal pages that Ghostty maps outside the allocator. A
per-pane figure is the growth over the one-pane daemon divided by the pane
count. A Warm pane is best measured by RSS and a Cold pane by heap.

After a demotion the daemon hands freed memory back to the system:
`malloc_trim` on Linux, and malloc pressure relief on macOS. macOS takes the
pages back only under memory pressure, so the Cold RSS figure there stays
high. On Linux it falls to near the heap figure.

## Results

### macOS, 400 panes

- **Machine:** Mac15,11 (Apple M3 Max, 14 cores, 36 GiB), Darwin 27.2.0
  arm64.
- **Revision:** branch `m7-05-loaded-typing`.
- **Why 400:** macOS allows 511 pseudo-terminals for the whole system
  (`kern.tty.ptmx_max`), so 1,000 panes cannot exist on a Mac without
  raising that limit.

| Measure | Empty shells | 2,000 lines of history | Target |
| --- | --- | --- | --- |
| Key to echo, one pane, p50 / p95 | 0.22 / 0.28 ms | 0.23 / 0.31 ms | p95 under 30 ms end to end (the paint is not in this path): met |
| Mirror 400 new panes | 1.22 s | 8.0 s | — |
| Daemon RSS, 1 pane / 400 Warm panes | 6.1 / 23.8 MiB | 6.1 / 809 MiB | — |
| Warm pane, RSS / heap | 45 / 19 KiB | 2,056 / 22 KiB | Measured and published |
| Daemon CPU, 400 idle Warm panes | 0.0 % of a core | 0.0 % | Under 1 % for 256 idle panes: met |
| Demote 400 panes to Cold | 0.02 s | 0.04 s | — |
| Cold pane, heap / RSS | 2.2 / 29 KiB | 2.7 / 34 KiB | At least 20 times smaller than Warm: met with history (60 times by RSS); see below for empty shells |
| Daemon CPU, 400 idle Cold panes | 0.1 % of a core | 0.1 % | — |
| Open a Cold pane, p50 / p95 | 24 / 27 ms | 22 / 24 ms | p95 under 300 ms: met |
| Key to echo with 63 busy panes, p50 / p95 | 13.7 / 14.6 ms | 13.7 / 14.4 ms | Within the one-pane budget plus 10 ms (40 ms): met |
| Daemon CPU with 63 busy panes | 46 % of a core | 44 % | — |

### Linux, 1,000 panes

- **Machine:** the gate's ARM64 Linux container, Ubuntu 24.04, Linux
  6.8.0 aarch64. It ran in a Colima VM with 4 cores and 8 GiB on the Mac
  above.
- **Command:** `cargo xtask bench headless --panes 1000 [--history 2000] --linux`.
- **Revision:** branch `m7-05-loaded-typing`.

| Measure | Empty shells | 2,000 lines of history | Target |
| --- | --- | --- | --- |
| Key to echo, one pane, p50 / p95 | 0.61 / 1.33 ms | 0.62 / 0.99 ms | p95 under 30 ms end to end (the paint is not in this path): met |
| Mirror 1,000 new panes | 1.85 s | 34 s | — |
| Daemon RSS, 1 pane / 1,000 Warm panes | 9.2 / 32.6 MiB | 9.2 / 1,888 MiB | — |
| Warm pane, RSS / heap | 24 / 18 KiB | 1,924 / 21 KiB | Measured and published |
| Daemon CPU, 1,000 idle Warm panes | 0.1 % of a core | 0.1 % | Under 1 % for 256 idle panes: met |
| Demote 1,000 panes to Cold | 0.13 s | 0.22 s | — |
| Cold pane, heap / RSS | 2.7 / 6.8 KiB | 3.1 / 6.8 KiB | At least 20 times smaller than Warm: met with history (280 times by RSS); see below for empty shells |
| Daemon CPU, 1,000 idle Cold panes | 0.0 % of a core | 0.1 % | — |
| Open a Cold pane, p50 / p95 | 32 / 55 ms | 29 / 44 ms | p95 under 300 ms: met |
| Key to echo with 63 busy panes, p50 / p95 | 125 / 129 ms | 135 / 140 ms | 40 ms: **not met** with 1,000 panes on the tmux server; see below |
| Daemon CPU with 63 busy panes | 12 % of a core | 11 % | — |

### Typing under load, split between tmux and the daemon

Machine and container as above, with `cargo xtask bench busy --idle M
--linux`: 63 panes running `yes`, after `M` idle shells.

| Idle panes on the tmux server | Key to echo through the daemon, p50 / p95 | Key in tmux's grid, p50 / p95 | tmux CPU | Daemon CPU |
| --- | --- | --- | --- | --- |
| 0 | 21 / 24 ms | 21 / 23 ms | 99 % | 51 % |
| 250 | 39 / 42 ms | 40 / 43 ms | 100 % | 28 % |
| 500 | 65 / 68 ms | 66 / 69 ms | 100 % | 21 % |
| 1,000 | 136 / 160 ms | 137 / 145 ms | 100 % | 12 % |

On the Mac with 400 idle panes, the same run gave 13.7 / 14.3 ms through
the daemon, with tmux at 100 % of a core.

### Desktop, macOS (M3 Max, as above)

`cargo xtask bench gui --panes N` creates an isolated tmux server with N
panes, at most 16 to a tab, the first tab visible. It starts the daemon
first, then a release `dreamer`, which measures itself
(`crates/app/src/bench_gui.rs`):
- **Ends at a paint.** Each measure stops when the pane's new frame has
  been painted, not when it arrived.
- **Load.** Under load, every other visible pane prints a line every
  20 ms.
- **Needs a display.** It must be awake and unlocked, with the window in
  front.

Revision: branch `m7-05-launch-and-tabs`.

| Measure | 1 pane | 16 panes | 64 panes | 256 panes | Target |
| --- | --- | --- | --- | --- | --- |
| Visible panes | 1 | 16 | 16 | 16 | — |
| Launch to every visible pane painted (daemon running) | 181 ms | 181 ms | 190 ms | 205 ms | Under 150 ms: **not met**; see below |
| Terminal paints over 3 s with nothing changing | 0 | 0 | 0 | 0 | Zero: met |
| Key to echo including paint, idle, p50 / p95 | 2.1 / 3.2 ms | 2.1 / 3.7 ms | 2.1 / 4.4 ms | 2.1 / 4.3 ms | p95 under 30 ms: met |
| Key to echo including paint, 15 busy panes, p50 / p95 | — | 2.1 / 3.5 ms | 2.1 / 3.4 ms | 2.1 / 3.7 ms | One-pane budget plus 10 ms: met |
| Terminal paint time per frame, 15 busy panes, p95 | — | 1.5 ms | 1.7 ms | 1.6 ms | Under 8 ms: met |
| Time between frames, 15 busy panes, p95 | — | 17 ms | 25 ms | 25 ms | — |
| Tab switch to every pane painted, p50 / p95 | — | — | 8.9 / 12.8 ms | 8.7 / 9.9 ms | p95 under 50 ms: met |

## Findings

- **Typing under load is bounded by tmux, not the daemon.**
  - At every size, the time for a key to reach a Dreamer client matches
    the time for it to appear in tmux's own grid, within the noise.
  - tmux is single-threaded and at 100 % of a core. With the daemon
    stopped it is just as slow (95 ms with 1,000 idle panes).
  - The delay grows with the number of panes on the tmux server, by about
    0.11 ms per pane on this 4-core VM. After every pass of its event loop,
    tmux walks every window and pane (`server_client_loop` in tmux 3.8:
    resize, style, buffer, and name checks). With 63 panes writing
    nonstop, it makes many passes a second.
  - Turning `automatic-rename` off changed nothing.

  **Revised target:** the daemon adds no measurable delay over tmux's own
  key to echo under load. The 40 ms budget holds with up to about 250
  panes on one tmux server on a 4-core Linux machine. On the 14-core Mac it
  holds with 400 panes (14 ms). More panes on a heavily loaded server need
  a faster tmux, or the panes split across tmux servers.
- **Cold against Warm memory depends on what the pane holds.**
  - A Cold pane holds about 3 KiB of heap whatever it held before. On
    Linux, once the freed memory is returned, that is about 7 KiB of RSS.
  - A Warm pane's mirror grows with its screen and history, by about
    1 KiB per 120-column line kept. With 2,000 lines, Cold is 60 times
    (macOS) to 280 times (Linux) smaller by RSS.
  - An empty idle shell's mirror is small to begin with (18 to 24 KiB on
    Linux), so there the ratio is 3.5 times by RSS on Linux and 8 times by
    heap.

  **Revised target:** a Cold pane holds under 4 KiB of heap and, on Linux,
  under 8 KiB of RSS, independent of its history. The 20-times ratio holds
  for any pane with a screenful of content.
- **Warm panes with long histories are expensive.** 1,000 Warm panes with
  2,000 lines each take 1.9 GB. Demoting panes nobody watches to Cold is
  what keeps a large daemon small.
- **Launch: 1.33 s, then 0.2 s, now about 185 ms.**
  - **Fonts.** Choosing the terminal font asked GPUI for every installed
    font name, twice, at about 0.6 s each on macOS. It now resolves the
    wanted family alone. It also parsed all five shipped families (46 MB)
    to use one; only the configured family's files are read now, on a
    thread started at launch, while GPUI starts up.
  - **Daemon and themes.** The daemon connection and the first topology
    start at process start too. The theme list, which only the palette's
    Theme command needs, is read after the window opens.
  - **What remains.** Most of it is outside Dreamer's code: GPUI's
    platform start (about 58 ms) and the window's first frame (about
    60–70 ms: Metal setup, first layout, and shaping).
  - **Revised target: 200 ms** with the daemon running. 150 ms would take
    changes to GPUI's startup.
- **Tab switch: 65–80 ms, now about 10 ms.**
  - **Before.** A switch waited for tmux's confirmation, then subscribed
    every pane of the tab again.
  - **Now.** Panes in hidden tabs keep their views: up to 48 of them, the
    spec's Warm client tier. A clicked tab is shown at once from those
    last frames while tmux confirms. tmux stays authoritative: if it
    reports another window as active, that is what is shown.
- **Attaching many panes at once failed some of them.** Each attach is a
  few command batches on the daemon's one tmux control connection, whose
  queue holds 256. Creating hundreds of panes together overflowed it, and
  the overflowed panes stayed failed ("command queue full"). The router now
  runs at most 16 attaches at once and queues the rest.
  `three_hundred_new_panes_all_become_live` fails without the limit and
  passes with it.
- **tmux cannot switch a pane's output off** for the daemon without
  stalling the pane when the daemon is its only client. A Cold pane's
  output is dropped in the daemon instead (see [mirrors.md](mirrors.md#tiers)).

## Not measured yet

- **Desktop workloads on Linux** (Wayland and X11), and on macOS with more
  than 16 visible panes.
- **Image-heavy and escape-heavy streams.**
- **Remote latency and jitter.**
- **The same workloads in reference terminals.**
