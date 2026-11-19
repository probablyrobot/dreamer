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
| mlua | 0.12.2 | MIT | `dreamerd` (scripts) |
| Luau (vendored by mlua) | as pinned by mlua 0.12.2 | MIT | `dreamerd` (scripts) |
| regex | 1.13.1 | MIT OR Apache-2.0 | `dreamerd` (queries, filters) |
| ratatui | 0.30.2 | MIT | `dreamer-termapp` |
| crossterm | 0.29.0 | MIT | `dreamer-termapp` |
| rmcp | 3.10.0 | Apache-2.0 | `dream` (MCP) |
| ureq | 3.5.2 | MIT OR Apache-2.0 | `dreamer-termapp` (`fetch` in script apps) |
| rustls | 0.23.46 | Apache-2.0 OR ISC OR MIT | `dreamer-termapp` (`fetch`) |
| ring | 0.17.14 | Apache-2.0 AND ISC | `dreamer-termapp` (`fetch`) |
| webpki-roots | 1.0.12 | CDLA-Permissive-2.0 | `dreamer-termapp` (`fetch`: Mozilla's root certificates) |

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

## More bundled fonts

The macOS app bundle and the generic Linux package also ship these Nerd
Fonts 3.5.1 families (the Mono variant; patches by Ryan L McIntyre), each
with its license file next to the font files:

| Family | Upstream | License | License file |
| --- | --- | --- | --- |
| FiraCode Nerd Font Mono | Fira Code, Copyright (c) 2014, The Fira Code Project Authors | SIL OFL 1.1 | `OFL-FiraCode.txt` |
| Hack Nerd Font Mono | Hack, Copyright 2018 Source Foundry Authors; Bitstream Vera Sans Mono, Copyright 2003 Bitstream Inc. | MIT and the Bitstream Vera license | `LICENSE-Hack.md` |
| CaskaydiaCove Nerd Font Mono | Cascadia Code, Copyright (c) 2019 - Present, Microsoft Corporation | SIL OFL 1.1 | `OFL-CascadiaCode.txt` |
| SauceCodePro Nerd Font Mono | Source Code Pro, Copyright 2010-2020 Adobe, with Reserved Font Name 'Source' | SIL OFL 1.1 | `OFL-SourceCodePro.txt` |

Nerd Fonts' patched names (CaskaydiaCove, SauceCodePro) avoid the
upstream Reserved Font Names, as the OFL requires of modified versions.
