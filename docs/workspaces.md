# Workspaces

A workspace file describes a tmux session declaratively: its windows, how
each window is split, and what runs in every pane. `dream workspace up`
creates the session; any tmux client can attach to it.

## Where files live

Dreamer's configuration directory is `~/.config/dreamer` on both macOS and
Linux (`$XDG_CONFIG_HOME/dreamer` when that is set, or `$DREAMER_CONFIG_DIR`
to override). Workspaces are YAML files in its `workspaces/` folder:

```
~/.config/dreamer/
  workspaces/
    dreamer-dev.yaml
    ops.yaml
```

```sh
dream config-dir                  # print the directory
dream workspace list              # every workspace and whether it is valid
dream workspace check dreamer-dev # validate and show what would be created
dream workspace up dreamer-dev    # create the session
dream workspace up ./other.yaml   # a file anywhere
dream workspace up ops -L work    # on the tmux server named "work"
```

`up` refuses to touch an existing session with the same name.

## Example

```yaml
version: 1
name: dreamer-dev            # session name; default: the file name
root: ~/Devel/dreamer        # absolute, ~/..., or relative to this file
env:
  RUST_LOG: info             # for every pane

windows:
  - name: code
    focus: true              # selected after creation
    layout:
      split: horizontal      # children left to right
      panes:
        - size: 60%
          title: editor
          run: nvim .        # typed into a shell, which stays afterwards
          focus: true
        - split: vertical    # children top to bottom
          panes:
            - title: check
              exec: [cargo, watch, -x, check]   # the pane's process, no shell
            - title: shell                      # just a shell

  - name: tests
    root: crates/tmux-backend                   # relative to the workspace root
    panes:                                      # a flat list…
      - title: unit
        shell: cargo test --lib                 # sh -c; the pane closes when it ends…
        keep: true                              # …unless kept
      - title: integration
        run: cargo test --test bootstrap
    arrange: even-horizontal                    # …arranged by a tmux preset
```

A complete example is in `crates/workspace-config/examples/dreamer-dev.yaml`.

## Reference

### Workspace

| Key | Meaning |
| --- | --- |
| `version` | Required. Must be `1`. |
| `name` | Session name. Defaults to the file name. No `:` or `.`. |
| `root` | Default working directory for every pane. |
| `env` | Environment variables for every pane. |
| `windows` | One or more windows, in order. |

### Window

| Key | Meaning |
| --- | --- |
| `name` | Window name, taken literally (`#` is not a format). |
| `root` | Working directory for this window, relative to the workspace root. |
| `env` | Added to the workspace environment. |
| `focus` | Select this window after creation (at most one). |
| `layout` | A split tree (see below). |
| `panes` | A flat list of panes, stacked and then arranged by `arrange`. |
| `arrange` | `tiled`, `even-horizontal`, `even-vertical`, `main-horizontal`, or `main-vertical`. Only with `panes`. |

Use either `layout` or `panes`. With neither, the window has one shell.

### Split

| Key | Meaning |
| --- | --- |
| `split` | `horizontal` (side by side) or `vertical` (stacked). |
| `panes` | Two or more children: panes or further splits. |
| `size` | This split's share of its parent. |

### Pane

| Key | Meaning |
| --- | --- |
| `exec` | Run this program as the pane's process: a list of arguments, no shell involved. |
| `shell` | Run this command with `/bin/sh -c`. |
| `run` | Start your shell and type this command into it. The shell stays when the command ends. One line. |
| `cwd` | Working directory, relative to the window root. |
| `env` | Added to the window environment. |
| `title` | Pane title, shown in pane headers. |
| `focus` | Select this pane in its window (at most one per window). |
| `keep` | Keep the pane open after its process exits, to read its output. |
| `size` | The pane's share of its split. |

Use at most one of `exec`, `shell`, and `run`; with none, the pane is your
default shell.

### Sizes

`size: 30%` is a share of the parent split; `size: 12` is a number of cells
(columns in a horizontal split, rows in a vertical one). Children without a
size share what is left equally. Percentages may not add up to 100% or more
when some children have no size, and must add up to 100% when all do.

## Errors

Every problem is reported with the file, and for syntax and unknown keys the
line and column:

```
~/.config/dreamer/workspaces/broken.yaml:3:5: windows[0]: unknown field `lyout`,
expected one of `name`, `root`, `env`, `focus`, `layout`, `panes`, `arrange`
```

Unknown keys are always errors, so a typo never silently does nothing.

## Safety

Names, paths, commands, and environment values are passed to tmux as encoded
arguments, never assembled into a command string. Quotes, `;`, `$`, `~`,
`#`, and leading `-` in any value reach tmux literally.
