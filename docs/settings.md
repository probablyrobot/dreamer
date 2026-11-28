# Settings

Dreamer reads `dreamer.yaml` from its configuration directory:
`~/.config/dreamer/dreamer.yaml` on macOS and Linux, or `$XDG_CONFIG_HOME`
or `DREAMER_CONFIG_DIR` when set. Every field is optional; without the file
everything is a default. Remote host profiles live beside it in
`hosts.yaml` ([remote.md](remote.md#profiles)).

## Live, and checked

The file is the one place to configure Dreamer, and changes apply live.
- **Watching.** The daemon and every open window look at `dreamer.yaml`
  and `themes/` once a second.
- **Applying.** A valid change applies at once, with no restart:
  - the terminal theme and cursor reach every running pane, in every
    window, including on other machines;
  - the font, chrome, appearance, window decorations, links, clipboard,
    fallback, and Dreamer Web settings apply in every window;
  - the tier budgets apply to the daemon's panes;
  - the memory provider applies to the next memory call.
  - the downloads folder applies to the next download in a web pane.
- **Refusing.** A file that does not read changes nothing. Dreamer keeps
  the settings in force, and the window's tab strip says
  `dreamer.yaml not applied: …` with the reason until the file reads
  again. The daemon logs it and publishes a `config.changed` event with
  state `refused`.
- **Not live.** Provider settings apply to the next agent run, agent
  harness settings to the next session, `render.frames` to the panes a
  window starts following after the change, and `web.color` once Dreamer
  restarts.

Two commands help with edits:
- `dream config schema` lists every key: its kind and range, default,
  meaning, an example, and whether it applies live. `--json` gives the
  same as data.
- `dream config check` validates every file in the directory
  (`dreamer.yaml` and the theme it names, every theme file, and
  `hosts.yaml`) with the same parsers Dreamer uses. It exits 1 on any
  problem, naming the file, the key or line, and what is wrong.

Agents can make these changes for you. `dream skills install` gives Claude
Code, Codex, and opencode the `dreamer-config` skill. It describes this loop (look
up, edit, check, and it applies) and what is safe to change.

```yaml
font:
  family: JetBrainsMono Nerd Font Mono   # default
  size: 13                               # points; 6 to 72
  line_height: 1.2                       # multiple of the font's own line height; 0.8 to 3.0
  letter_spacing: 0                      # pixels added to each cell's width; -3 to 10
links:
  schemes: [http, https, mailto]         # default
clipboard:
  write: focused                         # focused (default), allow, or deny
  tmux: sync                             # sync (default), from_tmux, to_tmux, or off
  copy_on_select: false                  # default
  middle_click: paste                    # paste (default) or off
fallback:
  launch: [kitty, -e, "{command}"]       # optional; no default
appearance: system                       # system (default), dark, or light
terminal:
  theme: dreamer-dark                    # optional; default follows appearance; `dream themes` lists all
  cursor:
    style: block                         # block (default), bar, or underline
    blink: false                         # default
tiers:
  cold_after: 30m                        # default; a wait (90s, 30m, 2h) or never
  warm_limit: 128                        # default; a number of panes or none
  warm_memory: 1GiB                      # default; a size (512MiB, 2GiB) or none
drawer:
  side: bottom                           # bottom (default), right, or left
  size: 35                               # default; percent of the window, 10 to 90
memory:
  provider: sqlite                       # default; or files, or kosmonaut
window:
  decorations: full                      # full (default) or none
render:
  frames: on                             # off, on (default), or always
  frames_above: 512KiB                   # to frames above this a second
  frames_below: 128KiB                   # back to the stream below this
  frames_dwell: 1s                       # least time on a feed
  frames_hold: 2s                        # on the stream after selecting
env:
  autosave: true                         # default; save the environment when Dreamer changes it
web:
  component: ask                         # ask (default), auto, or never
  url: https://example.com/dreamer-web/  # optional; default: the release
  downloads:
    folder: ~/Downloads                  # optional; default ~/Downloads (Linux: the XDG download folder)
  color: srgb                            # srgb (default) or display; applies once Dreamer restarts
```

## Font

The default is JetBrains Mono, IntelliJ's font, patched with Nerd Fonts
icons. Dreamer uses the **Mono** variant, whose icons are one cell wide like
every other character.

These families come with Dreamer, all in the Nerd Fonts Mono variant:

| `font.family` | Font | License |
| --- | --- | --- |
| `JetBrainsMono Nerd Font Mono` (default) | JetBrains Mono | SIL OFL 1.1 |
| `FiraCode Nerd Font Mono` | Fira Code (regular and bold; it has no italics) | SIL OFL 1.1 |
| `Hack Nerd Font Mono` | Hack | MIT and the Bitstream Vera license |
| `CaskaydiaCove Nerd Font Mono` | Cascadia Code | SIL OFL 1.1 |
| `SauceCodePro Nerd Font Mono` | Source Code Pro | SIL OFL 1.1 |

They are inside the macOS app and the generic Linux tarball, with their
license files. The Arch package depends on `ttf-jetbrains-mono-nerd` and
lists the others' distribution packages as optional. Any other installed
font can be named in `font.family`, using the family name the system shows
(on Linux, `fc-list : family`).

If the configured family is not available, Dreamer says so on standard
error and falls back to Menlo on macOS or DejaVu Sans Mono on Linux. It
never substitutes silently.

`font.size` is in points, from 6 to 72. `font.line_height` is a multiple
of the font's own line height, from 0.8 to 3.0, and `font.letter_spacing`
adds pixels to each cell's width, from -3 to 10. Bigger and smaller text
in the desktop ([desktop.md](desktop.md#menus-and-shortcuts)) change the
size in the open windows only and leave the file alone.

## Errors

An unknown key or an out-of-range value is an error that names the file
and the key, with the line and column for a syntax error or an unknown
key. A typo never quietly changes behavior:

- **At startup,** the desktop prints the error on standard error and runs
  with the defaults until the file reads.
- **While running,** the settings in force stay, and the tab strip shows
  the reason ([above](#live-and-checked)).

`dream config check` reports the same errors without starting anything.

## Links

`links.schemes` lists the URI schemes a Cmd-click (macOS) or Ctrl-click
(Linux) on a terminal hyperlink may open. Names are case-insensitive and
written without the colon. Any other scheme, such as `file`, is refused
with a notice. An empty list opens nothing.

## Clipboard

`clipboard.write` decides when a program's clipboard write (OSC 52)
reaches the system clipboard:
- `focused` (the default): only while the window is active;
- `allow`: whenever the daemon addresses the write to this window;
- `deny`: never.

The daemon addresses each write to one window only: the one focused on
the pane. Programs can never read the clipboard. See
[effects.md](effects.md#clipboard-writes-go-to-one-window).

**tmux's paste buffers and the clipboard** (`clipboard.tmux`):
- **A copy made in tmux** reaches the system clipboard. That covers copy
  mode's yank in any tmux client, or `tmux set-buffer`. It obeys `write`:
  with `focused`, only while a Dreamer window is active.
- **A copy made in Dreamer** becomes tmux's newest buffer. That covers
  Cmd-C, a finished mouse selection, and Copy in an app panel. tmux's own
  paste, and plain tmux clients, see it too.
- **Turning it down:** `from_tmux` or `to_tmux` keeps one direction, and
  `off` keeps neither.
- **What the daemon reports:** it says only that a buffer changed and its
  name. Its text is read by the window that puts it on the clipboard, and
  is never in the event log.

## Fallback terminal

`fallback.launch` is the terminal to open when a window or pane holds
something the workspace does not draw, such as floating panes or tmux copy
mode (see [desktop.md](desktop.md#what-the-workspace-does-not-draw)). It is an argv with exactly one
`{command}` element, which is replaced by the words of the `tmux attach`
command. Examples:
- `[kitty, -e, "{command}"]`
- `[alacritty, -e, "{command}"]`
- `[open, -na, Ghostty, --args, -e, "{command}"]`

Without it, the workspace only offers the copyable command.

## Appearance

`appearance` chooses dark or light for the whole window, terminals and
chrome alike:
- `system` (the default) follows the OS appearance at the time the daemon
  and the desktop start (on macOS the system setting; on Linux the GNOME
  `color-scheme`, else dark);
- `dark` and `light` fix it.

Without a `terminal.theme`, it picks `dreamer-dark` or `dreamer-light`.
The chrome is always drawn from the terminals' scheme, so the window is
one theme.

## Terminal

`terminal` sets every terminal's defaults: the color scheme, and the
cursor a program gets until it sets its own (and after it resets it with
`CSI 0 q`).

- `theme` names a color scheme file. `dream themes` lists them all, with
  a swatch, the configured one marked, and yours labelled. Names are
  case-insensitive, and `_` or a space counts as `-`.
  - **From the palette:** "Theme…" in the desktop's command palette lists
    every theme with a swatch, starting at the one in force. Type to
    narrow it. The highlighted theme is previewed: the window at once, and
    the terminals a moment later, because it is written to `dreamer.yaml`.
    Enter keeps it. Escape puts `dreamer.yaml` back exactly as it was.
    Only the `theme:` line changes; comments and every other setting stay.
  - **Built in:** the scheme files that ship with Dreamer. They are
    compiled into the program, so they are always there.
  - **Yours:** files in `<config dir>/themes/` (see `dream config-dir`),
    found by their name without the extension. Three formats are read,
    so the large published collections work unchanged:
    - alacritty-theme TOML (`.toml`), the format of the built-in files,
      and of the alacritty-theme collection;
    - Ghostty theme files (no extension, or `.conf`), as the
      iTerm2-Color-Schemes collection publishes them;
    - kitty theme files (`.conf`), as the kitty-themes collection
      publishes them.

    Drop one in and name it. A file of yours with a built-in's name
    replaces the built-in. A file that does not read (missing colors, a
    bad value) is an error naming the file and line.
  - Only the colors a terminal's defaults need are read: background,
    foreground, the 16 ANSI colors, and optionally the cursor and
    selection colors. Everything else in a file is ignored.
  - **Chrome colors.** A theme file may also style the window around the
    terminals. Anything it does not set is derived from its colors. The tokens are `background`, `surface`,
    `raised`, `divider`, `selected`, `text`, `muted`, `faint`, `accent`,
    `focus`, `danger`, `danger_surface`, `danger_text`, `warning`,
    `warning_surface`, and `warning_text`. An unknown token is an error.

    ```toml
    [dreamer.chrome]            # alacritty-theme TOML
    accent = '#ff8800'
    ```

    ```text
    dreamer-chrome-accent = #ff8800   # Ghostty
    dreamer_chrome_accent #ff8800     # kitty
    ```
  - Without one, the theme follows `appearance`: `dreamer-dark` or
    `dreamer-light`, Dreamer's own, designed with their chrome.
  - With one, the chrome is drawn from that scheme's own colors.
  - The others come from Catppuccin (Mocha, Macchiato, Frappé, Latte),
    Dracula, Nord, Gruvbox, Tokyo Night (night, storm, light), One Dark
    and One Light, Rosé Pine (main, moon, dawn), Everforest, Kanagawa,
    Solarized, GitHub, Ayu, Tomorrow, Selenized, and Flexoki (see
    `THIRD_PARTY_NOTICES.md`).
  - An unknown name is an error that lists the valid ones.
- `cursor.style` is `block`, `bar`, or `underline`; `cursor.blink` is
  `true` or `false`.

A terminal's colors are its state, not a view's. The daemon
applies the theme to every pane's canonical mirror, and every window,
including one on another machine, gets the same colors through its
checkpoint. A program can still change colors for itself (OSC 4, 10, 11,
12) until it resets them.

Scrollback length follows tmux's own `history-limit`, which the mirrors
copy.

## Pane tiers

The daemon keeps a terminal mirror (Warm) for every pane, so a window,
tap, or filter can follow it at once. A Warm pane holds about 1 KiB for
each line of history it keeps: about 2 MiB at tmux's default 2,000 lines.
A pane that nothing follows can go Cold instead. A Cold pane holds no
mirror, keeps running in tmux, and is attached again in tens of
milliseconds when something asks for it ([mirrors.md](mirrors.md#tiers)).

```yaml
tiers:
  cold_after: 30m      # quiet this long, and nothing follows it: Cold
  warm_limit: 128      # at most this many Warm panes that nothing follows
  warm_memory: 1GiB    # at most this much held by all mirrors together
```

- **Followed** means a window shows the pane, or a tap or filter reads
  it. A followed pane is never demoted.
- **Over the limit,** the quietest panes go Cold first.
- **Over the memory ceiling,** the quietest panes that nothing follows go
  Cold until the mirrors are back under it. The figure is an estimate:
  about 1 KiB per 120-column line of screen and history, plus the pane's
  journal. Followed panes count but stay Warm, so they alone can hold
  more. Sizes take `KiB`, `MiB`, or `GiB`.
- **Each daemon applies its own host's file,** so the budgets are per
  host.
- **Turning it off:** `cold_after: never`, `warm_limit: none`, and
  `warm_memory: none` keep every pane Warm.

## Agent harnesses

`agents:` chooses the harness a new agent session runs (pi, Claude Code,
Codex, opencode, or an ACP agent) and how each is started, and pins the pi release
Dreamer installs. See [agents.md](agents.md#settings).

```yaml
agents:
  default: pi                  # pi (default), claude, codex, opencode, or a name below
  pi:
    version: latest            # latest (default) or a release such as 1.5.2
    model: openai-codex/gpt-5.5  # optional; default: the profile's provider and its default model
    agent_dir: ~/.pi/agent     # optional; default: Dreamer's pi profile
    delegates: [claude, codex] # default; the CLIs pi may hand tasks to, [] for none
  gemini:
    protocol: acp
    command: [gemini, --experimental-acp]
```

## Harness drawer

`drawer:` places the [harness drawer](desktop.md#the-harness-drawer) in
the window it is shown in. A change applies the next time it is shown.

```yaml
drawer:
  side: bottom     # bottom (default), right, or left; always the whole window's width or height
  size: 35         # percent of the window's height (bottom) or width (sides), 10 to 90; `35%` also reads
```

## Window decorations

`window.decorations` chooses whether desktop windows have the system's
title bar and its close, zoom, and minimize buttons:
- `full` (the default) keeps them. On macOS the buttons sit at the tab
  strip's left; on Linux the desktop environment draws its title bar.
- `none` removes them. The tab strip still moves the window, a
  double-click on it zooms the window, and the window still resizes from
  its edges. On Linux the window asks for client-side decorations and
  draws none.

```yaml
window:
  decorations: none
```

## Screen frames

`render.frames` says whether a desktop window may show a pane from the
daemon's screen frames instead of following every byte the pane writes
with a terminal engine of its own
([desktop-internals.md](desktop-internals.md#screen-frames)):
- `off`: every pane follows the byte stream.
- `on` (the default): the window tells the daemon it accepts frames.
  Panes start on the byte stream, and the daemon moves a pane to frames
  while it writes fast, and back when it calms.
- `always`: every pane the window starts following is followed on frames.
  This is for checks.

On frames, selecting, scrolling back, searching, and copy mode bring the
pane back to the byte stream first, and keep it there while you use them.

It applies to panes a window starts following after the change.
`DREAMER_RENDER_FRAMES=off|on|always`, when set, comes first.

The daemon's side applies live, to every pane:
- `frames_above` (default `512KiB`): a pane goes to frames once it writes
  more than this a second, measured over `frames_dwell`.
- `frames_below` (default `128KiB`): it comes back to the byte stream once
  it writes less than this. It must be less than `frames_above`, so a pane
  that writes at about one rate does not flip between the two.
- `frames_dwell` (default `1s`): the least time on either feed before the
  daemon switches a pane, and the window its rate is measured over.
- `frames_hold` (default `2s`): how long a pane stays on the byte stream
  after you stop selecting, scrolling back, or searching in it.

Rates are a size a second (`KiB`, `MiB`, `GiB`); times are `ms`, `s`, or
`m`. The defaults come from output rates measured on an M3 Max: typing at
a prompt, `ping`, a build log, and `top` write under 5 KiB/s, a
full-screen program redrawing 30 times a second about 115 KiB/s, and `cat`
of a large file or `seq` 8 to 14 MiB/s. Listing a large directory tree
comes in bursts of up to about 1.4 MiB/s that last a second or less. A
busy full-screen program that redraws in colour without pause (the
benchmark's escape load) writes about 768 KiB/s.

```yaml
render:
  frames_above: 4MiB
```

## The environment

Once `dream env init` has run, the configuration directory is versioned
with git as your environment (see [the CLI](cli.md#the-environment)).
With `env.autosave` on (the default), Dreamer saves it when it changes
it itself: an app installed or rolled back with `dream app`, a theme
kept from the palette. Only the files that change are saved, as
"(automatic)" in `dream env log`; your own edits wait for
`dream env save`. When git cannot save (a merge under way, say), the
change still happens and Dreamer says the environment was not saved.

```yaml
env:
  autosave: false
```

## Dreamer Web

`web:` says how Dreamer adds Dreamer Web, the Chromium component web
panes need, when it is missing ([desktop.md](desktop.md#dreamer-web)).

- `component: ask` (the default) offers it once at the first start, in
  the tab strip, and in each web pane that needs it. Nothing downloads
  until you click.
- `component: auto` adds it without asking: at the start, and whenever a
  web pane (yours, an agent's, or an app's) needs it. For managed setups.
- `component: never` offers nothing; web panes say it is turned off.
  "Add Dreamer Web" in the palette and `dream web install` still add it.
- `url` is where to download it from instead of the release: the zip's
  address, or a directory's that holds `dreamer-web-<version>-<target>.zip`
  and its `.sha256`, over `https`, `http`, or `file`. `DREAMER_WEB_URL`,
  when set, comes first.

A copy already on the machine is used in place before anything is
downloaded, whatever the setting. A change applies to the next web pane
and the next install; the first-start offer is decided when the desktop
starts.

```yaml
web:
  component: auto
  url: https://downloads.example.com/dreamer-web/
```

## Downloads

`web.downloads.folder` is where a download in a web pane is saved, once
you choose Save in the pane ([desktop.md](desktop.md#the-browser)). It
is an absolute path, or one from `~/` (your home directory). Without it:
- macOS: `~/Downloads`;
- Linux: `XDG_DOWNLOAD_DIR` from `user-dirs.dirs` (in `$XDG_CONFIG_HOME`,
  or `~/.config`), else `~/Downloads`.

The folder is made if it is not there. A change applies to the next
download.

```yaml
web:
  downloads:
    folder: ~/Downloads/web
```

## Web page colors

`web.color` is the color space web panes draw pages in
([desktop.md](desktop.md#web-panes)):
- `srgb` (the default) draws them in sRGB, as the terminals and the
  chrome are. Wide-gamut colors (Display P3 images, video, CSS
  `color(display-p3 ...)`) are clamped to sRGB, so on a P3 display they
  look less saturated than in Safari or Chrome.
- `display` draws them in the display's own color space, so those colors
  keep their saturation on a wide-gamut display. The snapshot shown while
  the palette or a menu covers a page is converted to sRGB before it is
  drawn, so they are clamped for as long as it shows.

In both, colors within sRGB look as they do in the terminals: Chromium
matches its output to the display as the rest of the window does.

Chromium reads it when it starts, with the first web pane, so a change
applies once Dreamer restarts.

```yaml
web:
  color: display
```

## Memory

`memory:` chooses what keeps durable memory: `provider: sqlite` (the
default) keeps every memory in one SQLite database for the user on this
machine, `<data dir>/memory.sqlite`, where links are edges that recall
follows. The first time it reaches a scope, it copies in once what
`files` kept there, so memories from earlier versions carry over; the
files are left as they are ([memory.md](memory.md#moving-from-files)).
`provider: files` keeps one markdown file per memory and a `MEMORY.md`
index, under `<config dir>/memory/` for the user and `.dreamer/memory/`
in a project. `provider: kosmonaut` keeps the same on a Kosmonaut server (a build with
the `kosmonaut` feature). A change applies to the next memory call. See
[memory.md](memory.md).

```yaml
memory:
  provider: sqlite
  sqlite:
    path: ~/.local/share/dreamer/memory.sqlite   # default: <data dir>/memory.sqlite
  # provider: kosmonaut
  # kosmonaut:
  #   url: grpc://memory.example:7687    # grpc://host:port, or unix:/path
  #   tls: true                          # default; false only to localhost
  #   token_env: KOSMONAUT_TOKEN         # the variable holding the token, in dreamerd's environment
  embedder:                              # optional, for sqlite and kosmonaut; without it, no vectors
    kind: static                         # static, local, or openai
    path: ~/models/words.vec             # static: word vectors, word2vec text format
    # kind: local
    # command: [my-embedder, --json]     # local: JSON texts in, JSON vectors out
    # dimension: 384
    # timeout: 30s                       # default
    # kind: openai
    # model: text-embedding-3-small      # openai: OpenAI's embeddings API
    # key_env: OPENAI_API_KEY            # the variable holding the key, in dreamerd's environment
    # url: https://api.openai.com/v1     # default; https, or http to localhost only
    # dimension: 512                     # optional; the model's own length by default
    # timeout: 30s                       # default; each request
```

The embedder used to be under `memory.kosmonaut.embedder`. That key is
still read for now, and `dream config check` prints a note saying to
move it; setting both is an error.

`sqlite.path` and `embedder.path` are absolute or start with `~/`. The `openai`
key is read only from the environment variable `key_env` names, never
from a file or keychain, and so is the Kosmonaut server's token
(`token_env`); a `token:` key is refused. See
[memory.md](memory.md#embedders) for what each embedder does, and
[memory.md](memory.md#kosmonaut) for the Kosmonaut server.

## Agent providers

The Claude Code and Codex CLIs need no settings. The API adapters are
used only when configured here, and only when chosen: by name
(`--provider claude-api`), or as `default`. They are never a fallback.
See [providers.md](providers.md).

```yaml
providers:
  default: claude              # claude, codex, claude-api, or openai-api; default: the first ready CLI
  claude_api:                  # the Claude Messages API (metered)
    model: <model id>          # required: there is no built-in model
    key: env                   # env (ANTHROPIC_API_KEY), env:NAME, or keychain
    max_tokens: 4096           # 1 to 200000
  openai_api:                  # the OpenAI Chat Completions API (metered)
    model: <model id>
    key: keychain              # env (OPENAI_API_KEY), env:NAME, or keychain
    base_url: https://api.openai.com/v1   # https, or http on this machine
```

A key never goes in this file: `key` only says where to read it. With
`keychain`, the key is read from the OS keychain under service `dreamer`
and account `claude-api` or `openai-api`. Store it there yourself, for
example `security add-generic-password -s dreamer -a claude-api -w` on
macOS, or `secret-tool store --label dreamer service dreamer account
claude-api` on Linux.

