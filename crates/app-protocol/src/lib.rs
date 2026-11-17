//! The app process protocol: how the app host (`dreamer-termapp`) and an
//! app process talk, as JSON lines on the process's standard input and
//! output (see `docs/app-protocol.md`).
//!
//! - The host starts with [`HostMessage::Hello`]; the app answers with
//!   [`AppMessage::Hello`], naming the same [`PROTOCOL`] version.
//! - The app sends its view: the whole tree ([`AppMessage::View`]), then
//!   keyed changes to it ([`AppMessage::Patch`]), as `view-tree` defines
//!   them. A patch the host cannot apply gets [`HostMessage::Resync`].
//! - The host sends input as [`HostMessage::Event`]s.
//! - The app calls the daemon's API with [`AppMessage::Call`]; the host
//!   answers each with [`HostMessage::Result`] under the app's grant.
//!
//! Each message is one JSON object on one line, tagged by `type`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol version this crate speaks. A host and an app that name
/// different versions do not talk.
pub const PROTOCOL: u32 = 1;

/// The longest line either side accepts, in bytes (a large tree).
pub const MAX_LINE: usize = 16 * 1024 * 1024;

/// What the host sends the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    /// The first message: the protocol, the app, and what its grant lets
    /// it call.
    Hello {
        protocol: u32,
        app: AppInfo,
        /// The methods the app may call: its manifest's capabilities
        /// (`service.*` for a whole service).
        methods: Vec<String>,
        /// Whether the host is connected to the daemon now.
        live: bool,
    },
    /// Input for the app.
    Event { event: Event },
    /// The answer to the app's call `id`: its `result`, or an `error`.
    Result {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<CallError>,
    },
    /// An event from a subscription the app made (`events.subscribe`):
    /// its topic and its payload as the API's JSON.
    Notify { topic: String, payload: Value },
    /// The host's connection to the daemon came up or went away. Calls
    /// fail while it is down.
    Link {
        live: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The last patch did not apply: send the whole tree next.
    Resync { reason: String },
    /// Stop: the user quit. The host closes the app's input next.
    Quit,
}

/// The app, as its manifest declares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    pub id: String,
    pub version: String,
    pub title: String,
}

/// What the app sends the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppMessage {
    /// The answer to the host's hello, before anything else.
    Hello {
        protocol: u32,
        /// The app's name and version, for logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// The whole view tree.
    View { tree: view_tree::Frame },
    /// Changes to the last tree the host holds, in order.
    Patch { ops: Vec<view_tree::Op> },
    /// Call an API method under the app's grant. `id` is the app's, and
    /// comes back with the answer.
    Call {
        id: u64,
        method: String,
        #[serde(default = "empty_object")]
        params: Value,
    },
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Input for the app, from the pane's terminal or a desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// A key by name: `a`, `Enter`, `Ctrl-c`, `F2`, `Up`, ...
    Key { key: String },
    /// Text for the focused input as one edit: committed by a desktop's
    /// text input, or pasted.
    Text { text: String },
    /// A click: row or field `index` of node `node` (else of the focused
    /// node).
    Select {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
        index: u64,
    },
    /// Wheel steps over node `node` (else the focused node); positive is
    /// down.
    Scroll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
        steps: i64,
    },
    /// A component's own event, such as a button pressed or an input's
    /// value changed: the node, what happened, and a value.
    Component {
        node: String,
        action: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        value: Value,
    },
}

/// Why a call failed: the API's error code and message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// One message as a line, with its newline.
pub fn line<T: Serialize>(message: &T) -> String {
    let mut s = serde_json::to_string(message).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_read_as_documented() {
        let hello: AppMessage =
            serde_json::from_str(r#"{"type":"hello","protocol":1}"#).expect("hello");
        assert_eq!(
            hello,
            AppMessage::Hello {
                protocol: 1,
                name: None
            }
        );
        let call: AppMessage =
            serde_json::from_str(r#"{"type":"call","id":7,"method":"tmux.topology"}"#)
                .expect("call");
        assert_eq!(
            call,
            AppMessage::Call {
                id: 7,
                method: "tmux.topology".into(),
                params: serde_json::json!({}),
            }
        );
        let view: AppMessage = serde_json::from_str(
            r#"{"type":"view","tree":{"title":"T","status":"","root":{"key":"m","type":"text","text":"hi"}}}"#,
        )
        .expect("view");
        assert!(matches!(view, AppMessage::View { .. }));
        assert_eq!(
            line(&HostMessage::Event {
                event: Event::Key { key: "q".into() }
            }),
            "{\"type\":\"event\",\"event\":{\"kind\":\"key\",\"key\":\"q\"}}\n"
        );
        assert_eq!(
            line(&HostMessage::Result {
                id: 3,
                result: None,
                error: Some(CallError {
                    code: -32005,
                    message: "no".into()
                })
            }),
            "{\"type\":\"result\",\"id\":3,\"error\":{\"code\":-32005,\"message\":\"no\"}}\n"
        );
    }
}
