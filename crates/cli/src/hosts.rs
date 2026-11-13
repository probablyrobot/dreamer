//! `dream hosts`: the local daemon's connections to other hosts' daemons
//! over SSH (the coordinator role).

use api::{Request, Response};

use crate::daemon::{DaemonArgs, call, connect};
use crate::remote::session;

fn rt() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

pub fn run(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["connect", name, destination, rest @ ..] if !destination.starts_with("--") => {
            connect_host(d, name, destination, rest)
        }
        ["connect", name, rest @ ..] => connect_host(d, name, "", rest),
        ["reconnect", name] => reconnect(d, name),
        ["install", name, rest @ ..] => install(name, rest, false),
        ["upgrade", name, rest @ ..] => install(name, rest, true),
        ["verify", name] => verify(name, false),
        ["verify", name, "--wait"] => verify(name, true),
        ["list"] => list(false),
        ["list", "--json"] => list(true),
        ["disconnect", name] => disconnect(name),
        ["call", name, method] => forward(name, method, "{}"),
        ["call", name, method, params] => forward(name, method, params),
        _ => Err(USAGE.into()),
    }
}

pub const USAGE: &str = "usage:
  dream hosts connect PROFILE [--auto-reconnect | --manual]
  dream hosts connect NAME DESTINATION [--dream PATH] [--remote-tmux PATH]
                      [--remote-socket NAME | --remote-socket-path PATH]
                      [--auto-reconnect | --manual]
  dream hosts reconnect NAME
  dream hosts verify NAME [--wait]
  dream hosts install NAME [--artifacts DIR] [--yes] [--wait]
  dream hosts upgrade NAME [--artifacts DIR] [--yes] [--allow-older] [--wait]
  dream hosts list [--json]
  dream hosts call NAME METHOD [JSON-PARAMS]
  dream hosts disconnect NAME";

fn connect_host(
    d: &DaemonArgs,
    name: &str,
    destination: &str,
    rest: &[&str],
) -> Result<(), String> {
    let mut p = api::HostConnect {
        name: name.into(),
        destination: destination.into(),
        ..api::HostConnect::default()
    };
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match *flag {
            "--auto-reconnect" => {
                p.auto_reconnect = Some(true);
                continue;
            }
            "--manual" => {
                p.auto_reconnect = Some(false);
                continue;
            }
            _ => {}
        }
        let value = it
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?
            .to_string();
        match *flag {
            "--dream" => p.dream = Some(value),
            "--remote-tmux" => p.tmux = Some(value),
            "--remote-socket" => p.tmux_socket = Some(value),
            "--remote-socket-path" => p.tmux_socket_path = Some(value),
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    rt()?.block_on(async {
        let mut c = session(d).await?;
        match call(&mut c, Request::HostsConnect(p)).await? {
            Response::HostsConnect(s) => {
                print_status(&s);
                Ok(())
            }
            other => Err(format!("unexpected reply {other:?}")),
        }
    })
}

fn reconnect(d: &DaemonArgs, name: &str) -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = session(d).await?;
        match call(
            &mut c,
            Request::HostsReconnect(api::HostRef { name: name.into() }),
        )
        .await?
        {
            Response::HostsReconnect(s) => {
                print_status(&s);
                Ok(())
            }
            other => Err(format!("unexpected reply {other:?}")),
        }
    })
}

