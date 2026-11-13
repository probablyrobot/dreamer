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

## Terminal color schemes

Every executable with a terminal engine contains the color values of 31
schemes from the alacritty-theme collection
(<https://github.com/alacritty/alacritty-theme>, commit `95615789`),
licensed under the Apache License 2.0, the same license as Dreamer. Each
scheme was designed by the original project below. The copyright lines are
those in each project's license file, checked on 2026-11-13.

| Scheme | Original project | License | Copyright |
| --- | --- | --- | --- |
| Catppuccin (Mocha, Macchiato, Frappé, Latte) | catppuccin/catppuccin | MIT | Copyright (c) 2021 Catppuccin |
| Dracula | dracula/dracula-theme | MIT | Copyright (c) 2023 Dracula Theme |
| Nord | nordtheme/nord | Apache-2.0 | Sven Greb (Arctic Ice Studio) |
| Gruvbox (dark, light) | morhetz/gruvbox | MIT/X11 (stated in its README) | Pavel Pertsev (morhetz) |
| Tokyo Night (night, storm, light) | enkia/tokyo-night-vscode-theme | MIT | Copyright (c) 2018-present Enkia |
| One Dark, One Light | atom/atom | MIT | Copyright (c) 2011-2022 GitHub Inc. |
| Rosé Pine (main, moon, dawn) | rose-pine/rose-pine-theme | MIT | Copyright (c) 2023 Rosé Pine |
| Everforest (dark, light) | sainnhe/everforest | MIT | Copyright (c) 2019 sainnhe |
| Kanagawa (wave) | rebelot/kanagawa.nvim | MIT | Copyright (c) 2021 Tommaso Laurenzi |
| Solarized (dark, light) | altercation/solarized | MIT | Copyright (c) 2011 Ethan Schoonover |
| GitHub (dark, light) | primer/github-vscode-theme | MIT | Copyright (c) 2020 Primer |
| Ayu (dark, light) | ayu-theme/ayu-colors | MIT | Copyright (c) Konstantin Pschera |
| Tomorrow, Tomorrow Night | chriskempson/tomorrow-theme | MIT | Copyright (C) 2011 Chris Kempson |
| Selenized (dark, light) | jan-warchol/selenized | MIT | Copyright (c) 2021 Jan Warchoł |
| Flexoki (dark, light) | kepano/flexoki | MIT | Copyright (c) 2023 Steph Ango |

The MIT license of those projects:

> Permission is hereby granted, free of charge, to any person obtaining a
> copy of this software and associated documentation files (the
> "Software"), to deal in the Software without restriction, including
> without limitation the rights to use, copy, modify, merge, publish,
> distribute, sublicense, and/or sell copies of the Software, and to permit
> persons to whom the Software is furnished to do so, subject to the
> following conditions:
>
> The above copyright notice and this permission notice shall be included
> in all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
> OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
> MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
> NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
> DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
> OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
> USE OR OTHER DEALINGS IN THE SOFTWARE.

## JetBrains Mono Nerd Font

The macOS app bundle and the generic Linux package ship JetBrains Mono
patched with Nerd Fonts icons (Nerd Fonts 3.5.1, `JetBrainsMonoNerdFontMono`,
Regular, Bold, Italic, Bold Italic). JetBrains Mono is Copyright 2020 The
JetBrains Mono Project Authors; Nerd Fonts patches are by Ryan L McIntyre.
Both are licensed under the SIL Open Font License 1.1; the license text
(`OFL.txt`) is shipped next to the font files.
