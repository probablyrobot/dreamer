//! The runtime driven as the app host drives it: protocol lines in,
//! lines out.

use std::time::Duration;

use app_model::{EngineOut, EngineStart};
use app_protocol::{AppInfo, AppMessage, CallError, Event, HostMessage, PROTOCOL};
use tokio::sync::mpsc;
use view_tree::{Frame, Kind};

use super::{Limits, spawn};

struct Host {
    to_app: mpsc::Sender<String>,
    from_app: mpsc::UnboundedReceiver<EngineOut>,
    view: Option<Frame>,
    calls: Vec<(u64, String, serde_json::Value)>,
}

impl Host {
    fn start(js: &str, methods: &[&str], limits: Limits) -> Host {
        let (to_app, input) = mpsc::channel(64);
        let (output, from_app) = mpsc::unbounded_channel();
        let app = AppInfo {
            id: "test.app".into(),
            version: "0.1.0".into(),
            title: "Test".into(),
        };
        spawn(
            EngineStart {
                app: app.clone(),
                name: "app.ts".into(),
                js: js.into(),
                fixture: None,
                input,
                output,
            },
            limits,
        )
        .expect("spawn");
        let host = Host {
            to_app,
            from_app,
            view: None,
            calls: Vec::new(),
        };
        host.send(&HostMessage::Hello {
            protocol: PROTOCOL,
            app,
            methods: methods.iter().map(|m| (*m).to_owned()).collect(),
            live: true,
        });
        host
    }

    fn send(&self, m: &HostMessage) {
        self.to_app
            .try_send(app_protocol::line(m))
            .expect("the app reads");
    }

    fn key(&self, k: &str) {
        self.send(&HostMessage::Event {
            event: Event::Key { key: k.into() },
        });
    }

    /// Read what the app says until `done` holds; what ended it otherwise.
    async fn until(&mut self, what: &str, done: impl Fn(&Host) -> bool) -> Option<EngineOut> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if done(self) {
                return None;
            }
            let o = tokio::time::timeout_at(deadline, self.from_app.recv())
                .await
                .unwrap_or_else(|_| panic!("never {what}"));
            match o {
                Some(EngineOut::Line(l)) => match serde_json::from_str(&l).expect("a message") {
                    AppMessage::Hello { protocol, .. } => assert_eq!(protocol, PROTOCOL),
                    AppMessage::View { tree } => self.view = Some(tree),
                    AppMessage::Patch { ops } => {
                        let v = self.view.as_mut().expect("a view before a patch");
                        view_tree::apply(v, &ops).expect("the patch applies");
                    }
                    AppMessage::Call { id, method, params } => {
                        self.calls.push((id, method, params))
                    }
                },
                Some(EngineOut::Log(_)) => {}
                other => return other,
            }
        }
    }

    fn text(&self, key: &str) -> Option<String> {
        let n = self.view.as_ref()?.root.find(key)?;
        n.kind.text().map(str::to_owned)
    }
}

const COUNTER: &str = r#"
import { app, ui } from "dreamer";
app({
  state: { n: 0, said: "" },
  view(s) {
    return ui.screen(
      ui.column({ key: "main", children: [
        ui.text({ key: "count", text: String(s.n) }),
        ui.input({ key: "name", value: "", onSubmit: (v) => ({ said: "hello " + v }) }),
        ui.button({ key: "add", label: "Add", onPress: (ctx) => ({ n: ctx.state.n + 1 }) }),
        ui.text({ key: "said", text: s.said }),
      ]}),
      { status: "q quit" },
    );
  },
});
"#;

