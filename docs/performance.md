# Performance

Measured results from the benchmark harness (`cargo xtask bench`), on
named hardware, against Dreamer's performance targets. A target with no
measurement here is not claimed.

## Summary

Two machines have results:

- **macOS:** Mac15,11 (Apple M3 Max, 14 cores, 36 GiB), Darwin 27.2.0
  arm64.
- **Linux ARM64:** an Ubuntu 24.04 container (Linux 6.8.0 aarch64) in a
  VM with 4 cores and 8 GiB on that Mac. It has no display, so only the
  headless workloads ran there.

| Measure | macOS | Linux ARM64 | Target |
| --- | --- | --- | --- |
| Key to echo, one pane, no paint, p50 / p95 | 0.22 / 0.28 ms | 0.61 / 1.33 ms | p95 under 30 ms: met |
| Key to echo including paint, one pane, p50 / p95 | 2.1 / 3.2 ms | No measurement yet | p95 under 30 ms: met on macOS |
| Key to echo with 63 busy panes, p50 / p95 | 13.7 / 14.6 ms (400 panes on the tmux server) | 125 / 129 ms (1,000 panes on the tmux server) | 40 ms: met on macOS, **not met** on Linux with 1,000 panes |
| Key to echo through a host over OpenSSH, p50 / p95 | 1.06 / 1.73 ms (the Mac to the Linux container) | — | — |
| Launch to every visible pane painted, daemon running | 181 ms (1 or 16 panes) | No measurement yet | Under 150 ms: **not met** |
| Daemon RSS with one pane | 6.1 MiB | 9.2 MiB | — |
| Warm pane with 2,000 lines of history, RSS | 2,056 KiB | 1,924 KiB | Measured and published |
| Cold pane, heap | 2.2 to 2.7 KiB | 2.7 to 3.1 KiB | At least 20 times smaller than Warm: met with history |

No measurement yet:

