# Terminal color schemes

These files are copied unchanged from the alacritty-theme collection,
<https://github.com/alacritty/alacritty-theme>, at commit
`95615789c5eddee1c5407a520cd9959a86fa4716` (2026-10-29). The collection is
licensed under the Apache License 2.0 (`LICENSE-alacritty-theme`).

Each scheme was chosen because its original project is under a permissive
license (MIT, or Apache-2.0 for Nord); the copyright lines are in
`THIRD_PARTY_NOTICES.md`. `dreamer-dark.toml` and `dreamer-light.toml`
are Dreamer's own, in the same format.

`cargo xtask schemes` checks every file and lists them in
`src/schemes_data.rs`, which compiles the files themselves into the
program (`include_str!`): they always ship with it, and are read at run
time by the same parser as users' theme files.

| File | Scheme | Original project | License |
| --- | --- | --- | --- |
| catppuccin_mocha, catppuccin_macchiato, catppuccin_frappe, catppuccin_latte | Catppuccin | catppuccin/catppuccin, the Catppuccin organization | MIT |
| dracula | Dracula | dracula/dracula-theme, Zeno Rocha | MIT |
| nord | Nord | nordtheme/nord, Sven Greb (Arctic Ice Studio) | Apache-2.0 |
| gruvbox_dark, gruvbox_light | Gruvbox | morhetz/gruvbox, Pavel Pertsev | MIT/X11 |
| tokyo_night, tokyo_night_storm, tokyo_night_light | Tokyo Night | enkia/tokyo-night-vscode-theme, enkia | MIT |
| one_dark, one_light | One Dark and One Light | atom/atom, GitHub Inc. | MIT |
| rose_pine, rose_pine_moon, rose_pine_dawn | Rosé Pine | rose-pine/rose-pine-theme, the Rosé Pine authors | MIT |
| everforest_dark, everforest_light | Everforest | sainnhe/everforest, Sainnhe Park | MIT |
| kanagawa_wave | Kanagawa | rebelot/kanagawa.nvim, Tommaso Laurenzi | MIT |
| solarized_dark, solarized_light | Solarized | altercation/solarized, Ethan Schoonover | MIT |
| github_dark, github_light | GitHub | primer/github-vscode-theme, GitHub Inc. | MIT |
| ayu_dark, ayu_light | Ayu | ayu-theme/ayu-colors, Konstantin Pschera | MIT |
| tomorrow_night, tomorrow | Tomorrow | chriskempson/tomorrow-theme, Chris Kempson | MIT |
| selenized_dark, selenized_light | Selenized | jan-warchol/selenized, Jan Warchoł | MIT |
| flexoki, flexoki-light | Flexoki | kepano/flexoki, Steph Ango | MIT |
