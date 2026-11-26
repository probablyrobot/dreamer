//! `dream` commands that talk to `dreamerd` over its socket.

use api::{Request, Response};
use client::{Client, ClientError, Launch, ensure_running};

/// Options that choose how a daemon is started (`ensure-running`) and which
/// tmux server it drives.
#[derive(Debug, Default, Clone)]
pub struct DaemonArgs {
    pub start: Vec<String>,
}

/// Split `--tmux`, `--tmux-socket`, `--tmux-socket-path` (passed to a
/// started daemon) from the remaining arguments.
pub fn split_daemon_args(args: &[&str]) -> Result<(DaemonArgs, Vec<String>), String> {
    let mut d = DaemonArgs::default();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--tmux" | "--tmux-socket" | "--tmux-socket-path" | "-L" => {
                let v = it.next().ok_or(format!("{a} needs a value"))?;
                let flag = if *a == "-L" { "--tmux-socket" } else { a };
                d.start.push(flag.to_owned());
                d.start.push((*v).to_owned());
            }
            // After `--` everything is the command's own (a prompt, a
            // program's arguments).
            "--" => {
                rest.push("--".to_owned());
                rest.extend(it.by_ref().map(|s| (*s).to_owned()));
            }
            other => rest.push(other.to_owned()),
        }
    }
    Ok((d, rest))
}

fn rt() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

pub async fn connect() -> Result<Client, String> {
    Client::connect(&platform::socket_path(), "cli")
        .await
        .map_err(explain)
}

pub fn explain(e: ClientError) -> String {
    match e {
        ClientError::NotRunning(_) => {
            "dreamerd is not running; start it with `dream ensure-running`".into()
        }
        other => other.to_string(),
    }
}

pub async fn call(c: &mut Client, r: Request) -> Result<Response, String> {
    c.call(r).await.map_err(explain)
}

pub fn version() -> Result<(), String> {
    println!("dream {}", env!("CARGO_PKG_VERSION"));
    println!("build family {}", terminal_core::family::build_family());
    rt()?.block_on(async {
        match Client::connect(&platform::socket_path(), "cli").await {
            Ok(mut c) => match call(&mut c, Request::DaemonVersion(api::Empty {})).await? {
                Response::DaemonVersion(v) => {
                    println!(
                        "dreamerd {} (protocol {}.{}, {}, pid {})",
                        v.daemon_version,
                        v.protocol_major,
                        v.protocol_minor,
                        v.platform,
                        c.welcome.pid
                    );
                    Ok(())
                }
                other => Err(format!("unexpected reply {other:?}")),
            },
            Err(ClientError::NotRunning(_)) => {
                println!("dreamerd not running");
                Ok(())
            }
            Err(e) => Err(explain(e)),
        }
    })
}

pub fn status(json: bool, perf: bool) -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = connect().await?;
        let Response::DaemonStatus(s) = call(&mut c, Request::DaemonStatus(api::Empty {})).await?
        else {
            return Err("unexpected reply".into());
        };
        let d = if perf {
            let Response::DaemonDiagnostics(d) =
                call(&mut c, Request::DaemonDiagnostics(api::Empty {})).await?
            else {
                return Err("unexpected reply".into());
            };
            Some(d)
        } else {
            None
        };
        if json {
            let v = match &d {
                Some(d) => serde_json::json!({ "status": s, "diagnostics": d }),
                None => serde_json::to_value(&s).map_err(|e| e.to_string())?,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        println!("dreamerd pid {} up {}s", s.pid, s.uptime_ms / 1000);
        println!("  socket   {}", s.socket_path);
        println!("  log      {}", s.log_path);
        println!("  clients  {}", s.clients);
        if let Some(h) = s.heap_in_use {
            println!("  heap     {:.1} MiB in use", h as f64 / (1024.0 * 1024.0));
        }
        if let Some(v) = &s.version {
            println!(
                "  version  {} (protocol {}.{}, {})",
                v.daemon_version, v.protocol_major, v.protocol_minor, v.build_family
            );
        }
        for e in &s.endpoints {
            let state = match api::LinkState::try_from(e.state) {
                Ok(api::LinkState::Unspecified) => "not connected",
                Ok(s) => s.name(),
                Err(_) => "unknown",
            };
            println!(
                "  tmux     {} ({}) {state}{}{}",
                e.name,
                e.socket,
                e.tmux_version
                    .as_deref()
                    .map(|v| format!(", {v}"))
                    .unwrap_or_default(),
                e.detail
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            );
        }
        if let Some(d) = &d {
            print_diagnostics(d);
        }
        Ok(())
    })
}

/// `dream diagnostics on|off|reset`: `reset` zeroes them and leaves them
/// as they are.
pub fn diagnostics(what: &str, json: bool) -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = connect().await?;
        let enabled = match what {
            "on" => true,
            "off" => false,
            _ => {
                let Response::DaemonDiagnostics(d) =
                    call(&mut c, Request::DaemonDiagnostics(api::Empty {})).await?
                else {
                    return Err("unexpected reply".into());
                };
                d.enabled
            }
        };
        let Response::DaemonSetDiagnostics(d) = call(
            &mut c,
            Request::DaemonSetDiagnostics(api::DiagnosticsSettings {
                enabled,
                reset: what == "reset",
            }),
        )
        .await?
        else {
            return Err("unexpected reply".into());
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&d).map_err(|e| e.to_string())?
            );
        } else {
            print_diagnostics(&d);
        }
        Ok(())
    })
}

