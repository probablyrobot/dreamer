//! `dreamer-bench remote`: key to echo in a pane on a host connected
//! through the daemon, the path the desktop takes to a remote pane (spec
//! 16.5): the replica sends input with `hosts.call`, the coordinator
//! relays it over the host's link to the host's daemon, and the echo comes
//! back through `hosts.subscribe` into the replica.
//!
//! Reported separately, each with its p50, p95, and jitter:
//!
//! - **the link:** a `hosts.call` round trip of `daemon.version`, which
//!   the host's daemon answers at once, beside the same call to the local
//!   daemon;
//! - **the host's processing:** the host daemon's own stage timings
//!   (input, tmux and the pty, decode, route, parse, publish, transfer)
//!   over a run with its diagnostics on;
//! - **end to end:** key to echo through the host, beside key to echo in a
//!   local pane of the same run.
//!
//! Jitter is the mean difference between consecutive samples, in the order
//! they were taken.
//!
//! The host is reached either through a stand-in for `ssh` that runs the
//! remote command in a shell on this machine, as sshd would, with a second
//! tmux server and private daemon directories (the default: no SSH and no
//! network, so it shows what the bridge and relay cost alone), or through
//! a real SSH route given with `--ssh` and `--destination`, such as the
//! one `cargo xtask bench remote --container` prepares.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::Client;
use replica::Via;
use serde_json::{Value, json};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

use crate::{
    Rig, echo_times, first_cursor, hardware, key_to_echo_in_order, ms, percentile, print_table,
    revision, wait_live, write_json,
};

/// The host's name in the coordinator.
const HOST: &str = "bench";

/// How the coordinator reaches the host.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    /// A shell on this machine stands in for `ssh`.
    StandIn,
    /// A real `ssh` (or a wrapper around it) and a destination it knows.
    Ssh {
        program: PathBuf,
        destination: String,
        dream: Option<String>,
        tmux: Option<String>,
        tmux_socket: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    samples: usize,
    route: Route,
    /// What the link is, for the record.
    link: Option<String>,
    json: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut samples = 200usize;
    let mut ssh: Option<PathBuf> = None;
    let mut destination: Option<String> = None;
    let mut dream: Option<String> = None;
    let mut tmux: Option<String> = None;
    let mut tmux_socket: Option<String> = None;
    let mut link: Option<String> = None;
    let mut json_out: Option<PathBuf> = None;
    let mut it = args.iter();
    let value = |it: &mut std::slice::Iter<'_, String>, what: &str| {
        it.next()
            .cloned()
            .ok_or_else(|| format!("{what} needs a value"))
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--samples" => {
                samples = value(&mut it, "--samples")?
                    .parse()
                    .ok()
                    .filter(|n| *n >= 1)
                    .ok_or("--samples needs a number of at least 1")?;
            }
            "--ssh" => ssh = Some(PathBuf::from(value(&mut it, "--ssh")?)),
            "--destination" => destination = Some(value(&mut it, "--destination")?),
            "--dream" => dream = Some(value(&mut it, "--dream")?),
            "--host-tmux" => tmux = Some(value(&mut it, "--host-tmux")?),
            "--host-tmux-socket" => tmux_socket = Some(value(&mut it, "--host-tmux-socket")?),
            "--link" => link = Some(value(&mut it, "--link")?),
            "--json" => json_out = Some(PathBuf::from(value(&mut it, "--json")?)),
            other => return Err(format!("unknown option {other}")),
        }
    }
    let route = match (ssh, destination) {
        (Some(program), Some(destination)) => Route::Ssh {
            program,
            destination,
            dream,
            tmux,
            tmux_socket,
        },
        (None, None) if dream.is_none() && tmux.is_none() && tmux_socket.is_none() => {
            Route::StandIn
        }
        (None, None) => {
            return Err(
                "--dream, --host-tmux, and --host-tmux-socket go with --ssh and --destination"
                    .into(),
            );
        }
        _ => return Err("--ssh and --destination go together".into()),
    };
    Ok(Options {
        samples,
        route,
        link,
        json: json_out,
    })
}

