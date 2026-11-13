# Design

The desktop's look follows spec 14.4. Content comes first: terminal text
is the brightest, sharpest thing on screen. The chrome uses lower contrast
and a single accent color.

## Skins

There are two default skins, designed together. Neither is an inversion of
the other (a test checks this). `appearance` in `dreamer.yaml` chooses
between them: `system` (the default) follows the OS appearance live, and
`dark` or `light` fixes it.

The tokens live in `crates/app/src/theme.rs`. Every chrome color comes
from them, never from literals.

| Token | Dark | Light | Used for |
| --- | --- | --- | --- |
| background | `#14161A` | `#F6F6F4` | The window behind everything |
| surface | `#101216` | `#ECEBE7` | Tab strip, sidebar |
| raised | `#1F232A` | `#FFFFFF` | Command palette |
| divider | `#1F232A` | `#D9D7D1` | The gaps between panes |
| selected | `#2A2F38` | `#DEDCD5` | Selected tab, session, palette row |
| text | `#E6E6E6` | `#1F2328` | Primary chrome text |
| muted | `#B8BEC8` | `#454B56` | Inactive tabs, labels |
| faint | `#9AA0AA` | `#585F6B` | Hints, the zoom control |
| accent | `#8AB4F8` | `#2F6FDE` | Drag preview, links |
| focus | `#3D4A5C` | `#9DB4DC` | Focused pane hairline, palette border |
| danger / surface / text | `#F28B82` / `#5A2A2A` / `#F0D0D0` | `#B3261E` / `#FBE3E0` / `#6E1A14` | Kill commands, refused links |
| warning / surface / text | `#D7B46A` / `#3A2F1C` / `#F0DCB0` | `#7D5600` / `#FBEFD2` / `#4D3600` | Status, tmux-mode banners |

`both_skins_keep_readable_text_on_every_surface` requires WCAG contrast of
at least 4.5:1:
- for text, muted, and faint on every surface they appear on;
- for each danger and warning text on its surface.

It caught the earlier dark `faint` (`#8A909A`) at 4.18:1 on a selected
row.

**Terminal colors are not part of a skin.** A terminal's default colors
and palette are canonical terminal state. They change only through an
explicit, owner-authorized configuration (spec 14.2), which comes with
skins in M7. Until then, the light skin restyles the chrome and the
terminal keeps its own colors.

## Typography

- **Terminal:** JetBrains Mono, patched with Nerd Fonts (the variant whose
  icons stay one cell wide), 13 pt, line height 1.2. It is bundled with the
  macOS app and is a dependency of the Arch package (OFL-1.1; see
  `THIRD_PARTY_NOTICES.md` and [settings.md](settings.md)).
- **Chrome:** the system UI font, in a small fixed scale: 11, 12, and 13 pt.

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

Captures go outside the repository, in
`~/Devel/analysis/dreamer/design/`, named
`<scene>-<dark|light>-<macos|linux>.png`.

The spec's agentic-pane scene comes with M6. Later visual changes update
the captures.
