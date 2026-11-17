//! A process app: the tmux panes in a table beside a counter, with what
//! happened below. Run it from a package whose manifest names it:
//!
//! ```toml
//! schema_version = 2
//! id = "local.pane-app"
//! version = "0.1.0"
//! title = "Pane app"
//! capabilities = ["process", "tmux.topology"]
//!
//! [process]
//! command = ["/path/to/pane-app"]
//! ```
//!
//! Keys: `+` and `-` change the counter, Up and Down move through the
//! panes, `r` reads the panes again, `x` tries a call the app has no
//! capability for (the host refuses it), and `q` quits.

use dreamer_sdk::app::{self, App, Context, Event, Msg};
use dreamer_sdk::ui;
use serde_json::{Value, json};

#[derive(Default)]
struct Panes {
    count: i64,
    history: Vec<f64>,
    /// Each pane's id and command.
    panes: Vec<(u64, String)>,
    selected: usize,
    events: Vec<String>,
    outcome: String,
    live: bool,
}

impl Panes {
    fn said(&mut self, what: String) {
        self.events.push(what);
        if self.events.len() > 50 {
            self.events.remove(0);
        }
    }

    fn read_panes(cx: &mut Context) {
        cx.call("panes", "tmux.topology", json!({}));
    }
}

impl App for Panes {
    fn view(&self) -> ui::Screen {
        let mut table = ui::table("panes", ["id", "command"]).title("Panes");
        for (id, command) in &self.panes {
            table = table.row(id.to_string(), [format!("%{id}"), command.clone()]);
        }
        if let Some((id, _)) = self.panes.get(self.selected) {
            table = table.selected(id.to_string());
        }
        let mut log = ui::log("events").title("Events");
        for (i, e) in self.events.iter().enumerate() {
            log = log.item(i.to_string(), e.clone());
        }
        let ratio = (self.count as f64 / 10.0).clamp(0.0, 1.0);
        ui::screen(
            "Pane app",
            ui::column(
                "main",
                [
                    ui::split(
                        "top",
                        [
                            table,
                            ui::column(
                                "side",
                                [
                                    ui::gauge("level", ratio, format!("count {}", self.count))
                                        .title("Count"),
                                    ui::chart("history", self.history.clone()).title("History"),
                                ],
                            ),
                        ],
                    )
                    .sizes([60, 40]),
                    log,
                ],
            ),
        )
        .status(format!(
            "[{}] +/- count  r panes  x forbidden call  q quit  | {}",
            if self.live { "live" } else { "offline" },
            self.outcome
        ))
        .focus("panes")
    }

    fn update(&mut self, msg: Msg, cx: &mut Context) {
        match msg {
            Msg::Start { live, .. } => {
                self.live = live;
                Self::read_panes(cx);
            }
            Msg::Link { live, .. } => {
                self.live = live;
                if live {
                    Self::read_panes(cx);
                }
            }
            Msg::Event(Event::Key { key }) => {
                match key.as_str() {
                    "+" | "=" => self.count += 1,
                    "-" => self.count -= 1,
                    "Up" | "k" => self.selected = self.selected.saturating_sub(1),
                    "Down" | "j" => {
                        self.selected = (self.selected + 1).min(self.panes.len().saturating_sub(1));
                    }
                    "r" => Self::read_panes(cx),
                    // Harmless, but not among the manifest's capabilities.
                    "x" => cx.call("forbidden", "daemon.status", json!({})),
                    "q" => cx.quit(),
                    _ => {}
                }
                if matches!(key.as_str(), "+" | "=" | "-") {
                    self.history.push(self.count as f64);
                }
                self.said(format!("key {key}"));
            }
            Msg::Event(Event::Select { node, index }) => {
                if node.as_deref().is_none_or(|n| n == "panes") {
                    self.selected = (index as usize).min(self.panes.len().saturating_sub(1));
                }
            }
            Msg::Event(Event::Scroll { steps, .. }) => {
                let last = self.panes.len().saturating_sub(1) as i64;
                self.selected = (self.selected as i64 + steps).clamp(0, last.max(0)) as usize;
            }
            Msg::Event(e) => self.said(format!("{e:?}")),
            Msg::Reply { tag, result, .. } => match result {
                Ok(topology) if tag == "panes" => {
                    self.panes = topology["panes"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .map(|p| {
                                    (
                                        p["id"].as_u64().unwrap_or(0),
                                        p["current_command"].as_str().unwrap_or("").to_owned(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    self.outcome = format!("{} panes", self.panes.len());
                }
                Ok(_) => self.outcome = format!("{tag}: done"),
                Err(e) => {
                    self.outcome = format!("{tag}: refused: {}", e.message);
                    eprintln!("{tag}: {e}");
                }
            },
            Msg::Notify { topic, payload } => {
                self.said(format!(
                    "{topic}: {}",
                    payload.get("kind").unwrap_or(&Value::Null)
                ));
            }
        }
    }
}

fn main() -> Result<(), app::Error> {
    app::run_blocking(Panes::default())
}