- **x86-64 Linux,** apart from the screen frames runs under
  [Screen frames for busy panes](#screen-frames-for-busy-panes). CI runs
  the harness there only as a debug-build check and discards the numbers.
- **The desktop on Linux** (Wayland and X11): launch, paint, and key to
  echo including paint on an unshared GPU.
- More under [Not measured yet](#not-measured-yet).

The runs behind each number are under [Results](#results), and what they
led to under [Findings](#findings). The results record the revision each
run was built from, but not the day it ran.

## Results

### macOS, 400 panes

- **Machine:** Mac15,11 (Apple M3 Max, 14 cores, 36 GiB), Darwin 27.2.0
  arm64.
- **Revision:** a development branch; the commit was not recorded.
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
- **Revision:** the same development branch as the macOS run.

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

### One control client per session

`cargo xtask bench busy --panes 15 --idle 200 --sessions S` on the Mac
above: 200 idle shells, in the first session (one client) or spread over
`S` more sessions, then 15 panes running `yes`. Revision: a development
branch (the commit was not recorded), release build.

| Measure | 1 session | 10 more | 50 more | 200 more |
| --- | --- | --- | --- | --- |
| tmux clients (the daemon's) | 1 | 11 | 51 | 201 |
| Daemon memory (RSS), idle | 16.2 MiB | 17.1 MiB | 18.8 MiB | 28.0 MiB |
| tmux memory (RSS), idle | 6.0 MiB | 6.8 MiB | 11.0 MiB | 26.8 MiB |
| tmux CPU, idle | 0.2 % | 0.0 % | 0.6 % | 7.8 % |
| Key to echo, no load, p50 / p95 | 0.49 / 0.60 ms | 0.61 / 0.70 ms | 1.37 / 1.52 ms | 5.79 / 6.14 ms |
| Key to echo, 15 busy panes, p50 / p95 | 1.89 / 2.24 ms | 1.95 / 2.12 ms | 2.60 / 3.17 ms | 9.09 / 10.7 ms |
| Daemon CPU, 15 busy panes | 53 % | 51 % | 32 % | 9 % |
| tmux CPU, 15 busy panes | 100 % | 100 % | 100 % | 100 % |

tmux is at a full core under load in every column. With more clients it
spends more of that core on them, so less output reaches the daemon
(its CPU falls) and keys take longer. Up to 10 sessions the cost is
within noise.

### Tiers, memory, and extensions (M3 Max, as above)

Revision `d2c668ac90ae` (branch `main`), release build, each workload
with its defaults, one after another while nothing else ran.

**A pane's cost in each tier** (`bench tiers`: 16 panes, `yes` at the
workload's rate):

| Measure | Cold | Warm | Hot |
| --- | --- | --- | --- |
| Daemon heap per idle pane | 3.2 KiB | 19.9 KiB | 102 KiB |
| Daemon CPU per busy pane | 0.11 % | 0.41 % | 0.69 % |
| Daemon heap per busy pane | 3.3 KiB | 4,903 KiB | 4,698 KiB |
| Client replica CPU, 16 busy panes | — | — | 18.5 % (5.3 % per MiB/s) |
| Client replica RSS per pane, idle / busy | — | — | 771 / 2,934 KiB |

A busy Cold pane costs the daemon 26 % of a busy Warm pane's CPU: tmux
still sends its output, which the daemon reads and drops. tmux's own CPU
is the same in every tier (14 % for the 16 panes).

**Memory under pressure** (`bench memory`, 300 s a scenario). The output and
lagging rows are from 0.12.1 (e9c7e71) on the M3 Max. On macOS the bound is
checked on the peak footprint (see below):

| Scenario | Peak footprint | Peak RSS | Stated bound | Heap in use, peak | Bound |
| --- | --- | --- | --- | --- | --- |
| Sustained output, 64 panes | 582 MiB | 1,298 MiB | 910 MiB | 319 MiB | met |
| Lagging viewer, 64 busy panes | 639 MiB | 2,412 MiB | 998 MiB | 369 MiB | met |
| Paste of 10 MiB | — | 43 MiB | 47 MiB | 12 MiB | met (on RSS) |
| Extension crash loop (382 crashes) | — | 8.8 MiB | 40 MiB | 0.6 MiB | met (on RSS) |

- The journal stayed at its 256 MiB host limit, and the heap in use
  stayed flat. Every stalled subscription ended as `lagged`, and every
  resume from its start was answered with a checkpoint.
- RSS on macOS climbs far past the footprint (2.4 GiB against 639 MiB in the
  lagging scenario) because it also counts free pages the allocator keeps
  and the system has not taken back. On Linux (i9-12900K, glibc), the same
  64-pane output scenario peaked at 537 MiB RSS against a bound of 917 and
  was flat over the second half.
- The footprint still rose 28 to 30 MiB over the second half of each macOS
  run while the heap stayed flat; whether it levels off over longer runs
  has not been measured.
- The paste is delivered at 0.36 MiB/s into a program that reads and
  discards it.

**With a Warm memory ceiling** (`bench memory --scenario output
--warm-memory 64`: the same 64 busy panes, `tiers.warm_memory: 64MiB`,
one pane followed by a replica that takes frames throughout; the daemon
as above, with the bench's `--warm-memory` option added):

| Measure | Result |
| --- | --- |
| Panes Cold, most at once / at the end | 59 / 59 |
| Followed pane went Cold | no (32,950 frames taken) |
| Mirrors' memory, daemon's estimate, largest after a demotion round | 62 MiB |
| Daemon RSS, peak / at the end | 577 / 97 MiB |
| Heap in use, peak / at the end | 319 / 60 MiB |

The ceiling held: demotion kept the daemon's estimate under 64 MiB, the
followed pane stayed Warm, and peak RSS stayed within the 910 MiB bound.
RSS fell by 220 MiB over the second half once the quiet panes were Cold,
so the memory outside the heap belongs to Warm mirrors and goes with
them.

**Slow and stalled extensions** (`bench extensions`: 63 busy panes, a
quiet pane typed into):

| Measure | Quiet | 63 busy panes | Busy, monitor open | Busy, every slow extension |
| --- | --- | --- | --- | --- |
| Key to echo, p50 / p95 | 0.17 / 0.23 ms | 6.78 / 7.15 ms | 6.81 / 7.83 ms | 6.73 / 7.51 ms |
| Daemon CPU | 0.1 % | 124 % | 125 % | 227 % |
| Busy pane-seconds with no mirror progress, of 630 | — | 0 | 0 | 0 |

Key to echo with every slow extension stays within the one-pane budget
plus 10 ms (7.5 ms against 40 ms), and within this run's quiet p95 plus
10 ms. The monitor adds 1.5 % of a core and 1.2 MiB of daemon heap under
load, and nothing measurable when quiet. The slow extensions add 102 % of
a core, most of it the Luau scripts.

### Desktop, macOS (M3 Max, as above)

```sh
cargo xtask bench gui [--panes N] [--visible N] [--load text|escape|image] [--backend wayland|x11]
```

`bench gui` creates an isolated tmux server with N panes (16 by default).
The first tab holds `--visible` of them (16 by default, at most 64),
tiled and shown; the rest go 16 to a tab. It starts the daemon first,
then a release `dreamer`, which measures itself
(`crates/app/src/bench_gui.rs`):
- **Ends at a paint.** Each measure stops when the pane's new frame has
  been painted, not when it arrived.
- **Load.** Under load, every other visible pane prints a line every
  20 ms. With `--load`, they replay a streams corpus nonstop with `cat`
  instead: `text`, `sgr` for `escape`, or `images` for `image`. The
  corpora are written for 120x40 panes; tiled panes are smaller, so
  lines wrap and images are clipped. The results record which corpus,
  with its size and hash.
- **The frame, split.** For each window frame under load, from the
  workspace's render to its last element painted: the whole frame build
  (render, layout, and paint), the terminal panes' paint, the shaping of
  rows the cache did not hold, the conversion of new images for the GPU,
  and the rest (layout, chrome, building the element tree). GPU
  submission and present are not in it: GPUI times them only with its
  `profiler` feature, which Dreamer does not build. The results say so.
- **Display backend.** On Linux it runs on Wayland when the session
  offers it, else X11; `--backend` picks one (`x11` runs through
  XWayland on a Wayland session). The results record the backend the app
  drew on.
- **Needs a display.** It must be awake and unlocked, with the window in
  front. When it is not, the results file says `"not_run": true` with the
  reason and holds no results, and the command exits 5, as the gate's
  smoke runs do. That happens when the screen is locked (macOS), there
  is no display server or not the one asked for (Linux), or the window
  does not draw when asked: after launch, after the idle count, under
  load, or when a wait runs out. `bench report` refuses such a file.

Revision: a development branch; the commit was not recorded.

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

### Remote latency, through OpenSSH to an ARM64 Linux container

`cargo xtask bench remote --container`, and again with `--netem 'delay
20ms 5ms'`: the desktop side on the Mac above, the host in the gate's
ARM64 Linux container (Linux 6.8 under Colima), reached through
OpenSSH 10.5p1 on a published loopback port. 200 keystrokes each.
Revision `8e9dd7ade6d5` (branch `main`).

| Measure | No added delay | netem delay 20 ms ± 5 ms |
| --- | --- | --- |
| Connect to the host (SSH, bridge, handshake) | 0.19 s | 0.48 s |
| Link round trip (`hosts.call daemon.version`), p50 / p95 | 1.24 / 2.51 ms | 28.6 / 36.1 ms |
| Link round trip, jitter | 0.13 ms | 5.57 ms |
| Key to echo through the host, p50 / p95 | 1.06 / 1.73 ms | 29.1 / 32.5 ms |
| Key to echo through the host, jitter | 0.14 ms | 2.55 ms |
| Host processing (sum of its stage p50s) | 0.45 ms | 1.78 ms |
| Link, relay and replica (end-to-end p50 less host processing) | 0.96 ms | 27.5 ms |
| Key to echo, local pane in the same run, p50 / p95 | 0.27 / 0.43 ms | 0.28 / 0.38 ms |

Host processing is mostly the host's `input` and `tmux` stages (238 and
156 µs p50 without delay); the daemon's own stages after tmux take under
60 µs together. With the delay, every host stage took about four times
as long, the container being idle between keystrokes. Even so, the link
is nearly all of the end-to-end time. netem's jitter reorders packets, so
TCP adds to the 20 ms it puts on each round trip.

### Screen frames for busy panes

With `render.frames: on`, the daemon sends a desktop window the screen of
a pane that writes fast instead of every byte, and the window keeps no
terminal engine of its own for it
([settings.md](settings.md#screen-frames)). These runs compare frames off
and on.

Revision: f3b738e, with the frame thresholds of this release (512 KiB/s
to frames, 128 KiB/s back). Workload: `cargo xtask bench gui --load L`,
16 panes, 15 of them busy. CPU is the busiest 5 s of the run, as a share
of one core, sampled from the processes; memory is the peak (RSS on
macOS, PSS on Linux). One run each unless noted.

**macOS** (M3 Max, as above):

| Load | Frames | Desktop CPU | Daemon CPU | Desktop memory | Daemon memory | Key to echo, p50 / p95 |
| --- | --- | --- | --- | --- | --- | --- |
| text | off | 272 % | 131 % | 184 MiB | 191 MiB | 16.1 / 21.0 ms |
| text | on | 141 % | 99 % | 180 MiB | 194 MiB | 9.7 / 17.2 ms |
| escape | off | 215 % | 75 % | 281 MiB | 201 MiB | 45.2 / 53.5 ms |
| escape | on | 138 % | 58 % | 278 MiB | 205 MiB | 47.2 / 53.2 ms |
| image | off | 302 % | 159 % | 346 MiB | 254 MiB | 8.4 / 25.1 ms |
| image | on | 134 % | 116 % | 283 MiB | 365 MiB | 3.6 / 23.1 ms |

The macOS text and image runs used the earlier thresholds (1 MiB/s and
256 KiB/s); both loads are far above either.

**Linux** (grumpy: Intel i9-12900K, 24 threads, 128 GiB, Arch Linux
7.2.15, Hyprland 0.56.2). A training job shared the GPU during these runs
(0 to 99 % busy, 52 % on average), so frame counts, paint times, and key
to echo from this machine are not reported. CPU did not move with it: the
three Wayland runs of each text and image case were within 3 points of
each other.

| Load | Frames | Wayland desktop / daemon CPU | X11 desktop / daemon CPU | Daemon memory (Wayland) |
| --- | --- | --- | --- | --- |
| text | off | 171 to 173 % / 132 % (3 runs) | 146 % / 111 % | 173 MiB |
| text | on | 96 to 98 % / 120 to 125 % (3 runs) | 94 % / 113 % | 179 to 184 MiB |
| escape | off | 119 % / 41 % | 119 % / 48 % | 169 MiB |
| escape | on | 84 % / 52 % | 83 % / 55 % | 173 MiB |
| image | off | 170 to 172 % / 173 % (3 runs) | 166 % / 141 % | 264 to 271 MiB |
| image | on | 86 to 87 % / 131 to 133 % (3 runs) | 98 % / 116 % | 332 to 336 MiB |

**Against other terminals** (`cargo xtask bench compare`, grumpy, Wayland,
same GPU sharing). Every terminal draws the same isolated tmux session
with its default settings; the busy phase is 16 panes, 15 of them looping
`cat` of a 64 MiB text file, for 20 s. Memory is PSS. No window had
keyboard focus: on Hyprland over SSH the new window opens on its own
workspace but focus stays on the previous window. Dreamer limits an
unfocused window to 30 frames a second; the others' limits were not
checked.

| Terminal | Idle memory | `cat` 64 MiB | Busy CPU | Busy memory | tmux CPU |
| --- | --- | --- | --- | --- | --- |
| Dreamer, frames off | 206 MiB | 1.14 s | 299 % (desktop 163 %, daemon 136 %) | 540 MiB | 93 % |
| Dreamer, frames on | 205 MiB | 1.10 s | 179 % (desktop 37 %, daemon 142 %) | 511 MiB | 98 % |
| Ghostty 1.4.0 | 161 MiB | 0.99 to 1.29 s | 62 to 73 % | 172 MiB | 96 to 99 % |
| Alacritty 0.17.0 | 106 MiB | 1.02 to 1.08 s | 33 to 36 % | 120 MiB | 99 % |
| kitty 0.49.2 | 140 to 142 MiB | 0.94 to 0.99 s | 47 to 51 % | 141 MiB | 97 to 99 % |
| foot 1.28.0 | 39 MiB | 1.29 to 1.32 s | 341 to 506 % | 40 MiB | 97 to 98 % |

The other terminals' ranges are from the two runs (one with Dreamer's
frames off, one on). A second pair of Dreamer runs gave 306 % and 178 %.

## Findings

- **Screen frames halve the desktop's cost on busy panes, and the
  daemon does not pay it back.** With 15 busy panes the desktop's CPU fell
  by 35 to 80 % on both machines and every load. The daemon's moved far
  less: down by up to 43 points (images on macOS), up by at most 11
  (the escape load on Linux), as it builds a screen at most once per frame
  but no longer sends every byte. That is why `render.frames` is `on` by default. Two costs
  remain:
  - Under a constant stream of images the daemon holds 60 to 110 MiB more
    at its peak, from the pixels it sends with frames; it levels off.
  - Key to echo under the escape load stays about 47 ms on macOS: drawing
    colour-heavy cells takes about 22 ms a frame, which frames do not
    change.
- **Against other terminals, the daemon is now most of the gap.** With
  frames on, Dreamer's desktop used 37 % of a core in the comparison
  above, close to Ghostty, Alacritty, and kitty (33 to 73 %), but the
  daemon still parses every byte (about 140 %). That parse keeps every
  pane's history, search, and journal for agents and filters, so it stays.

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
  what keeps a large daemon small: `tiers.warm_memory` (1 GiB by default)
  caps what all mirrors hold together, and the benchmark turns it off to
  measure Warm panes.
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
    client's Warm tier. A clicked tab is shown at once from those
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

- **Headless workloads on x86-64 Linux.** Those Linux results are from an
  ARM64 container in a VM.
- **Stage timings on named hardware.** `bench headless` prints them; no
  run is recorded here yet.
- **Desktop paint and latency on Linux** (Wayland and X11) on an unshared
  GPU, and desktop workloads on macOS with more than 16 visible panes.
- **Image-heavy and escape-heavy streams:** the `streams` workload
  exists, but no results are recorded here yet.
- **Reference terminals on macOS,** and with keyboard focus on Linux.
- **Bytes sent to a remote client under load,** frames off and on.

## Running the harness

```sh
cargo xtask bench headless [--panes N] [--history LINES]  # this machine
cargo xtask bench headless --panes 1000 --linux           # the gate's ARM64 Linux container
cargo xtask bench busy [--panes N] [--idle M [--sessions S]]  # typing under load, split by process
cargo xtask bench streams [--panes N] [--corpus NAME]... [--seconds S]  # replayed corpora
cargo xtask bench tiers [--panes N] [--stream text|escape] [--rate KIB]  # a pane's cost in each tier
cargo xtask bench memory [--scenario NAME]... [--seconds S] [--warm-memory M]  # memory under pressure, see below
cargo xtask bench extensions [--panes N] [--without PART]  # typing next to slow and stalled extensions
cargo xtask bench remote [--samples N]                   # key to echo through a host (stand-in, no SSH)
cargo xtask bench remote --container [--netem SPEC]      # through OpenSSH to the ARM64 Linux container
cargo xtask bench compare [--terminal NAME]... [--frames off|on|always] [--seconds S]  # next to other terminals, Linux
cargo xtask bench report target/bench/headless-*.json     # result files as Markdown for this page
```

The harness builds `dreamerd` and `dreamer-bench` in release mode. It runs
them against a private tmux server and private daemon directories, never
the user's. It prints a table and writes the results to
`target/bench/<workload>-<time>.json`. Every result file, the desktop's
too, records:

- **The machine:** model, CPU, cores, memory, and operating system.
- **The revision:** commit, branch, whether tracked files had uncommitted
  changes, the build profile, `rustc -V`, `tmux -V` of the tmux that ran,
  and the pinned tmux and libghostty-vt from `deps/baseline.toml`.

`bench report` prints one or more result files as the Markdown this page
uses: the machine and revision, then a table with a column per file and a
row per measure, p50 and p95 on one row. Copy the rows from it rather than
typing numbers; the Target column is left for you to fill.

`--debug` builds and runs the debug binaries instead. It checks that the
harness works and is never a measurement; its results say so. Each pull
request's CI job runs `cargo xtask bench headless --panes 4 --debug`,
`cargo xtask bench remote --samples 20 --debug`, and `bench report` on
their results, on a shared x86-64 runner, and discards them. A broken
harness fails the job.

**Comparing terminals** (`bench compare`, Linux only for now). It builds
`dreamer`, `dreamerd`, and `dreamer-bench` in release mode and opens
windows on the machine's desktop: Dreamer, then each of Ghostty,
Alacritty, kitty, and foot that is installed (one that is not is listed as
skipped). Each draws the same isolated tmux session, on a tmux server of
its own with no tmux configuration. The other terminals run with their
built-in defaults and no user configuration; Dreamer runs with its
default settings and private daemon directories. Three phases, each on a
new tmux server:

- **Idle:** one pane at a shell prompt; memory after 6 s.
- **Cat:** the wall time of `cat` of a 64 MiB text file in one pane, as
  the shell times it, and memory after it.
- **Busy:** 16 tiled panes, 15 of them printing the file in a loop; the
  terminal's CPU (percent of one core) and peak memory over `--seconds`
  (20 by default), and the tmux server's CPU, reported on its own.

Memory is PSS, summed over the terminal's own processes. Dreamer's
numbers are split into the desktop (`dreamer`) and the daemon
(`dreamerd`). `--terminal` picks terminals (all by default). `--frames`
runs Dreamer with `DREAMER_RENDER_FRAMES` set to `off`, `on`, or `always`
(see `render.frames`), and the results record it, so the same command
measures frames off and on. On Hyprland, each window opens on an empty
workspace and the results record whether it had focus in the idle and
busy phases; elsewhere focus is recorded as unknown. Over SSH, it draws
on the logged-in desktop session, found through `XDG_RUNTIME_DIR`,
`WAYLAND_DISPLAY`, and `HYPRLAND_INSTANCE_SIGNATURE`. It prints a table
and writes `target/bench/compare-<time>.json`, which `bench report` reads.

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

   No GPU or paint is in this path. Then 200 more keystrokes with
   diagnostics on, in the daemon and in the harness's replica, reported
   as "diagnostics on" with each stage's p50 and p95 (see
   [Path timings](#path-timings-and-diagnostics)).
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
`--idle M`. `--sessions S` puts those shells in `S` sessions of their
own, so the daemon runs a control client in each; the busy and typed
panes stay in the first session. It records the number of tmux clients,
the daemon's and tmux's memory and idle CPU once the shells are
mirrored. Each time it also measures:

- tmux's own key to echo: `send-keys` into a `cat` pane, then
  `capture-pane` until the key is in tmux's grid;
- the CPU used by tmux and by the daemon;
- the same tmux measure with the daemon stopped.

The tmux measure includes starting two tmux clients per sample (about
1 ms on Linux and 11 ms on macOS), so compare it with its own unloaded
value.

**The streams workload** replays fixed byte corpora and measures what
each costs. The corpora are generated by `tools/bench/src/corpus.rs` from
a fixed seed, so every run replays the same bytes with nothing fetched;
a test pins each one's length and hash.

| Corpus | Bytes | What it is |
| --- | --- | --- |
| `text` | 2.0 MiB | Log lines, some longer than a row, no escapes |
| `sgr` | 2.0 MiB | Full-screen TUI redraws: a cursor move per row, 24-bit foreground and background on every run of 1 to 12 cells, some bold and underline, cursor-forward skips, erase to end of line |
| `altscreen` | 2.0 MiB | The alternate screen, each frame a synchronized update (mode 2026): full redraws, and scroll regions with new lines written at the bottom |
| `images` | 9.5 MiB | Kitty graphics: 40 raw RGBA images from 64 to 384 pixels a side, sent in 4 KiB chunks and placed, some placed again, with text between them. At most 24 are stored at once, under 14 MiB of the 64 MiB per-pane budget |

For each corpus in turn, with a fresh tmux server and daemon:

1. **The parser alone.** The corpus is fed to one engine configured as a
   daemon mirror, on one thread, with no tmux, transport, or frames
   (`engine parse alone`). For `images` the run fails unless images were
   stored and placed.
2. **The panes.** `N` panes (16 by default) replay the corpus in a loop
   with `cat`. A client replica follows the first of them, as a visible
   pane would. The others have no client: they are Warm and nobody
   paints them. The output states this split.
3. **The measures,** over `S` seconds (10 by default), or as long as the
   typing takes if that is longer:
   - **Daemon ingest:** bytes the busy panes' mirrors took in, per
     second. tmux delivers them, so this is bounded by tmux too.
   - **Daemon CPU** and **tmux CPU,** and the daemon's CPU time per MiB
     ingested.
   - **Replica frames built** per second for the visible pane, and how
     many synchronized updates were shown at the hold limit instead of at
     their end. Nothing paints, so frames are not limited to a display's
     rate.
   - **Key to echo in a quiet pane:** 100 keystrokes into a `cat` pane
     alongside, timed as in the headless workload.
   - **Mirror rebuilds:** times a mirror was rebuilt from a fresh capture
     because the output outran it. Zero means no pane lost its stream.
   - **Memory:** the daemon's RSS and heap growth under load, and the heap
     still held 3 s after the busy panes are closed.

**The remote workload** types into a pane on a host connected through
the daemon ([remote.md](remote.md)), the path the desktop takes to a
remote pane: the replica sends each key with `hosts.call`, the
coordinator relays it over the host's link to the host's daemon, and the
echo comes back through `hosts.subscribe`. It reports three things
separately, each with its p50, p95, and jitter (the mean difference
between consecutive samples, in the order taken):

1. **End to end.** 200 keystrokes (`--samples N`) into `cat` in a new
   window on the host, timed as in the headless workload, beside the same
   measure in a local pane of the same run.
2. **The link.** A `hosts.call` round trip of `daemon.version`, which
   the host's daemon answers at once, beside the same call to the local
   daemon. It also times the connection itself: SSH, the bridge, starting
   the host's daemon if needed, and the handshake.
3. **The host's processing.** The keystrokes again with the host's
   diagnostics on: each of the host daemon's stages (`input` through
   `transfer`, [below](#path-timings-and-diagnostics)) and the local
   replica's, then the sum of the host's stage p50s, and the end-to-end
   p50 less that sum (the link, the coordinator's relay, and the replica).

The host is reached one of two ways:

- **Stand-in** (the default). A shell on this machine takes the place of
  `ssh` and runs the remote command as sshd would, with a second tmux
  server and private daemon directories. There is no SSH and no network,
  so it shows what the bridge and the relay cost alone.
- **`--container`.** This machine is the desktop side; the host is the
  gate's ARM64 Linux container, reached through real OpenSSH. The
  container builds `dreamerd`, `dream`, and the pinned tmux from this
  checkout and runs `sshd` on a port published on this machine's
  127.0.0.1 only, with a generated host key and client key and private
  daemon directories. `ssh` here runs with its own config file,
  `known_hosts`, and key, and no agent: nothing of your SSH setup is read
  or changed. `--netem SPEC` (for example `--netem 'delay 20ms 5ms'`)
  adds `tc netem` to the container's network interface. It delays what
  the container sends, so each round trip gains the delay once. The
  container is removed afterwards. It needs an ARM64 host with a running
  container runtime, and it shares the gate's cargo caches, so run it
  while no gate is running.

The result file records the route, the link (OpenSSH version, the host's
`uname`, and the netem setting), and the host daemon's version and
platform.

**Kitty graphics and tmux.** tmux does not interpret Kitty graphics. It
passes the pane's bytes unchanged to the daemon, whose mirror stores and
places the images. tmux itself, as `screen` does, takes each APC string as
the pane's title (with `allow-set-title` on, the default), so in the
`images` corpus it sets a 4 KiB title for every chunk, and its CPU figure
includes that. This measures the daemon's side; images through tmux are
not otherwise claimed (see
[terminal-compatibility.md](terminal-compatibility.md)).

**The tiers workload** measures what one pane costs the daemon in each
tier, idle and busy. A Cold pane has no mirror, but tmux still sends its
output and the daemon reads and decodes it before dropping it (see
[mirrors.md](mirrors.md#tiers)). This workload measures that cost
against a Warm and a Hot pane. In order:

1. **Idle.** N idle shells (16 by default), measured Warm, then Cold,
   then Hot, then closed. Hot means each pane is followed by a client
   replica in the benchmark process, as a desktop view follows it, and
   marked Hot.
2. **Busy.** N panes writing output, measured Warm, then Cold, then Hot.
3. **No daemon.** The daemon is stopped and tmux is measured alone with
   the same busy panes.

Each step samples over `--seconds S` (10 by default):

- the daemon's CPU, tmux's CPU, and in the Hot steps the replicas' CPU,
  the frames each replica built, and (busy) the replicas' CPU per MiB/s
  of output. Each replica's frames are taken as they are woken, as a
  desktop view takes them;
- the daemon's heap;
- for Warm and Hot panes, the output their mirrors took in.

Per-pane figures are the growth over the step before the panes existed,
divided by N. The busy step ends with a busy Cold pane's daemon CPU as a
share of a busy Warm pane's.

The busy panes write `--rate KIB` KiB a second each (256 by default) in
ten bursts of `yes` output a second, so every tier sees the same output.
Check the "output into mirrors" rows against N times the rate: if tmux
fell behind, the tiers are not comparable. `--rate 0` runs `yes` flat out
instead, which measures the most tmux delivers rather than a fixed load.
`--stream escape` writes lines full of colour, attribute, cursor, and
erase sequences instead of plain text.

**The extensions workload** types into a quiet pane while extensions
misbehave next to N busy panes (63 by default). The extensions are the
harness's own stand-ins, never anything from your configuration:

- **Slow filter:** a filter on the busy panes records each match as an
  event, and a Luau script spends 45 ms of its 50 ms budget on each one.
- **Overrunning filter:** a second script runs past its budget on every
  match. The daemon stops it each time, and the harness reloads it when
  it is parked.
- **Stalled raw tap:** a client follows every busy pane at the `raw`
  level and never reads.
- **Stalled extension:** a supervised extension follows every pane at the
  `raw` level and the event log, then stops reading its input and
  answering heartbeats. The daemon kills it after a missed heartbeat, and
  it stalls again after each restart.
- **Crashing extension:** a supervised extension that exits with an error
  after its hello. It runs with a 100 ms first restart delay and is
  restarted whenever it is parked.
- **Monitor:** a supervised extension that does what a monitor pane does.
  It follows every pane at the `text` level and the event log, and reads
  the topology once a second.

It runs five phases on one daemon: quiet; quiet with the monitor open;
busy; busy with the monitor open; busy with every extension above. Each
phase measures:

- the daemon's CPU, resident size, and heap in use, and the busy panes'
  output ingested, over `--seconds` (10 by default);
- once a second, whether every busy pane's mirror sequence advanced (the
  daemon's reader never waits for an extension);
- key to echo in a fresh `cat` pane, 200 keystrokes, as in step 2;
- Ctrl-C in a quiet shell, timed until the shell's trap has redrawn the
  pane: the cancellation path.

The difference between phases is reported as the cost of opening the
monitor (idle and busy) and of the slow extensions, apart from the busy
panes' own cost. The daemon's figure includes the Luau scripts, which run
on the daemon's script threads, but not the supervised extensions'
processes; the monitor's own CPU and memory are reported beside it.
Selection happens in the desktop, not the daemon, so it is not part of
this workload. Three checks close the run:

- key to echo p95 with every extension running is within the one-pane
  budget plus 10 ms (40 ms);
- it is within the same run's quiet p95 plus 10 ms;
- every busy pane's mirror advanced in every second.

`--without PART` leaves one stand-in out (`monitor`, `filter`, `scripts`,
`raw-tap`, `stalled`, or `crasher`; `filter` takes the scripts with it),
to find which one a cost comes from. Without the monitor, its two phases
are skipped.

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

**The memory workload** runs four scenarios, each against a fresh daemon,
one after another. `--scenario` picks some of them; by default all four
run.

| Scenario | Load |
| --- | --- |
| `output` | `--panes N` panes (64 by default) run `yes` |
| `lagging` | The same load, and one client subscribes to every busy pane, then stops reading. At the end it reads again. Each subscription must have ended as `lagged`, and a resume from where it started must be answered with a checkpoint: the journal no longer holds that point |
| `paste` | A paste of `--paste-mib M` MiB (10 by default) of text lines, through `terminal.input`, into a pane whose program reads and discards it without echo |
| `extension` | A supervised extension answers `extension.hello`, subscribes to events, and exits with an error. The daemon restarts it and parks it after 8 failures; the workload then restarts it again |

`--backoff-ms B` sets the extension's first restart delay through
`DREAMER_TEST_EXTENSION_BACKOFF_MS`. It is 50 ms by default, so the
extension crashes many times a minute; the daemon's own delay is 500 ms.

`--warm-memory M` sets `tiers.warm_memory` to M MiB in the daemon's
configuration. In the `output` scenario, a replica then follows the first
busy pane and takes frames throughout, and the workload adds rows for the
ceiling: how many panes were Cold, whether the followed pane ever went
Cold (the bound fails if it did), and the daemon's own estimate of what
the mirrors hold after each demotion round.

Each scenario runs for `--seconds S` (300 by default; the paste at least
until it is delivered). Once a second it samples the daemon's RSS, its
heap in use, the host's journal, and, on macOS, its physical footprint
(`ri_phys_footprint` from `proc_pid_rusage`). The result file keeps these
curves under `curves`, with the columns `seconds`, `rss_mib`, `heap_mib`,
`journal_mib`, and `footprint_mib`; a value that cannot be read, such as
the footprint on Linux, is `null`. The table gives the values before the
load, at the peak, and at the end, and the RSS change over the second
half, which shows whether memory was still growing. On macOS it also
gives the footprint's peak, its value at the end, and its change over the
second half. RSS counts free pages the allocator keeps mapped and has not
handed back; the footprint, which is what the kernel charges the process
for, does not, so RSS can climb while the heap and the footprint stay
flat.

Each scenario also states a bound, built from the daemon's budgets, and
records under `bounds` whether the peak stayed under it. On macOS the
peak compared is the footprint's; elsewhere it is RSS's (`measured` in
the result file says which):

| Scenario | Bound |
| --- | --- |
| `output` | RSS before, plus the journal budget (the host limit, or the pane limit times the panes, whichever is smaller), plus 10 MiB per pane: up to 8 MiB of output held for a busy mirror and about 2 MiB of mirror for 2,000 lines of history |
| `lagging` | The `output` bound, plus the stalled connection's queue (256 messages of up to 256 KiB) and, per subscription, 4,096 queued records of the average retained size |
| `paste` | RSS before, plus 4 times the paste: its gathered parts, the encoded copy, and parts in flight |
| `extension` | RSS before, plus 32 MiB |

The journal limits come from the daemon (`DREAMER_JOURNAL_PANE_BYTES`,
`DREAMER_JOURNAL_HOST_BYTES`), so a run with other limits states its bound
from them. The limits count what records take in memory (see
[mirrors.md](mirrors.md)). The `output` and `lagging` scenarios report the
heap growth per byte the journal counts, which checks that estimate.

## Path timings and diagnostics

The daemon and the replica can time each stage of the terminal path and
count the traffic through it. This is off by default and costs nothing
then: each hook reads one flag and takes no clock reading. While on, a
stage costs two clock readings and a few atomic adds into a fixed
histogram, with no lock, no allocation, and nothing per byte. Only
durations and sizes are kept, never terminal content.

```sh
dream diagnostics on          # or start dreamerd with DREAMER_DIAGNOSTICS=1
dream status --perf [--json]  # timings, counters, and queue depths
dream diagnostics reset       # zero them; on or off stays as it was
dream diagnostics off
```

The API is `daemon.diagnostics` and `daemon.set_diagnostics`.

**Stages,** each timed per output chunk (or per input request) in path
order:

| Stage | Where | From | To |
| --- | --- | --- | --- |
| `input` | daemon | an input request arrives | its bytes are sent to tmux: the pane's input lane and the encoding against the canonical engine |
| `tmux` | daemon | input sent to a pane | the next read of that pane's output. tmux, the pty, and the program are all in it |
| `decode` | daemon | one read of tmux's control output returns | it is decoded into notifications |
| `route` | daemon | output decoded | the pane's VT owner starts on it: the router, any held backlog, and the owner's queue |
| `parse` | daemon | the canonical VT parse of one chunk starts | it ends |
| `publish` | daemon | a parsed record is handed to subscribers | it is in the journal |
| `transfer` | daemon | a record is handed to a subscriber | its batch reaches the client connection's writer |
| `replica_parse` | client | a received batch is applied to the replica | it is applied |
| `frame` | client | a frame is extracted from the replica | it is extracted |

Between `transfer` and `replica_parse`, the socket write, the kernel, and
the client's decoding are not timed. Glyph shaping, GPU submission, and
presentation are the desktop's and are not timed here. Percentiles come
from histogram buckets and are within 6 %.

**Counters** cover the time since diagnostics were turned on or reset:
- bytes read from tmux;
- bytes parsed, the records applied, and the parser's throughput while it
  works;
- bytes and batches sent to clients;
- subscriptions started from a checkpoint (and the checkpoints' bytes) or
  by replay;
- subscriptions ended as lagged: dropped subscriber events, each reported
  to its subscriber;
- mirrors rebuilt because a pane's output outran them;
- bytes of Cold panes' output dropped.

**Queue depths** are read when asked, on or off: tmux events waiting for
the router, output held for a full owner queue, attaches running and
waiting, each VT owner's queue, and the fullest subscriber queue.

**Views** are listed when asked, on or off: each view subscription, the
feed it is on (`stream` or `frames`), whether the daemon switches it by
rate or a client holds it, its switches, and its pane's output rate
([mirrors.md](mirrors.md#screen-frames)).
