//! `dream extension-probe`: a minimal supervised extension, for tests.
//! It speaks JSON-RPC on standard input and output and writes what happens
//! to standard error (the extension's log).
//!
//! - `--crash-after N`: exit with status 3 after answering N pings.
//! - `--stall-after N`: after N pings, stop answering (and keep running).
//! - `--try METHOD`: once started, call METHOD and log the outcome.
//! - `--publish TOPIC`: once started, publish a message on TOPIC.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

pub fn run(args: &[String]) -> Result<(), String> {
    let mut crash_after = None;
    let mut stall_after = None;
    let mut tries = Vec::new();
    let mut publish = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut num = || it.next().and_then(|n| n.parse::<u64>().ok());
        match a.as_str() {
            "--crash-after" => crash_after = num(),
            "--stall-after" => stall_after = num(),
            "--try" => tries.extend(it.next().cloned()),
            "--publish" => publish.extend(it.next().cloned()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let name = std::env::var("DREAMER_EXTENSION").unwrap_or_default();
    eprintln!("probe: started {name} pid {}", std::process::id());
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    let mut send = |v: Value| -> std::io::Result<()> {
        writeln!(out, "{v}")?;
        out.flush()
    };
    let mut pings = 0u64;
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            eprintln!("probe: not JSON: {line}");
            continue;
        };
        let id = msg.get("id").cloned();
        match msg.get("method").and_then(Value::as_str) {
            Some("extension.hello") => {
                eprintln!("probe: hello {}", msg["params"]);
                send(json!({ "jsonrpc": "2.0", "id": id, "result": { "version": "probe 1" } }))
                    .map_err(|e| e.to_string())?;
                for (n, m) in tries.iter().enumerate() {
                    send(json!({ "jsonrpc": "2.0", "id": format!("try-{n}"), "method": m, "params": {} }))
                        .map_err(|e| e.to_string())?;
                }
                for t in &publish {
                    send(json!({
                        "jsonrpc": "2.0", "id": format!("pub-{t}"), "method": "messages.publish",
                        "params": { "topic": t, "payload": "aGVsbG8=", "sender": "owner" },
                    }))
                    .map_err(|e| e.to_string())?;
                }
            }
            Some("$/ping") => {
                pings += 1;
                if crash_after.is_some_and(|n| pings > n) {
                    eprintln!("probe: crashing after {} pings", pings - 1);
                    std::process::exit(3);
                }
                if stall_after.is_some_and(|n| pings > n) {
                    eprintln!("probe: stalling after {} pings", pings - 1);
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                    }
                }
                send(json!({ "jsonrpc": "2.0", "id": id, "result": {} }))
                    .map_err(|e| e.to_string())?;
            }
            Some("extension.shutdown") => {
                eprintln!("probe: shutdown");
                send(json!({ "jsonrpc": "2.0", "id": id, "result": {} }))
                    .map_err(|e| e.to_string())?;
                return Ok(());
            }
            Some(other) => {
                send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("no method {other}") } }))
                    .map_err(|e| e.to_string())?;
            }
            // A reply to one of our calls.
            None => eprintln!("probe: reply {line}"),
        }
    }
    eprintln!("probe: input ended");
    Ok(())
}