/// Install this version's server on a host: show the plan, ask, install
/// exactly that plan, then connect.
fn install(name: &str, rest: &[&str], upgrade: bool) -> Result<(), String> {
    let mut artifacts = None;
    let (mut yes, mut wait, mut allow_older) = (false, false, false);
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match *a {
            "--artifacts" => {
                artifacts = Some(
                    it.next()
                        .ok_or("--artifacts needs a directory")?
                        .to_string(),
                );
            }
            "--yes" => yes = true,
            "--wait" => wait = true,
            "--allow-older" if upgrade => allow_older = true,
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    let result = install_inner(name, artifacts, yes, upgrade.then_some(allow_older));
    if let Err(e) = &result {
        eprintln!("dream: {e}");
    }
    if wait {
        println!("\nPress Enter to close.");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
    result.map_err(|_| "the install did not complete".into())
}

/// Show the plan, ask, and install it; with `upgrade` (`Some(allow
/// older)`), also restart the host's daemon on it.
fn install_inner(
    name: &str,
    artifacts: Option<String>,
    yes: bool,
    upgrade: Option<bool>,
) -> Result<(), String> {
    let rt = rt()?;
    let plan = rt.block_on(async {
        let mut c = connect().await?;
        match call(
            &mut c,
            Request::HostsInstallPlan(api::InstallPlanParams {
                name: name.into(),
                artifacts: artifacts.clone(),
            }),
        )
        .await?
        {
            Response::HostsInstallPlan(p) => Ok(p),
            other => Err(format!("unexpected reply {other:?}")),
        }
    })?;
    println!(
        "{} ({}) is {} {}: target {}",
        plan.name, plan.destination, plan.remote_os, plan.remote_arch, plan.target
    );
    println!(
        "installed now: {}",
        plan.installed.as_deref().unwrap_or("nothing")
    );
    println!("would install dreamer server {}:", plan.version);
    for (i, step) in plan.steps.iter().enumerate() {
        println!("  {}. {step}", i + 1);
    }
    if upgrade.is_some() {
        println!(
            "  {}. Restart the host's daemon on it. tmux sessions and panes keep running; views start again from fresh checkpoints",
            plan.steps.len() + 1
        );
    }
    if !yes {
        print!("Install? [y/N] ");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Err("not installed".into());
        }
    }
    if let Some(allow_older) = upgrade {
        return rt.block_on(async {
            let mut c = connect().await?;
            let Response::HostsUpgrade(r) = call(
                &mut c,
                Request::HostsUpgrade(api::UpgradeParams {
                    name: name.into(),
                    artifacts,
                    plan_id: plan.plan_id.clone(),
                    allow_older,
                }),
            )
            .await?
            else {
                return Err("unexpected reply".to_string());
            };
            println!(
                "{} -> {}: {}",
                r.from.as_deref().unwrap_or("nothing"),
                r.to,
                r.note
            );
            if let (Some(a), Some(b)) = (r.pid_before, r.pid_after) {
                println!("daemon pid {a} -> {b}");
            }
            Ok(())
        });
    }
    rt.block_on(async {
        let mut c = connect().await?;
        let Response::HostsInstall(r) = call(
            &mut c,
            Request::HostsInstall(api::InstallParams {
                name: name.into(),
                artifacts,
                plan_id: plan.plan_id.clone(),
            }),
        )
        .await?
        else {
            return Err("unexpected reply".to_string());
        };
        println!(
            "installed {} at {} ({}){}",
            r.version,
            r.remote_dream,
            r.check,
            r.previous
                .map(|p| format!("; previous: {p}"))
                .unwrap_or_default()
        );
        match call(
            &mut c,
            Request::HostsReconnect(api::HostRef { name: name.into() }),
        )
        .await
        {
            Ok(Response::HostsReconnect(s)) => print_status(&s),
            Ok(_) => {}
            Err(e) => println!("connecting afterwards failed: {e}"),
        }
        Ok(())
    })
}

/// A visible first connection: the user's own `ssh` to the host in this
/// terminal, so its questions (an unknown host key, a passphrase) are
/// answered by the user. Then the coordinator connects. With `--wait`, wait
/// for Enter before ending, so a window opened for this stays readable.
fn verify(name: &str, wait: bool) -> Result<(), String> {
    let result = verify_inner(name);
    if let Err(e) = &result {
        eprintln!("dream: {e}");
    }
    if wait {
        println!("\nPress Enter to close.");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
    result.map_err(|_| "verification did not complete".into())
}

fn verify_inner(name: &str) -> Result<(), String> {
    let rt = rt()?;
    let destination = rt.block_on(async {
        let mut c = connect().await?;
        let Response::HostsList(l) = call(&mut c, Request::HostsList(api::Empty {})).await? else {
            return Err("unexpected reply".to_string());
        };
        l.hosts
            .into_iter()
            .find(|h| h.name == name)
            .map(|h| h.destination)
            .ok_or_else(|| format!("no host named `{name}`"))
    })?;
    let ssh = std::env::var("DREAMER_SSH").unwrap_or_else(|_| "ssh".into());
    let argv = client::ssh::verify_command(&ssh, &destination).map_err(|e| e.to_string())?;
    println!("Connecting to {destination} with your ssh.");
    println!("Answer its questions here; Dreamer never answers them for you.\n");
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    if !status.success() {
        return Err(format!("ssh ended with {status}; `{name}` stays as it was"));
    }
    println!("\nSSH works. Connecting Dreamer to `{name}`…");
    rt.block_on(async {
        let mut c = connect().await?;
        match call(
            &mut c,
            Request::HostsReconnect(api::HostRef { name: name.into() }),
        )
        .await?
        {
            Response::HostsReconnect(s) => {
                print_status(&s);
                Ok(())
            }
            other => Err(format!("unexpected reply {other:?}")),
        }
    })
}

fn list(json: bool) -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = connect().await?;
        let Response::HostsList(l) = call(&mut c, Request::HostsList(api::Empty {})).await? else {
            return Err("unexpected reply".into());
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&l).map_err(|e| e.to_string())?
            );
        } else if l.hosts.is_empty() && l.profiles_error.is_none() {
            println!("no hosts");
        } else {
            l.hosts.iter().for_each(print_status);
        }
        if let Some(e) = &l.profiles_error {
            eprintln!("dream: hosts.yaml: {e}");
        }
        Ok(())
    })
}

