---
name: dreamer-config
description: Configure the Dreamer terminal for the user: themes and color schemes, fonts, appearance, links, clipboard, agent providers and harnesses, pane tier budgets (how many panes keep a live mirror), remote host profiles, and workspace layouts. Use when the user asks to change how Dreamer looks or behaves, to add a theme, or to set up a host or workspace. Edits the configuration files, checks them, and the running Dreamer applies them live.
---

# Configuring Dreamer

Dreamer is configured by plain files in one directory. Change them for the
user instead of asking them to edit by hand. Run `dream config-dir` to find
the directory (usually `~/.config/dreamer`).

| File | What it holds |
| --- | --- |
| `dreamer.yaml` | Appearance, terminal theme and cursor, font, links, clipboard, fallback terminal, agent providers, agent harnesses (`agents:`), pane tiers |
| `themes/` | The user's own color schemes, one file each |
| `hosts.yaml` | Remote host profiles (reached over SSH only) |
| `workspaces/*.yaml` | Workspace layouts: sessions, windows, splits, and what runs in each pane |

## The loop

Every change follows the same four steps.

1. **Look up the key.** Run `dream config schema` for readable output, or
   `dream config schema --json` for structured output. Each key comes with
   its kind and range, default, meaning, an example, and whether it applies
   live. Never guess a key: unknown keys are errors.
2. **Edit the file.** Change only what was asked, and keep the rest of the
   file and its comments as they are. If `dreamer.yaml` does not exist,
   create it with just the keys you need.
3. **Check.** Run `dream config check`. It validates every file with the
   same parsers Dreamer uses. Exit status 0 means all are valid. Otherwise
   it names the file, the key or line, and what is wrong: fix that and
   check again.
4. **It applies live.** The daemon and every open window watch these
   files. A valid change takes effect within about a second, with no
   restart: theme, terminal colors, cursor, font, appearance, links,
   clipboard, and fallback. A file that does not check changes nothing,
   and Dreamer keeps the settings already in force. The window's tab
   strip says "dreamer.yaml not applied: …" until the file is fixed.
   Provider settings apply to the next agent run.

## Themes

- **Built-in themes.** Run `dream themes` to list every theme, with a
  swatch, the current one marked, and the user's own labelled `(yours)`.
  Set one with:

  ```yaml
  terminal:
    theme: catppuccin-mocha
  ```

- **Light or dark without naming a theme.** Set `appearance: light`,
  `dark`, or `system`. Without a `terminal.theme`, it picks Dreamer's own
  `dreamer-light` or `dreamer-dark`.
- **One theme for everything.** The whole window (title bar, tabs,
  sidebar, palette) is drawn from the terminal theme. There is no separate
  "UI theme" to set.
- **Adding a theme the user found.** Save the file into `themes/` under
  the name to use, then set `terminal.theme` to that name. Three formats
  are read, so files from the big collections work as downloaded:
  - alacritty-theme TOML (`.toml`);
  - Ghostty theme files (no extension, or `.conf`), for example from
    iTerm2-Color-Schemes;
  - kitty theme files (`.conf`).

  A file of the user's with a built-in's name replaces the built-in.
- **Writing a theme.** A theme needs a background, a foreground, and the
  16 ANSI colors; the cursor and selection colors are optional. Make the
  foreground readable on the background: at least 4.5:1 contrast.
- **Tuning the window around the terminals.** A theme file may also set
  chrome colors, and anything it leaves out is derived from its colors:

  ```toml
  [dreamer.chrome]
  accent = '#ff8800'
  ```

  The tokens are `background`, `surface`, `raised`, `divider`, `selected`,
  `text`, `muted`, `faint`, `accent`, `focus`, `danger`,
  `danger_surface`, `danger_text`, `warning`, `warning_surface`, and
  `warning_text`. In Ghostty format write `dreamer-chrome-accent = #ff8800`,
  in kitty format `dreamer_chrome_accent #ff8800`.

## Fonts

```yaml
font:
  family: JetBrainsMono Nerd Font Mono   # the default
  size: 14                               # 6 to 72 points
  line_height: 1.2                       # 0.8 to 3.0
  letter_spacing: 0                      # -3 to 10 pixels
```

- The family must be a monospace font installed on the system.
- Prefer a Nerd Font "Mono" variant, so that prompt and status-line icons
  stay one cell wide.
- The chrome uses the same family.

## Hosts and workspaces

- **Hosts.** Remote hosts are profiles in `hosts.yaml`, reached through
  the system's own `ssh` with the user's keys and SSH configuration. Never
  edit `~/.ssh/config` or the user's tmux configuration. If a host needs
  SSH settings, tell the user what to add there. `dream hosts` connects,
  lists, and verifies hosts, and `dream config check` validates the file.
- **Workspaces.** Workspace layouts are YAML files in `workspaces/`.
  `dream workspace check NAME` validates one, and `dream workspace up NAME`
  opens it.

## Rules

- **Secrets never go in these files.** For an API provider, `key` is
  `env`, `env:NAME`, or `keychain`; tell the user how to put the key there.
- **Changes always pass the check.** Never leave a file that fails
  `dream config check`. If you cannot make it valid, restore what was
  there.
- **The user's other tools are off limits.** Do not touch the user's own
  tmux server, its configuration, or SSH configuration.
- **Report the result.** Say what changed, and confirm the check passed.