#[tokio::test]
async fn handlers_change_the_state_and_the_view_follows_as_patches() {
    let mut h = Host::start(COUNTER, &[], Limits::default());
    h.until("drew", |h| h.text("count").as_deref() == Some("0"))
        .await;
    assert_eq!(h.view.as_ref().map(|v| v.status.as_str()), Some("q quit"));
    // Typing goes to the focused input; Enter submits it.
    for k in ["b", "o", "b", "Enter"] {
        h.key(k);
    }
    h.until("said hello", |h| {
        h.text("said").as_deref() == Some("hello bob")
    })
    .await;
    let input = h
        .view
        .as_ref()
        .and_then(|v| v.root.find("name"))
        .map(|n| n.kind.clone());
    assert!(matches!(input, Some(Kind::Input { value, .. }) if value == "bob"));
    // Tab to the button and press it.
    h.key("Tab");
    h.key("Enter");
    h.until("counted", |h| h.text("count").as_deref() == Some("1"))
        .await;
    h.key("q");
    assert_eq!(h.until("quit", |_| false).await, Some(EngineOut::Quit));
}

#[tokio::test]
async fn calls_go_to_the_host_and_their_answers_come_back() {
    let js = r#"
app({
  async load() {
    const t = await dreamer.tmux.topology();
    let refused = "";
    try { await dreamer.call("daemon.shutdown"); } catch (e) { refused = e.code + " " + e.message; }
    return { panes: t.panes.length, refused, has: typeof dreamer.daemon };
  },
  view(s, d) { return ui.text({ key: "t", text: d.panes + "|" + d.refused + "|" + d.has }); },
});
"#;
    let mut h = Host::start(js, &["tmux.topology"], Limits::default());
    h.until("called", |h| !h.calls.is_empty()).await;
    let (id, method, _) = h.calls[0].clone();
    assert_eq!(method, "tmux.topology");
    h.send(&HostMessage::Result {
        id,
        result: Some(serde_json::json!({ "panes": [{}, {}] })),
        error: None,
    });
    h.until("called again", |h| h.calls.len() == 2).await;
    let (id, method, _) = h.calls[1].clone();
    assert_eq!(method, "daemon.shutdown");
    // The host refuses it (its capability check); the app hears why.
    h.send(&HostMessage::Result {
        id,
        result: None,
        error: Some(CallError {
            code: -32005,
            message: "not granted".into(),
        }),
    });
    h.until("drew", |h| h.text("t").is_some()).await;
    // Only granted methods are in `dreamer`.
    assert_eq!(
        h.text("t").as_deref(),
        Some("2|-32005 not granted|undefined")
    );
}

