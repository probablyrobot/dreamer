# Settings

Dreamer reads `dreamer.yaml` from its configuration directory:
`~/.config/dreamer/dreamer.yaml` on macOS and Linux, or `$XDG_CONFIG_HOME`
or `DREAMER_CONFIG_DIR` when set. Every field is optional; without the file
everything is a default. Remote host profiles live beside it in
`hosts.yaml` ([remote.md](remote.md#profiles)).

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
fallback:
  launch: [kitty, -e, "{command}"]       # optional; no default
appearance: system                       # system (default), dark, or light
terminal:
  theme: dreamer-dark                    # optional; default follows appearance; `dream themes` lists all
  cursor:
    style: block                         # block (default), bar, or underline
    blink: false                         # default
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

## Errors

An unknown key or an out-of-range value is reported with the file, line,
and column, and Dreamer then runs with the defaults. A typo never quietly
changes behavior.

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
the pane. Programs can never read the clipboard. See `docs/effects.md`.

## Fallback terminal

`fallback.launch` is the terminal to open when a window or pane holds
something the workspace does not draw, such as floating panes or tmux copy
mode (see [desktop.md](desktop.md)). It is an argv with exactly one
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
one theme ([design.md](design.md)).

## Terminal

`terminal` sets every terminal's defaults: the color scheme, and the
cursor a program gets until it sets its own (and after it resets it with
`CSI 0 q`).

- `theme` names a color scheme file. `dream themes` lists them all, with
  a swatch, the configured one marked, and yours labelled. Names are
  case-insensitive, and `_` or a space counts as `-`.
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
    terminals. Anything it does not set is derived from its colors
    ([design.md](design.md)). The tokens are `background`, `surface`,
    `raised`, `divider`, `selected`, `text`, `muted`, `faint`, `accent`,
    `focus`, `danger`, `danger_surface`, `danger_text`, `warning`,
    `warning_surface`, and `warning_text`. An unknown token is an error.

    ```toml
    [dreamer.chrome]            # alacritty-theme TOML
    accent = '#ff8800'
    ```

    ```
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

A terminal's colors are its state, not a view's (spec 14.2). The daemon
applies the theme to every pane's canonical mirror, and every window,
including one on another machine, gets the same colors through its
checkpoint. A program can still change colors for itself (OSC 4, 10, 11,
12) until it resets them. The theme takes effect when the daemon starts.
To switch, edit the file and run `dream shutdown`: tmux and every program
keep running, and the next window or `dream` command starts the daemon
again with the new theme.

Scrollback length follows tmux's own `history-limit`, which the mirrors
copy.

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

