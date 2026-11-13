//! `dream app run`: start a program in a new tmux window as a registered
//! app with a narrow grant; and `dream app probe`, a minimal app that
//! registers and reports what it may do (used by tests).

use api::{Request, Response};

use crate::daemon::{DaemonArgs, call, connect};
use crate::remote::session;

fn rt() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

pub const USAGE: &str = "usage:
  dream app run APP_ID [--method METHOD]... [--all-panes] [--session $N] [--ttl-ms N]
                [--name NAME] -- PROGRAM [ARGS...]";

/// `dream app run`.
pub fn run(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let split = args.iter().position(|a| a == "--").ok_or(USAGE)?;
    let (opts, argv) = (&args[..split], &args[split + 1..]);
    let [app_id, rest @ ..] = opts else {
        return Err(USAGE.into());
    };
    if argv.is_empty() {
        return Err(USAGE.into());
    }
    let mut grant = api::Grant::default();
    let (mut session_id, mut ttl_ms, mut name) = (None, None, None);
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--method" => grant.methods.push(value()?),
            "--all-panes" => grant.all_panes = true,
            "--session" => {
                let v = value()?;
                session_id = Some(
                    v.trim_start_matches('$')
                        .parse::<u32>()
                        .map_err(|_| format!("`{v}` is not a session id"))?,
                );
            }
            "--ttl-ms" => {
                ttl_ms = Some(value()?.parse().map_err(|_| "--ttl-ms needs a number")?);
            }
            "--name" => name = Some(value()?),
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    rt()?.block_on(async {
        let mut c = session(d).await?;
        let session_id = match session_id {
            Some(s) => s,
            None => {
                let Response::TmuxTopology(t) = call(
                    &mut c,
                    Request::TmuxTopology(api::TopologyParams::default()),
                )
                .await?
                else {
                    return Err("unexpected reply".into());
                };
                t.sessions
                    .first()
                    .map(|s| s.id)
                    .ok_or("tmux has no sessions")?
            }
        };
        let Response::AppsLaunch(l) = call(
            &mut c,
            Request::AppsLaunch(api::LaunchApp {
                app_id: app_id.clone(),
                grant: Some(grant),
                argv: argv.to_vec(),
                session: session_id,
                name,
                cwd: None,
                ttl_ms,
            }),
        )
        .await?
        else {
            return Err("unexpected reply".into());
        };
        println!(
            "launched {app_id} in pane %{} (launch {})",
            l.pane, l.launch_id
        );
        Ok(())
    })
}

/// `dream app probe [--other %N] [--delay MS]`: register from the launch
/// environment, then try a few things and print what happened, one line
/// each, so a test can read them from the pane. Never prints the ticket.
pub fn probe(args: &[String]) -> Result<(), String> {
    let mut other = None;
    let mut delay = 0u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--other" => {
                other = it
                    .next()
                    .and_then(|p| p.trim_start_matches('%').parse::<u32>().ok());
            }
            "--delay" => delay = it.next().and_then(|d| d.parse().ok()).unwrap_or(0),
            _ => {}
        }
    }
    let id = std::env::var("DREAMER_LAUNCH_ID").map_err(|_| "no DREAMER_LAUNCH_ID")?;
    let path = std::env::var("DREAMER_LAUNCH_TICKET").map_err(|_| "no DREAMER_LAUNCH_TICKET")?;
    let ticket = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let own: Option<u32> = std::env::var("TMUX_PANE")
        .ok()
        .and_then(|p| p.trim_start_matches('%').parse().ok());
    std::thread::sleep(std::time::Duration::from_millis(delay));
    let outcome = |r: Result<Response, String>| match r {
        Ok(_) => "ok".to_owned(),
        Err(e) => format!("refused: {e}"),
    };
    rt()?.block_on(async {
        let mut c = connect().await?;
        let register = || {
            Request::AppsRegister(api::RegisterApp {
                launch_id: id.clone(),
                ticket: ticket.trim().to_owned(),
            })
        };
        match call(&mut c, register()).await {
            Ok(Response::AppsRegister(r)) => {
                let g = r.grant.unwrap_or_default();
                println!(
                    "probe: registered {} on %{} methods {:?} panes {:?}",
                    r.instance, r.pane, g.methods, g.panes
                );
            }
            Ok(_) => println!("probe: register: unexpected reply"),
            Err(e) => {
                println!("probe: register refused: {e}");
                return Ok(());
            }
        }
        let read = |pane| {
            Request::TerminalRead(api::ReadParams {
                pane,
                history: Some(0),
                endpoint: None,
            })
        };
        if let Some(own) = own {
            println!(
                "probe: read own pane: {}",
                outcome(call(&mut c, read(own)).await)
            );
        }
        if let Some(other) = other {
            println!(
                "probe: read other pane: {}",
                outcome(call(&mut c, read(other)).await)
            );
        }
        let stop = call(&mut c, Request::DaemonShutdown(api::Empty {})).await;
        println!("probe: shutdown: {}", outcome(stop));
        let launch = call(
            &mut c,
            Request::AppsLaunch(api::LaunchApp {
                app_id: "escalate".into(),
                argv: vec!["true".into()],
                ..api::LaunchApp::default()
            }),
        )
        .await;
        println!("probe: launch another app: {}", outcome(launch));
        // On this connection, registering again is outside the narrow grant;
        // a fresh connection shows the ticket itself is used up.
        println!(
            "probe: register again here: {}",
            outcome(call(&mut c, register()).await)
        );
        let mut fresh = connect().await?;
        println!(
            "probe: reuse ticket: {}",
            outcome(call(&mut fresh, register()).await)
        );
        println!("probe: done");
        Ok::<(), String>(())
    })?;
    // Stay, so the pane (and what it printed) can be read.
    std::thread::sleep(std::time::Duration::from_secs(120));
    Ok(())
}
