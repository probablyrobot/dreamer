//! `dreamerd`: Dreamer's per-user headless daemon (spec 11.1, 11.3).
//!
//! ```text
//! dreamerd [--background] [--tmux PATH] [--tmux-socket NAME | --tmux-socket-path PATH]
//! dreamerd --version | --self-check
//! ```
//!
//! One instance per user and runtime directory: a startup lock makes
//! racing starts produce one daemon. `--background` detaches into its own
//! session (no controlling terminal) and logs to a file; the foreground
//! logs to standard error. Nothing is ever written to standard output after
//! startup, and nothing but protocol frames to the socket.

mod agents;
mod approvals;
mod apps;
mod captures;
mod collect_presets;
mod collectors;
mod config_watch;
mod conn;
mod decorations;
mod drawer;
mod events;
mod extensions;
mod filters;
mod forward;
mod grants;
mod hosts;
mod install;
mod jobs;
mod jsonrpc;
mod leases;
mod log;
mod memories;
mod messages;
mod query;
mod scripts;
mod service;
mod sessions;
mod taps;
mod tiers;
mod tools;
mod user_path;
mod web;

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use tmux_backend::Socket;
use tokio::net::UnixListener;
use wire::Identity;
use workspace_host::{Actions, Hooks, Ledger};

use crate::service::{EndpointConfig, State};

/// Set on the copy a group-leading `--background` start hands over to.
const DETACHED_COPY: &str = "DREAMER_DETACHED_COPY";

/// Exit status when another instance holds the startup lock.
const ALREADY_RUNNING: u8 = 3;

fn version_line() -> String {
    format!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"))
}