/// The mean absolute difference between consecutive samples, in the
/// order taken; zero for fewer than two.
fn jitter(in_order: &[f64]) -> f64 {
    if in_order.len() < 2 {
        return 0.0;
    }
    let sum: f64 = in_order.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
    sum / (in_order.len() - 1) as f64
}

/// Results as `(measure, value, unit)` rows, printed as they come.
#[derive(Default)]
struct Rows(Vec<Value>);

impl Rows {
    fn put(&mut self, measure: &str, value: f64, unit: &str) {
        eprintln!("bench: {measure}: {value:.3} {unit}");
        self.0
            .push(json!({ "measure": measure, "value": value, "unit": unit }));
    }

    /// p50, p95, and jitter of samples in the order taken, in ms.
    fn spread(&mut self, what: &str, in_order: &[f64]) {
        let mut sorted = in_order.to_vec();
        sorted.sort_by(f64::total_cmp);
        self.put(&format!("{what}, p50"), percentile(&sorted, 0.5), "ms");
        self.put(&format!("{what}, p95"), percentile(&sorted, 0.95), "ms");
        self.put(&format!("{what}, jitter"), jitter(in_order), "ms");
    }
}

pub fn remote(args: &[String]) -> Result<(), String> {
    let o = parse(args)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let (host, results) = rt.block_on(run(&o))?;
    let route = match &o.route {
        Route::StandIn => "stand-in: the remote command runs in a shell on this machine, no SSH",
        Route::Ssh { .. } => "ssh",
    };
    let report = json!({
        "workload": "remote",
        "samples": o.samples,
        "route": route,
        "link": o.link,
        "host": host,
        "hardware": hardware(),
        "revision": revision(),
        "results": results,
    });
    println!("route: {route}");
    if let Some(l) = &o.link {
        println!("link: {l}");
    }
    println!("host: {host}");
    print_table(&report);
    if let Some(p) = &o.json {
        write_json(p, &report)?;
    }
    Ok(())
}

/// The stand-in host: its own tmux server and daemon directories, and an
/// `ssh` that runs the remote command in a shell here, as sshd would.
struct StandIn {
    root: PathBuf,
    tmux: TmuxServer,
}

