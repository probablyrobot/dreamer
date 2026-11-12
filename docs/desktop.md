# Desktop app

`dreamer` is the GPUI desktop client.

```text
dreamer --attach %PANE [-L socket | --tmux-socket-path PATH] [--tmux PATH]
dreamer [--fixture FILE]
```

`--attach` shows a live tmux pane. The app starts `dreamerd` if it is not
running, passing it the tmux options. It then follows the pane as a client
replica (see [mirrors.md](mirrors.md)). Without `--attach`, the window
paints the built-in renderer demo or a VT fixture.

Input, selection, and window sizing come with later work. The pane is shown
at the size tmux gives it.

## Frame scheduling

- **Owner thread.** A background thread per pane owns the replica's engine
  and waits on the daemon connection with no timeout. It builds a frame only
  when records were applied or a checkpoint was restored.
- **Coalescing.** The newest frame replaces any unpainted one, and the
  window is woken through a channel that holds at most one signal. A burst
  of output becomes as few repaints as the window can take.
- **Idle costs nothing.** No timers and no polling: an idle pane produces no
  thread wakeups, no window wakeups, and no paints.
- **Synchronized output** (`CSI ? 2026 h`): a held frame is not shown until
  the application ends the hold, or until 1 second has passed, whichever
  comes first. A broken program cannot freeze the view.
- **Connection loss.** The owner reconnects with backoff (up to 2 s) and
  resumes, or resnapshots, as the replica rules decide. A closed pane, or
  one that cannot catch up, is reported, not retried forever.

## Renderer

Each row's text is shaped once and reused for as long as the row is shared
between frames and the colors, font, size, and cell width stay the same.
Paint statistics report lines shaped versus reused, decorated runs, and the
cursor shape painted.

## Checks

The macOS gate runs these after building the app:

| Step | Checks | Needs |
| --- | --- | --- |
| `matrix` | Every fixture in `crates/terminal-render/fixtures` paints the statistics listed in its `expected.txt`. These are derived from the fixture content: ASCII, combining marks, CJK, emoji, fallback glyphs, box drawing, five underline styles, strikethrough, inverse, dim, and the block, bar, underline, and hidden cursor | A GUI session |
| `cache` | `dreamer --smoke-repaint`: painting the same frame again reshapes nothing | A display that keeps refreshing |
| `live` | The live pane check below | A display that keeps refreshing |

A locked screen still paints a window's first frame, but macOS then stops
refreshing it. `cache` and `live` are recorded as not run while the screen
is locked. If a requested repaint does not happen, the app itself exits
with status 5 ("inconclusive"), so a count of zero paints is never taken
as evidence when nothing could have painted.

`dreamer --smoke --expect TEXT --attach %PANE …` waits for a frame
containing TEXT. It then requests one repaint as a control. If that paints,
it watches 1.5 s of idle pane and exits with status 3 if anything woke the
window or painted. The `live` step runs this against an isolated tmux
server and private daemon directories.
