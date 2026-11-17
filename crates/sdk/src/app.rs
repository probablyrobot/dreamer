//! Write a process app in Rust: a pane app whose program draws its own
//! view tree (see `docs/app-protocol.md`). The package's manifest names
//! the program and has the `process` capability:
//!
//! ```toml
//! schema_version = 2
//! id = "local.counter"
//! version = "0.1.0"
//! title = "Counter"
//! capabilities = ["process", "tmux.topology"]
//!
//! [process]
//! command = ["./counter"]
//! ```
//!
//! The program holds its state in a type that implements [`App`]:
//! [`App::view`] draws the state as a tree (built with [`crate::ui`]), and
//! [`App::update`] changes it for each [`Msg`]: input, the answer to an
//! API call, an event. [`run`] does the rest: the handshake with the host,
//! sending the tree after every message (the whole tree first, then only
//! what changed), and matching answers to calls.
//!
//! ```no_run
//! use dreamer_sdk::app::{self, App, Context, Event, Msg};
//! use dreamer_sdk::ui;
//!
//! struct Counter(i64);
//!
//! impl App for Counter {
//!     fn view(&self) -> ui::Screen {
//!         ui::screen("Counter", ui::text("count", self.0.to_string())).status("+ add  q quit")
//!     }
//!     fn update(&mut self, msg: Msg, cx: &mut Context) {
//!         if let Msg::Event(Event::Key { key }) = msg {
//!             match key.as_str() {
//!                 "+" => self.0 += 1,
//!                 "q" => cx.quit(),
//!                 _ => {}
//!             }
//!         }
//!     }
//! }
//!
//! fn main() -> Result<(), app::Error> {
//!     app::run_blocking(Counter(0))
//! }
//! ```
//!
//! Standard output belongs to the protocol: print logs to standard
//! error, whose last lines the host keeps and shows if the app fails.

use std::collections::HashMap;

pub use app_protocol::{AppInfo, CallError, Event, PROTOCOL};
use app_protocol::{AppMessage, HostMessage, MAX_LINE};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::ui::Screen;

/// What the app is told.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// The first message: the app as its manifest declares it, the
    /// methods it may call, and whether the host is connected to the
    /// daemon.
    Start {
        app: AppInfo,
        methods: Vec<String>,
        live: bool,
    },
    /// Input: a key, text, a click, wheel steps, or a component's event.
    Event(Event),
    /// The answer to the call made with [`Context::call`] under `tag`.
    Reply {
        tag: String,
        method: String,
        result: Result<Value, CallError>,
    },
    /// An event from a subscription the app made (`events.subscribe`).
    Notify { topic: String, payload: Value },
    /// The host's connection to the daemon came up or went away.
    Link { live: bool, reason: Option<String> },
}

impl Msg {
    /// A reply's result as the API's typed response.
    pub fn response(method: &str, result: &Value) -> Result<api::Response, String> {
        api::Response::from_json(method, result.clone()).map_err(|e| e.to_string())
    }
}

/// An app: its state, drawn by `view` and changed by `update`.
pub trait App {
    /// The state as a tree. Called after every message; only what changed
    /// from the last tree is sent.
    fn view(&self) -> Screen;
    /// Change the state for `msg`. Calls and quitting go through `cx`.
    fn update(&mut self, msg: Msg, cx: &mut Context);
}

/// What `update` asks for besides changing the state.
#[derive(Debug, Default)]
pub struct Context {
    next: u64,
    calls: Vec<(u64, String, Value)>,
    tags: HashMap<u64, (String, String)>,
    quit: bool,
}

impl Context {
    /// Call API `method` with `params` (its JSON). The answer comes back
    /// later as [`Msg::Reply`] with this `tag`; `update` is not held up.
    pub fn call(&mut self, tag: impl Into<String>, method: impl Into<String>, params: Value) {
        self.next += 1;
        let method = method.into();
        self.tags.insert(self.next, (tag.into(), method.clone()));
        self.calls.push((self.next, method, params));
    }

