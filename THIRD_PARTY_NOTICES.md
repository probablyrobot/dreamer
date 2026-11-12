# Third-party notices

Dreamer binaries include or link the components below. Each entry gives the
component, the pinned version, and its license. The full license texts ship
with every distributable archive.

This list is maintained alongside `docs/dependency-baseline.md`; a dependency
that is redistributed must appear in both. Rust crate licenses are collected
from `Cargo.lock` at packaging time.

| Component | Version | License | Used by |
| --- | --- | --- | --- |
| libghostty-vt | 1.0.1 | MIT | Every executable with a terminal engine |
| highway (in libghostty-vt) | 1.2.0 | Apache-2.0 OR BSD-3-Clause | same |
| simdutf (in libghostty-vt) | 9.0.0 | Apache-2.0 OR MIT | same |
| translate_c (in libghostty-vt) | 2.0.0 | MIT | same |
| uucode (in libghostty-vt) | 0.2.0 | MIT AND Unicode-3.0 | same |
| wuffs (in libghostty-vt) | 0.4.0-alpha.10 | Apache-2.0 OR MIT | same |
| GPUI (zed-industries/zed @ 6ceb6fe) | 0.2.2 | Apache-2.0 | `dreamer` desktop client |
| image | 0.25.10 | MIT OR Apache-2.0 | `dreamer` desktop client |
| serde | 1.0.229 | MIT OR Apache-2.0 | `dream`, workspace files |
| serde_norway | 0.9.42 | MIT OR Apache-2.0 | `dream`, workspace files |

## JetBrains Mono Nerd Font

The macOS app bundle and the generic Linux package ship JetBrains Mono
patched with Nerd Fonts icons (Nerd Fonts 3.5.1, `JetBrainsMonoNerdFontMono`,
Regular, Bold, Italic, Bold Italic). JetBrains Mono is Copyright 2020 The
JetBrains Mono Project Authors; Nerd Fonts patches are by Ryan L McIntyre.
Both are licensed under the SIL Open Font License 1.1; the license text
(`OFL.txt`) is shipped next to the font files.
