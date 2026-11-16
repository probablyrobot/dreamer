# Performance

Measured results from the benchmark harness, on named hardware, against
the targets in the specification (sections 16.5 and 16.6). A target with
no measurement here is not claimed.

## Running the harness

```sh
cargo xtask bench headless [--panes N]           # this machine
cargo xtask bench headless --panes 1000 --linux  # the gate's ARM64 Linux container
```

The harness builds `dreamerd` and `dreamer-bench` in release mode. It runs
them against a private tmux server and private daemon directories, never
the user's. It prints a table and writes the results, with the machine they
ran on, to `target/bench/headless-<time>.json`.

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
   mirror. Measure memory and the daemon's CPU over 10 s.
4. **Cold.** Demote every pane to Cold. Measure memory and idle CPU again.
5. **Open a Cold pane.** For 100 Cold panes, time a read, which attaches
   the pane again before answering.
6. **Typing under load.** Start 63 panes running `yes` and repeat the
   key-to-echo measurement. Measure the daemon's CPU meanwhile.

Memory is the daemon's resident set size. A per-pane figure is the
growth over the one-pane daemon divided by the pane count. After demotion
the allocator may keep pages it no longer uses, so the Cold figure is an
upper bound.

## Results

### macOS, 400 panes

- **Machine:** Mac15,11 (Apple M3 Max, 14 cores, 36 GiB), Darwin 27.2.0
  arm64.
- **Revision:** branch `m7-04-cold-tier-bench`.
- **Why 400:** macOS allows 511 pseudo-terminals for the whole system
  (`kern.tty.ptmx_max`), so 1,000 panes cannot exist on a Mac without
  raising that limit.

| Measure | Result | Target |
| --- | --- | --- |
| Key to echo, one pane, p50 / p95 | 0.24 / 0.33 ms | p95 under 30 ms end to end (the paint is not in this path) |
| Mirror 400 new panes | 1.27 s | — |
| Daemon memory, 1 pane / 400 Warm panes | 6.1 / 23.1 MiB | — |
| Memory per Warm pane | 43 KiB | Measured and published |
| Daemon CPU, 400 idle Warm panes | 0.0 % of a core | Under 1 % for 256 idle panes: met |
| Demote 400 panes to Cold | 0.03 s | — |
| Memory per Cold pane (upper bound) | 28 KiB | At least 20 times smaller than Warm: **not met** (1.6 times, as RSS) |
| Daemon CPU, 400 idle Cold panes | 0.1 % of a core | — |
| Open a Cold pane, p50 / p95 | 23 / 29 ms | p95 under 300 ms: met |
| Key to echo with 63 busy panes, p50 / p95 | 13.6 / 14.4 ms | Within the one-pane budget plus 10 ms (40 ms): met |
| Daemon CPU with 63 busy panes | 40 % of a core | — |

### Linux, 1,000 panes

- **Machine:** the gate's ARM64 Linux container, Ubuntu 24.04, Linux
  6.8.0 aarch64. It ran in a Colima VM with 4 cores and 8 GiB on the Mac
  above.
- **Command:** `cargo xtask bench headless --panes 1000 --linux`.
- **Revision:** branch `m7-04-cold-tier-bench`.

| Measure | Result | Target |
| --- | --- | --- |
| Key to echo, one pane, p50 / p95 | 0.62 / 1.24 ms | p95 under 30 ms end to end (the paint is not in this path) |
| Mirror 1,000 new panes | 1.73 s | — |
| Daemon memory, 1 pane / 1,000 Warm panes | 9.1 / 33.3 MiB | — |
| Memory per Warm pane | 25 KiB | Measured and published |
| Daemon CPU, 1,000 idle Warm panes | 0.1 % of a core | Under 1 % for 256 idle panes: met |
| Demote 1,000 panes to Cold | 0.15 s | — |
| Memory per Cold pane (upper bound) | 21 KiB | At least 20 times smaller than Warm: **not met** (1.2 times, as RSS) |
| Daemon CPU, 1,000 idle Cold panes | 0.1 % of a core | — |
| Open a Cold pane, p50 / p95 | 34 / 55 ms | p95 under 300 ms: met |
| Key to echo with 63 busy panes, p50 / p95 | 121 / 124 ms | Within the one-pane budget plus 10 ms (40 ms): **not met** |
| Daemon CPU with 63 busy panes | 12 % of a core | — |

With 63 panes running `yes` on 4 cores, typing is slow, but the daemon
is mostly idle. The time goes elsewhere, most likely to tmux and the
busy programs themselves competing for the cores. Finding where is the
next step (#66). On the 14-core Mac the same load stays within the budget.

## Findings

- **Attaching many panes at once failed some of them.** Each attach is a
  few command batches on the daemon's one tmux control connection, whose
  queue holds 256. Creating hundreds of panes together overflowed it, and
  the overflowed panes stayed failed ("command queue full"). The router now
  runs at most 16 attaches at once and queues the rest.
  `three_hundred_new_panes_all_become_live` fails without the limit and
  passes with it.
- **A Cold pane is not 20 times smaller than a Warm one, as measured.** An
  idle Warm pane's mirror is small to begin with (about 43 KiB with a short
  history), and freed memory is not all returned to the system. The target
  needs a measurement that separates the two, or a revised number (#66).
- **tmux cannot switch a pane's output off** for the daemon without
  stalling the pane when the daemon is its only client. A Cold pane's
  output is dropped in the daemon instead (see [mirrors.md](mirrors.md#tiers)).

## Not measured yet

- **GUI workloads (1, 16, 64, and 256 observed panes):** frame time,
  launch to first interactive frame, tab switch, and key to visible echo
  including paint. These need a display that is awake and unlocked.
- **Image-heavy and escape-heavy streams.**
- **Remote latency and jitter.**
- **The same workloads in reference terminals.**