#[derive(Debug)]
struct Args {
    background: bool,
    endpoint: EndpointConfig,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut background = false;
    let mut program = tmux_backend::server::tmux_program();
    let mut socket = Socket::Default;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--background" => background = true,
            "--tmux" => program = PathBuf::from(value("--tmux")?),
            "--tmux-socket" => socket = Socket::Name(value("--tmux-socket")?),
            "--tmux-socket-path" => {
                socket = Socket::Path(PathBuf::from(value("--tmux-socket-path")?))
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(Args {
        background,
        endpoint: EndpointConfig {
            name: "default".into(),
            program,
            socket,
        },
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("{}", version_line());
            return ExitCode::SUCCESS;
        }
        Some("--self-check") => {
            // Exercises the domain and wire types in the shipped binary.
            let epoch = domain::ConnectionEpoch::default().next();
            println!(
                "domain ok (first epoch {}), build family {}",
                epoch.0,
                terminal_core::family::build_family()
            );
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    adopt_user_path();
    let args = match parse(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("dreamerd: {e}");
            return ExitCode::from(2);
        }
    };
    if args.background
        && let Err(e) = platform::detach_session()
    {
        // A shell job leads its own process group, and a group leader
        // cannot start a session. A copy of this process is not a leader.
        if std::env::var_os(DETACHED_COPY).is_some() {
            eprintln!("dreamerd: {e}");
            return ExitCode::FAILURE;
        }
        return match std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .args(std::env::args_os().skip(1))
                .env(DETACHED_COPY, "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
        }) {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("dreamerd: cannot detach: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let background = args.background;
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            log::warn(format!("stopped: {e}"));
            // Whoever started a background daemon reads why it could not
            // start from its standard error (`client::ensure_running`).
            if background {
                eprintln!("dreamerd: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Restart this process with the user's `PATH` when it was started with
/// only the system directories (as from Finder or the Dock), so tmux and
/// every program a provider, job, or pane runs by name are found. The
/// process is replaced (same pid, same open files) before it has started
/// anything, and every child inherits the new `PATH` from then on.
/// `--tmux` and `DREAMER_TMUX` still name tmux exactly.
fn adopt_user_path() {
    use std::os::unix::process::CommandExt as _;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(path) = user_path::effective(
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("SHELL").as_deref(),
        home.as_deref(),
        user_path::SHELL_TIMEOUT,
    ) else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut argv = std::env::args_os();
    let mut cmd = std::process::Command::new(exe);
    if let Some(arg0) = argv.next() {
        cmd.arg0(arg0);
    }
    let e = cmd.args(argv).env("PATH", path).exec();
    // Only reached when the restart failed: run on with the PATH we have.
    eprintln!("dreamerd: cannot restart with the user's PATH: {e}");
}

/// The startup lock, held for the daemon's lifetime.
fn take_lock() -> Result<Option<File>, String> {
    let path = platform::lock_path();
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    match f.try_lock() {
        Ok(()) => {
            f.set_len(0).map_err(|e| e.to_string())?;
            f.rewind().map_err(|e| e.to_string())?;
            writeln!(f, "{}", std::process::id()).map_err(|e| e.to_string())?;
            Ok(Some(f))
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut holder = String::new();
            let _ = f.read_to_string(&mut holder);
            log::info(format!("already running (pid {}); exiting", holder.trim()));
            Ok(None)
        }
        Err(std::fs::TryLockError::Error(e)) => Err(format!("{}: {e}", path.display())),
    }
}

fn run(args: Args) -> Result<ExitCode, String> {
    let runtime = platform::runtime_dir();
    let state_dir = platform::state_dir();
    platform::private_dir(&runtime)?;
    platform::private_dir(&state_dir)?;
    let log_path = platform::log_path();
    if args.background {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("{}: {e}", log_path.display()))?;
        platform::owner_only(&log_path)?;
        log::init(Box::new(file));
    } else {
        log::init(Box::new(std::io::stderr()));
    }

    platform::check_socket_path(&platform::socket_path())?;
    platform::check_socket_path(&jsonrpc::socket_path())?;
    let Some(lock) = take_lock()? else {
        return Ok(ExitCode::from(ALREADY_RUNNING));
    };
    let socket = platform::socket_path();
    let rpc_socket = jsonrpc::socket_path();
    // Holding the lock means any socket file left here is stale.
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&rpc_socket);

    let (ledger, settled) = Ledger::open(&platform::ledger_path()).map_err(|e| e.to_string())?;
    for r in &settled {
        log::warn(format!(
            "action {} settled as {} after a restart: {}",
            r.id,
            workspace_host::ledger::state_name(r.state),
            r.reason.as_deref().unwrap_or_default()
        ));
    }
    let hooks = Hooks {
        abort_after_dispatch: std::env::var_os("DREAMER_TEST_ABORT_AFTER_DISPATCH").is_some(),
        ..Hooks::default()
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let result = rt.block_on(async {
        let listener =
            UnixListener::bind(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
        platform::owner_only(&socket)?;
        let rpc = UnixListener::bind(&rpc_socket)
            .map_err(|e| format!("{}: {e}", rpc_socket.display()))?;
        platform::owner_only(&rpc_socket)?;
        let (started, started_unix_ms) = service::started_now();
        let events = Arc::new(events::Events::default());
        let settings = workspace_config::load_settings();
        let state = Arc::new(State {
            identity: Identity::current("daemon"),
            instance: format!("{:x}-{:x}", started_unix_ms, std::process::id()),
            epoch: started_unix_ms,
            started,
            started_unix_ms,
            socket: socket.clone(),
            log: if args.background {
                log_path.clone()
            } else {
                PathBuf::from("stderr")
            },
            endpoint_config: args.endpoint.clone(),
            endpoint: tokio::sync::Mutex::new(None),
            actions: tokio::sync::Mutex::new(Actions::new(ledger, hooks)),
            clients: 0.into(),
            shutting_down: false.into(),
            shutdown: tokio::sync::Notify::new(),
            mirrors: tokio::sync::RwLock::new(None),
            input: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            topology: tokio::sync::watch::channel(None).0,
            sizing: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            pastes: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            leases: crate::leases::Leases::default(),
            approvals: crate::approvals::Approvals::default(),
            terminal: tokio::sync::Mutex::new(match &settings {
                Ok(s) => {
                    let d = config_watch::defaults(s);
                    log::info(format!("terminal theme {}", d.scheme.name));
                    d
                }
                Err(e) => {
                    log::warn(format!("{e}; terminal defaults used"));
                    mirror::TerminalDefaults::default()
                }
            }),
            tiers: tokio::sync::Mutex::new(config_watch::tiers(
                &settings.as_ref().map(|s| s.tiers).unwrap_or_default(),
            )),
            hosts: hosts::Hosts::new(events.clone()),
            events,
            broker: Arc::default(),
            extensions: extensions::Extensions::default(),
            scripts: scripts::Scripts::default(),
            filters: filters::Filters::default(),
            collectors: collectors::Collectors::new(state_dir.join("collectors")),
            memory: memories::Memory::new(
                &state_dir,
                &settings
                    .as_ref()
                    .map(|s| s.memory.clone())
                    .unwrap_or_default(),
            ),
            decorations: decorations::Decorations::default(),
            agents: agents::Agents::default(),
            sessions: Arc::new(sessions::Sessions::load()),
            apps: apps::Apps::default(),
            captures: captures::Captures::default(),
            // Epochs must never repeat across daemon restarts: a client
            // resuming with an old epoch must not replay a new mirror.
            epochs: (started_unix_ms * 1000).into(),
        });
        log::info(format!(
            "started {} (instance {}, socket {}, background {})",
            version_line(),
            state.instance,
            socket.display(),
            args.background
        ));
        log::info(format!(
            "PATH {}",
            std::env::var_os("PATH")
                .unwrap_or_default()
                .to_string_lossy()
        ));
        extensions::start_all(&state).await;
        collectors::start_all(&state).await;
        tokio::spawn(jobs::watch(state.clone()));
        tokio::spawn(config_watch::run(state.clone()));
        tokio::spawn(tiers::run(state.clone()));
        let r = serve(listener, rpc, state.clone()).await;
        state.filters.remove_all();
        state.collectors.save_all();
        state.scripts.stop_all().await;
        state.extensions.stop_all().await;
        state.sessions.stop_all().await;
        r
    });
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&rpc_socket);
    drop(lock);
    log::info("stopped");
    result.map(|()| ExitCode::SUCCESS)
}

async fn serve(listener: UnixListener, rpc: UnixListener, state: Arc<State>) -> Result<(), String> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    // Registering SIGHUP replaces its default (terminate): a hangup of
    // whatever started the daemon does not stop it.
    let mut hup = signal(SignalKind::hangup()).map_err(|e| e.to_string())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let state = state.clone();
                    tokio::spawn(async move { conn::connection(stream, state).await });
                }
                Err(e) => log::warn(format!("accept: {e}")),
            },
            accepted = rpc.accept() => match accepted {
                Ok((stream, _)) => {
                    let state = state.clone();
                    tokio::spawn(async move { jsonrpc::connection(stream, state).await });
                }
                Err(e) => log::warn(format!("accept (json-rpc): {e}")),
            },
            () = state.shutdown.notified() => break,
            _ = term.recv() => { log::info("SIGTERM"); break }
            _ = int.recv() => { log::info("SIGINT"); break }
            _ = hup.recv() => log::info("SIGHUP ignored"),
        }
    }
    Ok(())
}

pub(crate) fn host_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOST").ok())
        .unwrap_or_else(|| "localhost".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_line_names_the_binary() {
        assert!(super::version_line().starts_with("dreamerd "));
    }

    #[test]
    fn arguments_select_the_tmux_endpoint() {
        let a = super::parse(&["--background".into(), "--tmux-socket".into(), "x".into()])
            .expect("parse");
        assert!(a.background);
        assert!(matches!(a.endpoint.socket, tmux_backend::Socket::Name(ref n) if n == "x"));
        assert!(super::parse(&["--bogus".into()]).is_err());
        assert!(super::parse(&["--tmux".into()]).is_err());
    }
}
