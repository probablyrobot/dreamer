# dreamer-sdk

Write [Dreamer](https://github.com/probablyrobot/dreamer) extensions and process apps in Rust.

Dreamer is a native terminal for macOS and Linux in which every tmux pane is
its own surface, and panes can host scripted apps and agents. This crate is
the Rust side of its two program interfaces:

- **Extensions** are programs `dreamerd` starts and supervises. They speak
  JSON-RPC 2.0 on standard input and output. The SDK answers the daemon's
  handshake and heartbeats, makes typed calls under the extension's grant,
  and delivers typed notifications.
- **Process apps** are pane apps whose program draws its own view tree. The
  SDK runs their event loop and builds the tree.

```toml
[dependencies]
dreamer-sdk = "0.11"
tokio = { version = "1", features = ["macros", "rt"] }
```

`dreamer_sdk::api`, `dreamer_sdk::app_protocol`, and `dreamer_sdk::view_tree`
re-export the types, so this is the only Dreamer crate you add.

See [extensions](https://github.com/probablyrobot/dreamer/blob/main/docs/extensions.md) and the
[app protocol](https://github.com/probablyrobot/dreamer/blob/main/docs/app-protocol.md), and the examples in
`examples/`.

Licensed under Apache-2.0.
