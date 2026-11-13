# dreamer

<img src="packaging/icons/linux/128.png" width="96" height="96" alt="Dreamer icon: a halftone moon">

A native terminal for macOS (Apple Silicon) and Linux. Each tmux pane is drawn
as its own surface, a headless server keeps working when the window closes, and
panes can host small apps and agents that watch, query, and filter other
terminals.

Status: early implementation. Nothing here is usable yet.

## Building

Requirements are checked by:

```sh
cargo xtask doctor
```

Then:

```sh
cargo xtask hooks install   # once per clone
cargo test --workspace --locked
```

The pre-commit hook runs `cargo xtask gate --staged` on exactly what is staged;
the pre-push hook runs `cargo xtask gate --full`. Reports are written to
`target/gate/<platform>/<tree>-<mode>.json`.

## License

Apache-2.0. See `LICENSE`, `NOTICE`, and `THIRD_PARTY_NOTICES.md`.
