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
        ["connect", name, destination, rest @ ..] => connect_host(d, name, destination, rest),
        ["list"] => list(false),
        ["list", "--json"] => list(true),
        ["disconnect", name] => disconnect(name),
        ["call", name, method] => forward(name, method, "{}"),
        ["call", name, method, params] => forward(name, method, params),
        _ => Err(USAGE.into()),
    }
}

pub const USAGE: &str = "usage:
  dream hosts connect NAME DESTINATION [--dream PATH] [--remote-tmux PATH]
                      [--remote-socket NAME | --remote-socket-path PATH]
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
        } else if l.hosts.is_empty() {
            println!("no hosts");
        } else {
            l.hosts.iter().for_each(print_status);
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
        Ok(api::LinkState::Unspecified) => "unknown",
        Ok(state) => state.name(),
        Err(_) => "unknown",
    };
    print!("{} ({}) {state}", s.name, s.destination);
    if let (Some(host), Some(pid)) = (&s.remote_host, s.remote_pid) {
        print!(": dreamerd pid {pid} on {host}");
    }
    if let Some(v) = &s.remote {
        print!(
            " {} protocol {}.{}",
            v.daemon_version, v.protocol_major, v.protocol_minor
        );
    }
    println!();
    if let Some(detail) = &s.detail {
        println!("  {detail}");
    }
}