fn print_diagnostics(d: &api::Diagnostics) {
    let us = |ns: u64| ns as f64 / 1000.0;
    if d.enabled {
        println!(
            "diagnostics on, collecting for {:.1}s",
            d.collecting_ms as f64 / 1000.0
        );
    } else {
        println!("diagnostics off (`dream diagnostics on` collects timings and counters)");
    }
    if d.enabled || d.stages.iter().any(|s| s.count > 0) {
        println!(
            "  {:<10} {:>10} {:>10} {:>10} {:>10} {:>10}",
            "stage", "count", "p50 µs", "p95 µs", "p99 µs", "max µs"
        );
        for s in &d.stages {
            println!(
                "  {:<10} {:>10} {:>10.1} {:>10.1} {:>10.1} {:>10.1}",
                s.stage,
                s.count,
                us(s.p50_ns),
                us(s.p95_ns),
                us(s.p99_ns),
                us(s.max_ns)
            );
        }
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
        println!("  read from tmux    {:.2} MiB", mib(d.tmux_bytes));
        println!(
            "  parsed            {:.2} MiB in {} records, {:.0} MiB/s while parsing",
            mib(d.parsed_bytes),
            d.records,
            mib(d.parser_bytes_per_second)
        );
        println!(
            "  sent to clients   {:.2} MiB in {} batches",
            mib(d.transferred_bytes),
            d.transferred_batches
        );
        println!(
            "  subscriptions     {} from checkpoints ({:.2} MiB), {} replays, {} ended lagged",
            d.checkpoints,
            mib(d.checkpoint_bytes),
            d.replays,
            d.lagged_subscribers
        );
        println!(
            "  overruns          {} mirrors rebuilt; {:.2} MiB of Cold output dropped",
            d.reattaches,
            mib(d.cold_dropped_bytes)
        );
    }
    println!(
        "  queues now        tmux events {}, held output {} panes / {} bytes, attaching {} (+{} waiting)",
        d.event_queue, d.backlog_panes, d.backlog_bytes, d.attaching, d.attach_queue
    );
    println!(
        "                    VT owners {:?} of {}, subscribers {} (fullest {} of {})",
        d.worker_queues,
        d.worker_queue_capacity,
        d.subscribers,
        d.subscriber_queue_max,
        d.subscriber_queue_capacity
    );
    if !d.views.is_empty() {
        println!(
            "  views             {:<6} {:<8} {:<8} {:<10} {:>8} {:>10}",
            "pane", "feed", "switches", "by", "to frames", "output/s"
        );
    }
    for v in &d.views {
        let feed = if v.feed == api::TerminalFeed::Frames as i32 {
            "frames"
        } else {
            "stream"
        };
        let how = match (v.automatic, v.held) {
            (_, true) => "held",
            (true, false) => "automatic",
            (false, false) => "fixed",
        };
        println!(
            "                    %{:<5} {:<8} {:<8} {:<10} {:>8} {:>10}",
            v.pane,
            feed,
            v.switches,
            how,
            v.to_frames,
            if v.automatic {
                format!("{:.0} KiB", v.rate as f64 / 1024.0)
            } else {
                "-".into()
            }
        );
    }
}

pub fn ensure(d: &DaemonArgs) -> Result<(), String> {
    rt()?.block_on(async {
        let (c, started) = ensure_running(&Launch::new(d.start.clone()), "cli")
            .await
            .map_err(explain)?;
        println!(
            "dreamerd {} (pid {}, instance {})",
            if started {
                "started"
            } else {
                "already running"
            },
            c.welcome.pid,
            c.welcome.instance
        );
        Ok(())
    })
}

/// Stop a running daemon, if any, saying nothing. tmux keeps running.
pub fn shutdown_quietly() -> Result<(), String> {
    rt()?.block_on(async {
        if let Ok(c) = Client::connect(&platform::socket_path(), "cli").await {
            let _ = c.call(Request::DaemonShutdown(api::Empty {})).await;
        }
        Ok(())
    })
}

pub fn shutdown() -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = match Client::connect(&platform::socket_path(), "cli").await {
            Ok(c) => c,
            // A daemon from another installation refuses this client's
            // protocol, but it is still this user's daemon: stop it by the
            // pid in its startup lock.
            Err(ClientError::Handshake(e)) => {
                let pid = platform::lock_holder()
                    .filter(|p| platform::process_alive(*p))
                    .ok_or(format!("{e}; and no running daemon is recorded in {}", platform::lock_path().display()))?;
                platform::terminate(pid)?;
                println!("dreamerd (pid {pid}) is from another build ({e}); sent it SIGTERM. tmux keeps running.");
                return Ok(());
            }
            Err(e) => return Err(explain(e)),
        };
        let Response::DaemonShutdown(r) = call(&mut c, Request::DaemonShutdown(api::Empty {})).await? else {
            return Err("unexpected reply".into());
        };
        println!("{}", r.effect);
        Ok(())
    })
}