fn disconnect(name: &str) -> Result<(), String> {
    rt()?.block_on(async {
        let mut c = connect().await?;
        match call(
            &mut c,
            Request::HostsDisconnect(api::HostRef { name: name.into() }),
        )
        .await?
        {
            Response::HostsDisconnect(s) => {
                print_status(&s);
                Ok(())
            }
            other => Err(format!("unexpected reply {other:?}")),
        }
    })
}

/// Encode the parameters with the method's own type, forward them, and
/// print the remote result as JSON.
fn forward(name: &str, method: &str, params: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(params).map_err(|e| format!("parameters: {e}"))?;
    let request = Request::from_json(method, value).map_err(|e| e.to_string())?;
    rt()?.block_on(async {
        let mut c = connect().await?;
        let Response::HostsCall(r) = call(
            &mut c,
            Request::HostsCall(api::HostCall {
                name: name.into(),
                method: method.into(),
                payload: request.encode(),
            }),
        )
        .await?
        else {
            return Err("unexpected reply".into());
        };
        let response = Response::decode(method, &r.payload).map_err(|e| e.to_string())?;
        println!(
            "{}",
            serde_json::to_string_pretty(&response.to_json()).map_err(|e| e.to_string())?
        );
        Ok(())
    })
}

fn print_status(s: &api::HostStatus) {
    let state = match api::LinkState::try_from(s.state) {
        Ok(api::LinkState::Unspecified) => "not connected",
        Ok(state) => state.name(),
        Err(_) => "unknown",
    };
    print!("{} ({}) {state}", s.name, s.destination);
    if let (Some(host), Some(pid)) = (&s.remote_host, s.remote_pid) {
        print!(": dreamerd pid {pid} on {host}");
    }
    if let Some(v) = &s.remote {
        print!(
            " {} protocol {}.{} build {}",
            v.daemon_version,
            v.protocol_major,
            v.protocol_minor,
            v.build_family.get(..12).unwrap_or(&v.build_family)
        );
    }
    println!();
    if !s.tags.is_empty() {
        println!("  tags {}", s.tags.join(", "));
    }
    if let Some(detail) = &s.detail {
        println!("  {detail}");
    }
    let failure = api::HostFailure::try_from(s.failure).unwrap_or(api::HostFailure::Unspecified);
    if failure != api::HostFailure::Unspecified && s.state == api::LinkState::Failed as i32 {
        match s.next_retry_unix_ms {
            Some(at) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                println!(
                    "  attempt {} failed ({}); retrying in {} s",
                    s.attempts,
                    failure.name(),
                    at.saturating_sub(now).div_ceil(1000)
                );
            }
            None if s.auto_reconnect && failure != api::HostFailure::Transient => println!(
                "  {} failure: not retried automatically; `dream hosts reconnect {}` after fixing it",
                failure.name(),
                s.name
            ),
            None => {}
        }
    }
}
