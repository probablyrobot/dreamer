# 0002. Terminal renderer on GPUI: proof of approach

- Status: accepted (visual review pending)
- Date: 2026-11-12
- Issue: #8

## Context

Dreamer owns its terminal renderer (spec section 9). The M0 question is
whether GPUI's public text and paint APIs can draw a terminal grid
cell-accurately, including wide and emoji cells, a cursor, and an image,
without moving the grid when shaped glyph advances differ from cell widths.

## Decision

`crates/terminal-render` paints a `FrameSnapshot` with a custom GPUI
`Element`:

- **Cell metrics:** width is the advance of `M` in the terminal font, and
  height is ascent plus descent times the line-height factor. Both are
  snapped to device pixels, with the baseline centered in the cell. One set
  of metrics per pane, owned by its sizing owner (spec 11.8).
- **Text:** consecutive narrow cells with the same style are shaped as one
  run through `TextSystem::shape_line` with `force_width = cell width`. That
  fixes every glyph's advance to the grid regardless of the font. Each wide
  cell is shaped alone with `force_width = 2 × cell width` at its own column.
  The engine's width decisions (`CellWidth`) are authoritative; spacer cells
  draw nothing.
- **Order:** default background, merged cell backgrounds, text with its
  underline and strikethrough, cursor (block, hollow block, bar, underline),
  then images clipped to the grid.
- **Styles:** bold and italic map to font weight and style. Inverse swaps
  colors, faint halves alpha, and invisible draws nothing. Curly underlines
  are drawn wavy; other underline shapes are drawn straight until the
  renderer gets its own decoration pass.
- **Images:** pixels become a GPUI `RenderImage` (BGRA) painted with
  `Window::paint_image` into a rectangle measured in cells.

## Evidence

macOS 27.2, Apple M3 Max, 2× display, `dreamer --smoke` with the built-in
demo (styles, 256 and 24-bit color, inverse, faint, underline kinds, CJK,
combining marks, skin-tone and ZWJ emoji, box drawing, a mixed-width ruler,
a bar cursor, and a 96×48 gradient image at 12×4 cells):

```
smoke: frame presented (900x420 logical px, scale 2)
smoke: cell 8.5x19.5 px, baseline 14.5 px
smoke: painted PaintStats { background_quads: 2, text_runs: 25, wide_cells: 23, images: 1 }
```

The counts match the demo frame: two backgrounds (`bg256`, inverse), 23 wide
cells (CJK, Korean, Japanese, and emoji), and one image.

Not yet evidenced:

- **Pixels.** Automated screenshots need macOS Screen Recording permission
  for the terminal running the gate, which is not active yet. Until it is,
  the rendered window needs a visual check (`cargo run -p dreamer-app`).
  Emoji fallback in particular is unverified.
- **Linux.** CI builds this code. A real Wayland and X11 window comes from
  the second machine (M0-16).
- **Kitty placements.** The image here is placed by the caller. Reading
  placements and pixels from libghostty-vt's Kitty graphics API is M2-06.

## Consequences

- Grid accuracy does not depend on font metrics beyond the cell width.
  Proportional fallback glyphs (emoji, CJK from another font) are centered
  in their cells rather than defining them.
- Shaping happens every paint, with no glyph or run cache yet. That is
  acceptable for the proof; M2-03 adds shaping caches keyed by row content
  and font, plus damage-driven repaint.
- Ligatures are off, since forced advances defeat them. Enabling them later
  needs per-run shaping without `force_width` and an explicit choice.
- Underline shapes beyond single and curly, overline, and hyperlink hit
  regions need a renderer-owned decoration pass (M2-03, M2-05).