    /// Make a typed call; the answer comes back as with [`Context::call`].
    pub fn request(&mut self, tag: impl Into<String>, request: &api::Request) {
        self.call(tag, request.method(), request.to_json());
    }

    /// Stop after this message: the program exits successfully, and the
    /// pane closes.
    pub fn quit(&mut self) {
        self.quit = true;
    }
}

#[derive(Debug)]
pub enum Error {
    /// Reading or writing the protocol failed.
    Io(std::io::Error),
    /// The host said something this SDK does not understand, or nothing.
    Protocol(String),
    /// The view's tree is not valid (a key missing or used twice among
    /// siblings).
    Tree(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(e) => write!(f, "app protocol: {e}"),
            Error::Tree(e) => write!(f, "view tree: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Run `app` on this process's standard input and output, on a runtime
/// of its own, until the host stops it or it quits.
pub fn run_blocking<A: App>(app: A) -> Result<(), Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run(app))
}

/// Run `app` on this process's standard input and output.
pub async fn run<A: App>(app: A) -> Result<(), Error> {
    run_on(app, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Read one line, at most [`MAX_LINE`] bytes; `None` at the end.
async fn read_line<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> Result<Option<String>, Error> {
    let mut buf = Vec::new();
    let n = (&mut *r)
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_LINE {
        return Err(Error::Protocol("a line too long".into()));
    }
    Ok(Some(String::from_utf8_lossy(&buf).trim_end().to_owned()))
}

async fn send<W: AsyncWrite + Unpin>(w: &mut W, m: &AppMessage) -> Result<(), Error> {
    w.write_all(app_protocol::line(m).as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

/// Run `app` on `input` and `output`, as the host's end would see them.
pub async fn run_on<A, R, W>(mut app: A, input: R, mut output: W) -> Result<(), Error>
where
    A: App,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut input = BufReader::new(input);
    let hello = loop {
        match read_line(&mut input).await? {
            None => return Ok(()),
            Some(l) if l.is_empty() => {}
            Some(l) => break l,
        }
    };
    let (info, methods, live) = match serde_json::from_str::<HostMessage>(&hello) {
        Ok(HostMessage::Hello {
            protocol,
            app,
            methods,
            live,
        }) => {
            if protocol != PROTOCOL {
                return Err(Error::Protocol(format!(
                    "the host speaks protocol {protocol}; this app speaks {PROTOCOL}"
                )));
            }
            (app, methods, live)
        }
        _ => {
            return Err(Error::Protocol(
                "the host did not start with a hello".into(),
            ));
        }
    };
    send(
        &mut output,
        &AppMessage::Hello {
            protocol: PROTOCOL,
            name: Some(format!("dreamer-sdk {}", env!("CARGO_PKG_VERSION"))),
        },
    )
    .await?;
    let mut cx = Context::default();
    let mut out = view_tree::Outgoing::default();
    let mut msg = Some(Msg::Start {
        app: info,
        methods,
        live,
    });
    loop {
        if let Some(m) = msg.take() {
            app.update(m, &mut cx);
        }
        for (id, method, params) in std::mem::take(&mut cx.calls) {
            send(&mut output, &AppMessage::Call { id, method, params }).await?;
        }
        if cx.quit {
            return Ok(());
        }
        let frame: view_tree::Frame = app.view().into();
        frame.check().map_err(Error::Tree)?;
        match out.next(&frame) {
            Some(view_tree::Update::Full { frame, .. }) => {
                send(&mut output, &AppMessage::View { tree: frame }).await?;
            }
            Some(view_tree::Update::Patch { ops, .. }) => {
                send(&mut output, &AppMessage::Patch { ops }).await?;
            }
            None => {}
        }
        // The next message that means something to the app.
        while msg.is_none() {
            let Some(line) = read_line(&mut input).await? else {
                return Ok(());
            };
            if line.is_empty() {
                continue;
            }
            let m: HostMessage =
                serde_json::from_str(&line).map_err(|e| Error::Protocol(format!("{e}: {line}")))?;
            msg = match m {
                HostMessage::Hello { .. } => None,
                HostMessage::Event { event } => Some(Msg::Event(event)),
                HostMessage::Result { id, result, error } => {
                    cx.tags.remove(&id).map(|(tag, method)| Msg::Reply {
                        tag,
                        method,
                        result: match error {
                            Some(e) => Err(e),
                            None => Ok(result.unwrap_or(Value::Null)),
                        },
                    })
                }
                HostMessage::Notify { topic, payload } => Some(Msg::Notify { topic, payload }),
                HostMessage::Link { live, reason } => Some(Msg::Link { live, reason }),
                HostMessage::Resync { .. } => {
                    out.resync();
                    break;
                }
                HostMessage::Quit => return Ok(()),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui;

    struct Counter {
        n: i64,
        said: String,
    }

    impl App for Counter {
        fn view(&self) -> Screen {
            ui::screen(
                "Counter",
                ui::column(
                    "main",
                    [
                        ui::text("count", self.n.to_string()),
                        ui::text("said", self.said.clone()),
                    ],
                ),
            )
        }

        fn update(&mut self, msg: Msg, cx: &mut Context) {
            match msg {
                Msg::Event(Event::Key { key }) if key == "+" => self.n += 1,
                Msg::Event(Event::Key { key }) if key == "c" => {
                    cx.call("v", "daemon.version", serde_json::json!({}));
                }
                Msg::Event(Event::Key { key }) if key == "q" => cx.quit(),
                Msg::Reply { tag, result, .. } => {
                    self.said = format!(
                        "{tag}: {}",
                        result.map_or_else(|e| e.message, |_| "ok".into())
                    );
                }
                _ => {}
            }
        }
    }

    /// The host's side of a conversation: lines in, lines out.
    #[tokio::test]
    async fn the_loop_sends_the_tree_then_patches_and_matches_replies() {
        let host = [
            r#"{"type":"hello","protocol":1,"app":{"id":"t.c","version":"1","title":"C"},"methods":[],"live":true}"#,
            r#"{"type":"event","event":{"kind":"key","key":"+"}}"#,
            r#"{"type":"event","event":{"kind":"key","key":"c"}}"#,
            r#"{"type":"result","id":1,"error":{"code":-32005,"message":"refused"}}"#,
            r#"{"type":"resync","reason":"lost"}"#,
            r#"{"type":"event","event":{"kind":"key","key":"q"}}"#,
        ]
        .join("\n");
        let mut out = Vec::new();
        run_on(
            Counter {
                n: 0,
                said: String::new(),
            },
            host.as_bytes(),
            &mut out,
        )
        .await
        .expect("ran");
        let lines: Vec<Value> = String::from_utf8(out)
            .expect("utf-8")
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        let types: Vec<&str> = lines
            .iter()
            .map(|l| l["type"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(types, ["hello", "view", "patch", "call", "patch", "view"]);
        assert_eq!(lines[0]["protocol"], 1);
        // Only the changed node travels.
        assert_eq!(
            lines[2]["ops"][0]["path"],
            serde_json::json!(["main", "count"])
        );
        assert_eq!(lines[3]["method"], "daemon.version");
        assert_eq!(lines[4]["ops"][0]["kind"]["text"], "v: refused");
        // After a resync, the whole tree.
        assert_eq!(
            lines[5]["tree"]["root"]["children"][1]["text"],
            "v: refused"
        );
    }

    #[tokio::test]
    async fn a_host_of_another_protocol_is_refused() {
        let host = r#"{"type":"hello","protocol":9,"app":{"id":"t","version":"1","title":"T"},"methods":[],"live":false}"#;
        let r = run_on(
            Counter {
                n: 0,
                said: String::new(),
            },
            host.as_bytes(),
            Vec::new(),
        )
        .await;
        assert!(matches!(r, Err(Error::Protocol(e)) if e.contains("protocol 9")));
    }
}
