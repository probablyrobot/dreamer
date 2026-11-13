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

pub fn status(json: bool) -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = connect().await?;
        let Response::DaemonStatus(s) = call(&mut c, Request::DaemonStatus(api::Empty {})).await?
        else {
            return Err("unexpected reply".into());
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        println!("dreamerd pid {} up {}s", s.pid, s.uptime_ms / 1000);
        println!("  socket   {}", s.socket_path);
        println!("  log      {}", s.log_path);
        println!("  clients  {}", s.clients);
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
        Ok(())
    })
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