#[tokio::test]
async fn an_endless_handler_is_stopped_and_the_app_fails() {
    let js = r#"
app({
  view() { return ui.button({ key: "b", label: "spin", onPress() { for (;;) {} } }); },
});
"#;
    // Long enough that loading and the first view finish in time under
    // emulation, so only the endless handler runs into it.
    let limits = Limits {
        call: Duration::from_millis(750),
        ..Limits::default()
    };
    let mut h = Host::start(js, &[], limits);
    h.until("drew", |h| h.view.is_some()).await;
    let started = std::time::Instant::now();
    h.key("Enter");
    match h.until("stopped", |_| false).await {
        Some(EngineOut::Failed(why)) => {
            assert!(why.contains("a handler ran longer than 750 ms"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn running_out_of_memory_stops_the_app() {
    let js = r#"
app({
  view() { return ui.button({ key: "b", label: "grow", onPress() { const a = []; for (;;) a.push(new Array(1000).fill(1)); } }); },
});
"#;
    let limits = Limits {
        call: Duration::from_secs(5),
        memory: 16 << 20,
    };
    let mut h = Host::start(js, &[], limits);
    h.until("drew", |h| h.view.is_some()).await;
    h.key("Enter");
    match h.until("stopped", |_| false).await {
        Some(EngineOut::Failed(why)) => assert!(why.contains("out of memory"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn errors_in_handlers_show_on_the_status_line_and_timers_run() {
    let js = r#"
app({
  state: { ticks: 0 },
  load(ctx) { setInterval(() => ctx.set({ ticks: ctx.state.ticks + 1 }), 20); return {}; },
  view(s) { return ui.button({ key: "b", label: "ticks " + Math.min(s.ticks, 3), onPress() { throw new Error("nope"); } }); },
});
"#;
    let mut h = Host::start(js, &[], Limits::default());
    let label = |h: &Host| match h.view.as_ref().map(|v| &v.root.kind) {
        Some(Kind::Button { label }) => label.clone(),
        _ => String::new(),
    };
    h.until("ticked", |h| label(h) == "ticks 3").await;
    h.key("Enter");
    h.until("said why", |h| {
        h.view.as_ref().is_some_and(|v| v.status == "error: nope")
    })
    .await;
}

#[test]
fn syntax_errors_name_their_place() {
    assert!(super::check("app.ts", "app({ view() { return 1; } });").is_ok());
    let e = super::check("app.ts", "let a = ;\n").expect_err("a syntax error");
    assert!(e.contains("app.ts:1"), "{e}");
}

/// A value of a `ui.yaml` type, as JavaScript.
fn sample(ty: &str, types: &serde_json::Value) -> String {
    if ty.starts_with('(') {
        return "() => undefined".into();
    }
    let ty = ty.split('|').next().unwrap_or_default().trim();
    if let Some(inner) = ty.strip_suffix("[]") {
        return format!("[{}]", sample(inner, types));
    }
    match ty {
        "string" => "\"x\"".into(),
        "number" => "1".into(),
        "boolean" => "true".into(),
        "unknown" | "any" => "{ id: \"r1\", x: 1 }".into(),
        "Child" => "ui.text(\"c\")".into(),
        name => {
            let t = &types[name];
            if let Some(v) = t["values"].as_array() {
                format!("{}", v[0])
            } else {
                let fields = t["fields"].as_object().expect("a ui.yaml type");
                let f: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", sample(v.as_str().unwrap_or_default(), types)))
                    .collect();
                format!("{{ {} }}", f.join(", "))
            }
        }
    }
}

/// `dreamer.d.ts` declares the builders from `ui.yaml`: it must name every
/// builder the prelude has, and each builder, given every property it
/// declares, must make a node of its name that view-tree accepts, with no
/// field on the wire it does not declare.
#[tokio::test]
async fn the_declared_ui_builders_are_the_preludes_and_make_valid_nodes() {
    let types: serde_json::Value = serde_json::from_str(api::APP_TYPES).expect("app-types.json");
    let ui = &types["ui"];
    let components = ui["components"].as_array().expect("components");
    let mut names: Vec<&str> = components
        .iter()
        .filter_map(|c| c["name"].as_str())
        .chain(["screen"])
        .collect();
    names.sort_unstable();
    let mut built = Vec::new();
    for c in components {
        let name = c["name"].as_str().expect("name");
        let props = c["props"].as_object().expect("props");
        let mut p: Vec<String> = props
            .iter()
            .map(|(k, v)| {
                format!(
                    "{k:?}: {}",
                    sample(v.as_str().unwrap_or_default(), &ui["types"])
                )
            })
            .collect();
        p.push(format!("key: {name:?}"));
        p.push("title: \"t\"".into());
        built.push(format!("ui.{name}({{ {} }})", p.join(", ")));
    }
    let js = format!(
        "app({{ view() {{ return ui.screen(ui.column({{ key: \"root\", children: [\n\
         ui.text({{ key: \"keys\", text: Object.keys(ui).sort().join(\",\") }}),\n{}\n] }}), {{ status: \"s\" }}); }} }});\n",
        built.join(",\n")
    );
    let mut h = Host::start(&js, &[], Limits::default());
    if let Some(end) = h.until("drew", |h| h.view.is_some()).await {
        panic!("{end:?}\n{js}");
    }
    assert_eq!(
        h.text("keys").unwrap_or_default(),
        names.join(","),
        "ui.yaml's components are not the prelude's builders"
    );
    let view = h.view.as_ref().expect("view");
    for c in components {
        let name = c["name"].as_str().expect("name");
        let node = view
            .root
            .find(name)
            .unwrap_or_else(|| panic!("no {name} node"));
        assert_eq!(node.kind.name(), name);
        let wire = serde_json::to_value(node).expect("node json");
        for field in wire.as_object().expect("an object").keys() {
            let declared = ["key", "type", "title", "children"].contains(&field.as_str())
                || c["props"].get(field).is_some();
            assert!(
                declared,
                "ui.{name} sends `{field}`, which ui.yaml does not declare"
            );
        }
    }
}