impl StandIn {
    fn start() -> Result<StandIn, String> {
        let root = PathBuf::from(format!("/tmp/drm-bench-far-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).map_err(|e| e.to_string())?;
        std::fs::write(
            root.join("config/dreamer.yaml"),
            "tiers:\n  cold_after: never\n  warm_limit: none\n",
        )
        .map_err(|e| e.to_string())?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let daemon = std::env::var_os("DREAMER_BENCH_DAEMON")
            .map(PathBuf::from)
            .unwrap_or_else(|| exe.with_file_name("dreamerd"));
        let tmux = TmuxServer::start(120, 40, "/bin/sh").map_err(|e| format!("tmux: {e}"))?;
        // `ssh [options] -- DESTINATION COMMAND`: run COMMAND.
        let ssh = format!(
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\n\
             export DREAMER_RUNTIME_DIR='{rt}' DREAMER_STATE_DIR='{st}' DREAMER_CONFIG_DIR='{config}'\n\
             export DREAMER_FORBID_DEFAULT_TMUX=1 DREAMER_DAEMON='{daemon}'\n\
             exec /bin/sh -c \"exec $3\"\n",
            rt = root.join("rt").display(),
            st = root.join("st").display(),
            config = root.join("config").display(),
            daemon = daemon.display(),
        );
        std::fs::write(root.join("ssh"), ssh).map_err(|e| e.to_string())?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join("ssh"), std::fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        Ok(StandIn { root, tmux })
    }

    fn ssh(&self) -> PathBuf {
        self.root.join("ssh")
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        // The bridge started the host's daemon, which outlives the link.
        if let Some(pid) = std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
        {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn call(c: &Client, via: &Via, r: Request) -> Result<Response, String> {
    let method = r.method();
    tokio::time::timeout(Duration::from_secs(30), via.call(c, r))
        .await
        .map_err(|_| format!("{method}: no reply in 30 s"))?
        .map_err(|e| format!("{method}: {e}"))
}

/// Connect the host and wait for its link to be live: the host's status.
async fn connect(c: &Client, route: &Route, stand_in: Option<&StandIn>) -> Result<Value, String> {
    let dream = |exe: &Path| exe.with_file_name("dream").display().to_string();
    let params = match (route, stand_in) {
        (
            Route::Ssh {
                destination,
                dream,
                tmux,
                tmux_socket,
                ..
            },
            _,
        ) => api::HostConnect {
            name: HOST.into(),
            destination: destination.clone(),
            dream: dream.clone(),
            tmux: tmux.clone(),
            tmux_socket: tmux_socket.clone(),
            tmux_socket_path: None,
            auto_reconnect: Some(false),
        },
        (Route::StandIn, Some(s)) => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let d = dream(&exe);
            if !Path::new(&d).is_file() {
                return Err(format!("{d} is missing; run `cargo xtask bench remote`"));
            }
            api::HostConnect {
                name: HOST.into(),
                destination: "stand-in".into(),
                dream: Some(d),
                tmux: Some(tmux_program().display().to_string()),
                tmux_socket: Some(s.tmux.socket().into()),
                tmux_socket_path: None,
                auto_reconnect: Some(false),
            }
        }
        (Route::StandIn, None) => return Err("no stand-in host".into()),
    };
    // A failure shows in the host's status below, with SSH's last lines.
    let _ = call(c, &Via::Local, Request::HostsConnect(params)).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let Response::HostsList(l) =
            call(c, &Via::Local, Request::HostsList(api::Empty {})).await?
        else {
            return Err("hosts.list: unexpected reply".into());
        };
        let h = l
            .hosts
            .into_iter()
            .find(|h| h.name == HOST)
            .ok_or("the host is not listed")?;
        if h.state == api::LinkState::Live as i32 {
            return Ok(json!({
                "host": h.remote_host,
                "version": h.remote.as_ref().map(|v| v.daemon_version.clone()),
                "platform": h.remote.as_ref().map(|v| v.platform.clone()),
            }));
        }
        if h.state == api::LinkState::Failed as i32 || Instant::now() > deadline {
            return Err(format!(
                "the host never came up: {}",
                h.detail.unwrap_or_default()
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `n` round trips of `daemon.version` to the daemon `via` reaches, in ms,
/// in the order taken.
async fn round_trips(c: &Client, via: &Via, n: usize) -> Result<Vec<f64>, String> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        call(c, via, Request::DaemonVersion(api::Empty {})).await?;
        out.push(ms(t.elapsed()));
    }
    Ok(out)
}

/// A new window running `cat` in the host's first session, live in the
/// host's daemon: its pane.
async fn host_cat(c: &Client, via: &Via) -> Result<u32, String> {
    let Response::TmuxTopology(t) = call(
        c,
        via,
        Request::TmuxTopology(api::TopologyParams::default()),
    )
    .await?
    else {
        return Err("tmux.topology: unexpected reply".into());
    };
    let session = t
        .sessions
        .first()
        .ok_or("the host's tmux has no session")?
        .id;
    let Response::ActionsSubmit(r) = call(
        c,
        via,
        Request::ActionsSubmit(api::SubmitAction {
            action: Some(api::Action {
                kind: Some(api::action::Kind::NewWindow(api::NewWindow {
                    session,
                    name: Some("bench-echo".into()),
                    argv: vec!["cat".into()],
                    cwd: None,
                })),
            }),
            ..api::SubmitAction::default()
        }),
    )
    .await?
    else {
        return Err("actions.submit: unexpected reply".into());
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut record = r;
    let pane = loop {
        match api::ActionState::try_from(record.state) {
            Ok(api::ActionState::Completed) => {
                break record.new_pane.ok_or("the new session has no pane")?;
            }
            Ok(
                api::ActionState::Failed
                | api::ActionState::Cancelled
                | api::ActionState::OutcomeUnknown,
            ) => {
                return Err(format!(
                    "starting `cat` on the host failed: {}",
                    record.reason.unwrap_or_default()
                ));
            }
            _ if Instant::now() > deadline => {
                return Err("starting `cat` on the host timed out".into());
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let Response::ActionsGet(r) = call(
            c,
            via,
            Request::ActionsGet(api::ActionQuery {
                id: record.id.clone(),
            }),
        )
        .await?
        else {
            return Err("actions.get: unexpected reply".into());
        };
        record = r;
    };
    loop {
        let Response::TerminalMirrors(l) = call(
            c,
            via,
            Request::TerminalMirrors(api::TopologyParams::default()),
        )
        .await?
        else {
            return Err("terminal.mirrors: unexpected reply".into());
        };
        if l.mirrors
            .iter()
            .any(|m| m.pane == pane && m.state == api::MirrorState::Live as i32)
        {
            return Ok(pane);
        }
        if Instant::now() > deadline {
            return Err("the host never mirrored its `cat` pane".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn set_diagnostics(
    c: &Client,
    via: &Via,
    enabled: bool,
    reset: bool,
) -> Result<api::Diagnostics, String> {
    match call(
        c,
        via,
        Request::DaemonSetDiagnostics(api::DiagnosticsSettings { enabled, reset }),
    )
    .await?
    {
        Response::DaemonSetDiagnostics(d) => Ok(d),
        _ => Err("daemon.set_diagnostics: unexpected reply".into()),
    }
}

/// The host daemon's stages, in path order.
const HOST_STAGES: [perf::Stage; 7] = [
    perf::Stage::Input,
    perf::Stage::Tmux,
    perf::Stage::Decode,
    perf::Stage::Route,
    perf::Stage::Parse,
    perf::Stage::Publish,
    perf::Stage::Transfer,
];

async fn run(o: &Options) -> Result<(Value, Vec<Value>), String> {
    let mut rows = Rows::default();
    let stand_in = match o.route {
        Route::StandIn => Some(StandIn::start()?),
        Route::Ssh { .. } => None,
    };
    let ssh = match (&o.route, &stand_in) {
        (Route::Ssh { program, .. }, _) => program.clone(),
        (_, Some(s)) => s.ssh(),
        (Route::StandIn, None) => return Err("no stand-in host".into()),
    };
    let rig = Rig::start_with(&[("DREAMER_SSH", ssh.as_path())])?;
    let c = rig.client().await?;
    wait_live(&c, 1, Duration::from_secs(15)).await?;

    // The local baseline, on this machine, in the same run.
    let local = key_to_echo_in_order(&rig, &c, o.samples).await?;
    rows.spread("key to echo, local pane", &local);
    rows.spread(
        "local call round trip (daemon.version)",
        &round_trips(&c, &Via::Local, o.samples).await?,
    );

    let started = Instant::now();
    let host = connect(&c, &o.route, stand_in.as_ref()).await?;
    rows.put(
        "connect to the host (SSH, bridge, handshake)",
        started.elapsed().as_secs_f64(),
        "s",
    );
    let via = Via::Host(HOST.into());
    rows.spread(
        "link round trip (hosts.call daemon.version)",
        &round_trips(&c, &via, o.samples).await?,
    );

    let pane = host_cat(&c, &via).await?;
    let feed = replica::feed::Feed::start_via(rig.socket(), via.clone(), Some(pane), None)
        .map_err(|e| e.to_string())?;
    let mut cursor = first_cursor(&feed).await?;
    let echo = echo_times(&feed, &mut cursor, o.samples).await?;
    rows.spread("key to echo through the host", &echo);

    // Again with the host's diagnostics on, and this process's (the
    // replica), for the time split by stage.
    set_diagnostics(&c, &via, true, true).await?;
    perf::reset();
    perf::set_enabled(true);
    let echo = echo_times(&feed, &mut cursor, o.samples).await;
    perf::set_enabled(false);
    let host_diag = set_diagnostics(&c, &via, false, false).await?;
    let echo = echo?;
    rows.spread("key to echo through the host, diagnostics on", &echo);
    let us = |ns: u64| ns as f64 / 1000.0;
    let mut host_p50_ns = 0u64;
    for s in HOST_STAGES {
        if let Some(x) = host_diag.stages.iter().find(|x| x.stage == s.name()) {
            eprintln!("bench: host stage {}: {} samples", s.name(), x.count);
            host_p50_ns += x.p50_ns;
            rows.put(&format!("host stage {}, p50", s.name()), us(x.p50_ns), "µs");
            rows.put(&format!("host stage {}, p95", s.name()), us(x.p95_ns), "µs");
        }
    }
    let here = perf::snapshot();
    for s in [perf::Stage::ReplicaParse, perf::Stage::Frame] {
        if let Some(x) = here.stage(s) {
            rows.put(
                &format!("replica stage {}, p50", s.name()),
                us(x.p50_ns),
                "µs",
            );
            rows.put(
                &format!("replica stage {}, p95", s.name()),
                us(x.p95_ns),
                "µs",
            );
        }
    }
    let host_ms = host_p50_ns as f64 / 1e6;
    rows.put("host processing (sum of its stage p50s)", host_ms, "ms");
    let mut sorted = echo.clone();
    sorted.sort_by(f64::total_cmp);
    rows.put(
        "link, relay, replica: p50 less host processing",
        (percentile(&sorted, 0.5) - host_ms).max(0.0),
        "ms",
    );
    drop(feed);
    let _ = call(
        &c,
        &Via::Local,
        Request::HostsDisconnect(api::HostRef { name: HOST.into() }),
    )
    .await;
    Ok((host, rows.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn options_default_to_the_stand_in_and_take_a_real_route() {
        assert_eq!(
            parse(&[]),
            Ok(Options {
                samples: 200,
                route: Route::StandIn,
                link: None,
                json: None,
            })
        );
        let o = parse(&args(
            "--ssh /x/ssh --destination box --dream /d/dream --host-tmux /t/tmux --host-tmux-socket s --samples 50 --json out.json",
        ))
        .expect("options");
        assert_eq!(o.samples, 50);
        assert_eq!(
            o.route,
            Route::Ssh {
                program: "/x/ssh".into(),
                destination: "box".into(),
                dream: Some("/d/dream".into()),
                tmux: Some("/t/tmux".into()),
                tmux_socket: Some("s".into()),
            }
        );
        assert_eq!(o.json, Some(PathBuf::from("out.json")));
    }

    #[test]
    fn a_route_needs_both_ssh_and_a_destination() {
        assert!(parse(&args("--ssh /x/ssh")).is_err());
        assert!(parse(&args("--destination box")).is_err());
        assert!(parse(&args("--host-tmux /t/tmux")).is_err());
        assert!(parse(&args("--samples 0")).is_err());
        assert!(parse(&args("--samples")).is_err());
    }

    #[test]
    fn jitter_is_the_mean_step_between_consecutive_samples() {
        assert_eq!(jitter(&[]), 0.0);
        assert_eq!(jitter(&[5.0]), 0.0);
        // Steps of 2, 4, and 1: order matters, not only the spread.
        assert_eq!(jitter(&[1.0, 3.0, -1.0, 0.0]), 7.0 / 3.0);
        assert_eq!(jitter(&[1.0, 1.0, 1.0]), 0.0);
    }
}
