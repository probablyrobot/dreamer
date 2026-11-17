# Design

The desktop's look follows spec 14.4. Content comes first: terminal text
is the brightest, sharpest thing on screen. The chrome uses lower contrast
and a single accent color.

## One theme for the whole window

The window is one surface: the title bar, tabs, sidebar, palette,
banners, the gaps between panes, and the terminals share one color
scheme and one type family.

- **The scheme decides.** The terminals' scheme (`Settings::scheme`:
  `terminal.theme`, or `dreamer-dark` / `dreamer-light` for
  `appearance`) is set by the daemon on every pane, and the desktop draws
  its chrome from the same scheme. The chrome follows the terminals, never
  the other way: a terminal's colors are its canonical state (spec 14.2).
- **Dreamer's two schemes** come with hand-tuned chrome (below). Any other
  scheme gets chrome derived from its own colors: its background for the
  window, surfaces a few steps toward its foreground, its blue as the
  accent (a scheme's cursor color is often too pale for an accent line),
  and its red and yellow for danger and warning.
- **A scheme's own chrome.** A theme file may set any chrome token itself
  ([settings.md](settings.md)). What it sets wins over what is derived;
  what it leaves out is derived as above.
- **Always a visible cursor.** A scheme whose cursor barely shows on its
  background (under 3:1) has its cursor drawn in the text color. This is a
  view choice; the terminal's colors are unchanged.
- **Readable everywhere.** `every_scheme_gets_chrome_drawn_from_its_own_colors`
  checks every built-in scheme: chrome text on every surface is as
  readable as the scheme's own text (up to 4.5:1). Derived surfaces step
  back toward the background when a scheme's contrast is low (Solarized
  Dark needed this).

The tokens live in `crates/app/src/theme.rs`. Every chrome color comes
from them, never from literals. Dreamer's own pair:

| Token | Dark | Light | Used for |
| --- | --- | --- | --- |
| background | `#14161A` | `#F6F6F4` | The window, the title bar, the gaps between panes: the terminals' own background |
| surface | `#101216` | `#ECEBE7` | Sidebar |
| raised | `#1F232A` | `#FFFFFF` | Command palette, the zoom chip |
| divider | `#1F232A` | `#D9D7D1` | Hairlines: between panes, under the tab strip |
| selected | `#2A2F38` | `#DEDCD5` | Selected session, palette row |
| text | `#E6E6E6` | `#1F2328` | Primary chrome text, the active tab |
| muted | `#B8BEC8` | `#454B56` | Inactive tabs, labels |
| faint | `#9AA0AA` | `#585F6B` | Hints |
| accent | `#8AB4F8` | `#2F6FDE` | The active tab's underline, drag preview, links |
| focus | `#3D4A5C` | `#9DB4DC` | Focused pane hairline, palette border |
| danger / surface / text | `#F28B82` / `#5A2A2A` / `#F0D0D0` | `#B3261E` / `#FBE3E0` / `#6E1A14` | Kill commands, refused links |
| warning / surface / text | `#D7B46A` / `#3A2F1C` / `#F0DCB0` | `#7D5600` / `#FBEFD2` / `#4D3600` | Status, tmux-mode banners |

`both_skins_keep_readable_text_on_every_surface` requires WCAG contrast of
at least 4.5:1 for text, muted, and faint on every surface they appear
on, and for each danger and warning text on its surface.

## Layout

- **Title bar.** On macOS the tab strip is the title bar: the window's
  buttons sit at its left, it drags the window, and a double click zooms
  it as the system setting says. On Linux it sits under the compositor's
  title bar.
- **Tabs.** Plain `index:name` labels. The active one is in full text
  color with an accent line under it.
- **Panes.** Inset 6 px from the window's edges, in the background color.
  tmux's one-cell gaps between panes are drawn as hairlines down their
  middle. The focused pane's hairline frame shows only when there is more
  than one pane.
- **Command palette.** A query line (a hint while empty), then at most
  12 commands. The list scrolls with the selection and says how many more
  there are. It floats on a soft shadow.

## Typography

- **Terminal:** JetBrains Mono, patched with Nerd Fonts (the variant whose
  icons stay one cell wide), 13 pt, line height 1.2. It is bundled with the
  macOS app and is a dependency of the Arch package (OFL-1.1; see
  `THIRD_PARTY_NOTICES.md` and [settings.md](settings.md)).
- **Chrome:** the same family as the terminal (the configured `font`), in
  a small fixed scale: 11, 12, and 13 pt. One family keeps the chrome and
  the terminals visibly of a piece.

## Review gate

Before M3 closes, the product owner approves captures of the reference
scenes on macOS and Linux, in both skins. `cargo xtask scenes [NAME]`
opens them one at a time, each against an isolated tmux with a temporary
config, and names the file to save. Close each window to move on.

| Scene | Shows |
| --- | --- |
| `single-pane` | One pane with colored text, wide characters, and a prompt |
| `dense-splits` | Six panes in one window |
| `sidebar-open` | The sessions sidebar with three sessions |
| `command-palette` | The palette over two panes |
| `error-state` | A pane in tmux copy mode, marked with the attach fallback |
| `app-components` | The reference app with every text, input, and status component, drawn natively and as a terminal interface |

Captures go outside the repository, in
`~/Devel/analysis/dreamer/design/`, named
`<scene>-<dark|light>-<macos|linux>.png`.

The spec's agentic-pane scene comes with M6. Later visual changes update
the captures.
