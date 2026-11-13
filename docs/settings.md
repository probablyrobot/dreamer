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
  line_height: 1.2                       # multiple of the size; 0.8 to 3.0
links:
  schemes: [http, https, mailto]         # default
clipboard:
  write: focused                         # focused (default), allow, or deny
fallback:
  launch: [kitty, -e, "{command}"]       # optional; no default
appearance: system                       # system (default), dark, or light
terminal:
  theme: dreamer-dark                    # default; `dream themes` lists all
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

`appearance` chooses the desktop's skin:
- `system` (the default) follows the OS appearance live;
- `dark` and `light` fix it.

Both skins are described in [design.md](design.md).

## Terminal

`terminal` sets every terminal's defaults: the color scheme, and the
cursor a program gets until it sets its own (and after it resets it with
`CSI 0 q`).

- `theme` is one of the built-in schemes. `dream themes` lists them, with a
  swatch and the configured one marked. Names are case-insensitive, and
  `_` or a space counts as `-`.
  - `dreamer-dark` (the default) and `dreamer-light` are designed with the
    two chrome skins.
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
