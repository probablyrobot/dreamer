//! An example extension: follows the daemon's events and, after each
//! batch, publishes a summary on `summary.events`: how many events of each
//! kind it has seen, and whether any were missed.
//!
//! Configure it in `extensions.yaml` (see `docs/extensions.md`):
//!
//! ```yaml
//! extensions:
//!   - name: summary
//!     argv: [/path/to/event-summary]
//!     grant:
//!       methods: [events.subscribe, messages.publish]
//!       all_panes: true
//! ```

use std::collections::BTreeMap;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut session = dreamer_sdk::start("event-summary 0.1").await?;
    eprintln!("event-summary: started as {}", session.hello.name);
    let mut notifications = session.notifications().ok_or("notifications taken")?;
    session
        .call(api::Request::EventsSubscribe(api::EventSubscribe::default()))
        .await?;
    let mut kinds: BTreeMap<String, u64> = BTreeMap::new();
    let mut gaps = 0u64;
    loop {
        tokio::select! {
            Some(n) = notifications.recv() => {
                let api::Notification::EventsBatch(batch) = n else { continue };
                if batch.gap.is_some() {
                    gaps += 1;
                }
                for e in &batch.events {
                    *kinds.entry(e.kind.clone()).or_default() += 1;
                }
                let summary = serde_json::json!({
                    "kinds": kinds,
                    "gaps": gaps,
                    "cursor": batch.cursor,
                });
                if let Err(e) = session.publish("summary.events", summary.to_string().into_bytes()).await {
                    eprintln!("event-summary: publish: {e}");
                }
            }
            () = session.stopped() => {
                eprintln!("event-summary: stopping");
                return Ok(());
            }
        }
    }
}
