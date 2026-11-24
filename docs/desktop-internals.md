# Desktop internals

How the desktop schedules frames and draws text, and the checks that drive
real windows to prove it. You need this page if you work on the desktop
itself. To use the app, see [the desktop app](desktop.md).

## Frame scheduling

- **Owner thread.** A background thread per pane owns the replica's engine
  and waits on the daemon connection with no timeout. It builds a frame only
  when records were applied or a checkpoint was restored.
- **Pacing.** Frames follow what the window shows, not what arrives.
  While the window has not taken the last frame, no new one is built, and
  frames are at least 8.3 ms apart (120 a second). Records are still
  applied as they arrive, so the next frame shows all of them. The first
  frame after a pause, and the echo of typed input, are built at once.
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
| `clipboard` | The [clipboard check](#clipboard-check) | A window that is being drawn |
| `screens` | [Pixel evidence](#pixel-evidence) | A display that keeps refreshing, and Screen Recording permission |
| `preview` | The [preview check](#preview-check) | A display that keeps refreshing, and Screen Recording permission |
| `web` | The [web pane check](#web-pane-check) | `cargo xtask deps web`, and a display that keeps refreshing |

A locked screen still paints a window's first frame, but macOS then stops
refreshing it. `cache` and `live` are recorded as not run while the screen
is locked. If a requested repaint does not happen, the app itself exits
with status 5 ("inconclusive"), so a count of zero paints is never taken
as evidence when nothing could have painted.

`dreamer --smoke --expect TEXT --attach %PANE …` waits for a frame
containing TEXT. It then waits for the pane to settle (no new frame for
0.5 s), watches 4.5 s of idle pane, and exits with status 3 if any frame
woke the window. Repaints the pane did not ask for are not the pane's
work: macOS has GPUI repaint a window now and then for activity elsewhere
on the desktop. One is allowed. More, with no view wakeup, means the
desktop was busy (someone using the Mac), so the result is status 5, not
run. A repaint loop in the pane would show as view wakeups. A passing count is then checked by requesting one
repaint as a control: if the window does not paint (occluded, or the
display asleep), the result is status 5, not a pass. The `live` step runs
this against an isolated tmux server and private daemon directories.

### Clipboard check

`dreamer --smoke-clipboard` checks copy and paste in every kind of pane
there is, against an isolated tmux server, private daemon directories,
and a private configuration whose harness drawer runs a shell. A smoke
run never reads or replaces the clipboard you use: every desktop smoke
uses an in-memory clipboard and primary selection instead. Every gesture
is dispatched to the window as input, so the window's own input policy
decides what it does.

- **Terminal pane:** a drag, a double click, and a triple click select a
  part of a line, a word, and the line; Cmd-C (Ctrl-Shift-C) copies each.
  A finished selection becomes tmux's newest buffer at once and reaches
  the clipboard only on Copy, or on its own with `copy_on_select`. On
  Linux it is also the primary selection. Cmd-V (Ctrl-Shift-V) arrives as
  a bracketed paste in a program that asked for one; a middle click
  pastes tmux's newest buffer; a program's OSC 52 write reaches the
  clipboard.
- **tmux:** a buffer set by another tmux client reaches the clipboard
  once, and a Dreamer copy becomes tmux's newest buffer without coming
  back to the clipboard.
- **Harness drawer:** a drag and Copy copy from it, and Paste types into
  it, as in a terminal pane.
- **App pane:** a form app launched as a registered app and joined beside
  the terminal pane. A click focuses it; typed keys and an IME commit go
  into its first field; the palette shortcut opens the palette over it,
  and closing the palette leaves it focused; Tab moves to the second
  field; Paste and a middle click type into it; Copy takes the field to
  the clipboard and tmux's newest buffer; tmux's prefix and Left move
  focus to the terminal pane, and the prefix and Right back.
- **Web pane:** not checked: copy and paste in a page is Chromium's own,
  on the system clipboard, which a check never touches. The [web pane
  check](#web-pane-check) checks only that Cmd+A is Chromium's select
  all.

The macOS gate runs this as the `clipboard` step, and the Linux desktop
gate on each backend ([linux-desktop-gate.md](linux-desktop-gate.md)).
A window that is not being drawn (a locked screen, a display asleep)
exits with status 5 and the step is recorded as not run, never failed.

### Web pane check

When `cargo xtask deps web` has provisioned CEF, the macOS gate's `web`
step runs clippy on the desktop with its `web` feature, builds the
[bundle with web panes](desktop.md#a-build-with-web-panes), checks that the
framework and the five helper apps are in it and that a helper loads the
framework, and then runs `dreamer --smoke-web DIR` from the bundle
against an isolated tmux server with private daemon, desktop, and
configuration directories:

- first, adding Dreamer Web as a person would (`dreamer
  --smoke-web-install`, its own run of the desktop): the private data
  directory starts empty, and `DREAMER_WEB_URL` names a `file://`
  directory holding the zip and `.sha256` made from the bundle's
  component (copies already on the Mac are not used). The tab strip
  must show the first-start offer (`web.component: ask`); a web pane
  must show **Add Dreamer Web**; its button is pressed (the click's own
  handler, not a synthetic OS event); the install's progress must reach
  the end with the component ready; the offer must be gone; and the
  page must then load in that pane and draw frames, with no restart.
  Each passed check is a `smoke: web install:` line, and the gate wants
  all five. The rest of the check runs with the component this
  installed;
- the smoke opens a `data:` page beside the session's shell pane: one
  screen of `#2a6fdb`, then three screens of `#d9822b`, and a title that
  counts animation frames and reports the scroll position;
- it waits until Chromium has loaded the page and drawn 30 frames, and
  captures the window (`before.png`);
- it sends a wheel event to the page (or, if CEF does not deliver it,
  scrolls by script and says so), waits for the scroll to settle, and
  captures again (`after.png`);
- it resizes the pane through tmux (`resized.png`) and zooms it
  (`zoomed.png`), waiting each time for the page to follow and draw;
- it opens a new tab (`tab.png`), then goes back to the page's tab,
  checking that the page was kept rather than loaded again (`back.png`);
- it opens the palette, waits for the pane to show the page's snapshot
  (`palette.png`), closes it, and waits for the live page
  (`closed.png`);
- then input, sent through AppKit as a person's would be: a click in the
  page's text field focuses the web pane (it starts on the shell pane);
  keys, a Delete, and an input method composing "日" and committing "日本"
  type `hi日本` into the field, read back through DevTools; Cmd+J pressed
  in the page shows and focuses the harness drawer (a shell in this
  check's configuration), and pressed again hides it and gives the keys
  back to the page; Cmd+A selects the whole field (its selection read
  back through DevTools), through Chromium's own select all. Copy, cut,
  and paste are not pressed: they use the system clipboard, which the
  gate never touches, so copy and paste in a page is not checked by the
  gate. Each passed input check is a `smoke: web input:` line, and the
  gate wants all five;
- then an [app page](desktop.md#app-pages): the example web app
  (`examples/apps/panes-web`), granted `tmux.topology`, opens beside the
  shell pane. Its page lists the panes through the bridge, and a call
  through it lists the app's own pane (`app.png`); `terminal.list`,
  outside its grant, is refused with `-32005`, and so is `web.read`
  (its grant has no `web.inspect`); `apps.capture` of its pane, as
  `dream app preview --png` asks, writes its page's own image
  (`app-page.png`, mostly the page's `#1e1e1e`); a sandboxed frame
  inside it (another origin) has no `window.dreamer` and cannot reach
  the bridge, nor does the plain web pane have one; and a navigation to
  `https://example.invalid/`, outside its allow-list, is refused and the
  page stays on its origin; a download its page starts is refused (the
  app has no `web.download`) with a line over the page, and nothing is
  written. Each passed check is a `smoke: web app:` line, and the gate
  wants all eight;
- then the [browser](desktop.md#the-browser), in the plain web pane, against a
  test site the gate serves on `127.0.0.1` (nothing from the internet):
  `web.navigate` loads its login, which sets a session cookie; Cmd+L,
  an address typed without a scheme, and Enter load its first page; a
  link is followed; Cmd+[ and Cmd+] go back and forward, and so do mouse
  buttons 4 and 5; Cmd+R loads the page again (the site counts its
  loads); Cmd+F and a word typed get matches from Chromium's find, and
  Esc ends it; Cmd+= zooms the page and Cmd+0 back; a download asks
  first and writes nothing until Save; Save writes it, with the site's
  contents, into the downloads folder; Cancel at the prompt writes
  nothing; Cancel while a slow download is written leaves no partial
  file; and a link that wants a new window opens in the same pane. The
  downloads folder is the gate's own (`web.downloads.folder` in its
  private configuration, under `/tmp`): the smoke refuses a folder in the
  home directory, so it never writes to your Downloads. Then the gate
  starts the desktop again on the same profile (`dreamer
  --smoke-web-restart`), and a page of the site must still get the
  login's session cookie. Each passed check is a `smoke: web browser:`
  line, and the gate wants all fourteen. The swipe and the row's buttons
  (the smoke answers through the same call they make) are not clicked;
- then [an agent and a page](desktop.md#agents-and-pages), through the daemon on
  connections narrowed to an agent's grant at `own`: the agent opens the
  test site's form in a window of its own; without `web.inspect`,
  `web.read` is refused; with it, `web.read` reads the form and finds its
  field in the tree, `web.type` fills the field by its node and presses
  Enter, `web.click` follows a link by its selector, and
  `web.screenshot` writes the page (`web-inspect.png`). Each passed check
  is a `smoke: web inspect:` line, and the gate wants all five, the site
  must have got the form with the name typed, and the screenshot must be
  mostly the form's `#3fa34d`;
- then [page colors](settings.md#web-page-colors), in two more runs of
  the desktop (`dreamer --smoke-web-color DIR`), since Chromium takes
  `web.color` as it starts: the gate writes `web.color: srgb`, then
  `display`, into its private configuration before each. Each run opens
  a page whose top half is sRGB's red and bottom half Display P3's red
  (`color(display-p3 1 0 0)`), zooms its pane, and captures it live
  (`color-live.png`) and as its snapshot under the palette
  (`color-palette.png`), in `srgb/` and `display/`. In both modes the
  page's sRGB red must be sRGB's red as the window shows it (Chromium's
  output is color matched, as the window's is), and the snapshot must
  match the live page there, within 30 per channel. Display P3's red must
  then be clamped to sRGB's red with `srgb`, and stay beyond it with
  `display`; on a display that is not wide-gamut that check is a `smoke:
  web color not run:` line instead. Each passed check
  is a `smoke: web color:` line, and the gate wants six, counting a
  check not run;
- the gate checks each capture as the smoke's `checks.txt` lists: the
  page's color covers at least 90% of the pane's rectangle (of the
  zoomed pane, or of the part beside the palette) and at most 1% of the
  window left of it, none of it shows where the pane was while another
  tab is shown, and at most 5% of the palette's query line where it
  crosses the pane is the page's.

The captures and `checks.txt` are kept in `target/gate/web/`.
`cargo xtask web smoke` builds the bundle and runs the same check by
itself, keeping them in `target/web-smoke/`.
Without CEF provisioned, without a display that refreshes, when
Chromium stops drawing frames, or when the window is not the key window
(it cannot be typed into), the step is recorded as not run.

### Pixel evidence

The macOS gate's `screens` step captures the `single-pane` reference
scene in both of Dreamer's schemes (`cargo xtask screenshot` does the same
by hand; on Linux it works under Hyprland, on Wayland or XWayland, with
`grim`) and checks the pixels:
- the main color is the scheme's background, so the theme reached the
  terminals and the chrome alike;
- the scheme's green, which the scene prints, is drawn;
- the image is not flat.

`cargo xtask screenshot` refuses to save a capture where less than a fifth
of the pixels are the scheme's background: that is not Dreamer's window,
but a lock screen or another surface drawn over it. On macOS it also
refuses while the screen is locked, and on Linux while every display is
asleep.

It captures the `app-components` scene in both schemes too: the
reference app with every text, input, and status component
(`examples/apps/components/`), drawn natively as an app pane on the left
and, started from a shell, as its terminal interface on the right. The
main color is the scheme's background (or the app pane's surface), the
accent is drawn, and a badge is drawn both ways: natively on its danger
surface, and in the terminal in the scheme's red. A window that drew
neither (it stopped repainting) records the step as not run.

The captures stay in `target/gate/screens/`. The step is not run on a
locked screen, or when the terminal running the gate lacks Screen
Recording permission (System Settings > Privacy & Security > Screen &
System Audio Recording).

### Preview check

The macOS gate's `preview` step opens the desktop against an isolated
tmux server, private daemon directories, and a temporary configuration,
in each of Dreamer's two schemes, and in each:

- writes the `table` template app (`dream app new`) and opens its live
  preview (`dream app preview --open`);
- writes `dream app preview --png`, and checks the image: at least half
  of it is the scheme's background or the app pane's surface, and more
  than a few colors are drawn on it.

In the dark scheme it then saves a change to the app's `app.ts` (the
table's title) and times it: until the host's tree has the change
(`apps.tree`), then until the desktop has drawn it, taken as how much
longer a capture that waits for the new version takes than one of what
was already drawn. The two together must stay under one second, and the
image after the save must differ from the one before. The images stay
in `target/gate/screens/`. A locked screen, a window that stops
repainting, or a terminal without Screen Recording permission records
the step as not run.
