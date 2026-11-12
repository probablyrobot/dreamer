# dreamer

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
cargo test --workspace --locked
```

## License

Apache-2.0. See `LICENSE`, `NOTICE`, and `THIRD_PARTY_NOTICES.md`.
