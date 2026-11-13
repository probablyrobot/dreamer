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
```

## Font

The default is JetBrains Mono, IntelliJ's font, patched with Nerd Fonts
icons. Dreamer uses the **Mono** variant, whose icons are one cell wide like
every other character.

The font comes with Dreamer: it's inside the macOS app, and the Arch package
depends on `ttf-jetbrains-mono-nerd`. Any other installed font can be named
in `font.family`, using the family name the system shows (on Linux,
`fc-list : family`).

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
