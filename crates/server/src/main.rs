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

mod log;
mod service;

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use prost::Message as _;
use tmux_backend::Socket;
use tokio::net::{UnixListener, UnixStream};
use wire::io::{Reader, server_handshake, write_frame};
use wire::{Identity, Kind, welcome};
use workspace_host::{Actions, Hooks, Ledger};

use crate::service::{DaemonService, EndpointConfig, State};

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
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            log::warn(format!("stopped: {e}"));
            ExitCode::FAILURE
        }
    }
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

    let Some(lock) = take_lock()? else {
        return Ok(ExitCode::from(ALREADY_RUNNING));
    };
    let socket = platform::socket_path();
    // Holding the lock means any socket file left here is stale.
    let _ = std::fs::remove_file(&socket);

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
        let (started, started_unix_ms) = service::started_now();
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
        });
        log::info(format!(
            "started {} (instance {}, socket {}, background {})",
            version_line(),
            state.instance,
            socket.display(),
            args.background
        ));
        serve(listener, state).await
    });
    let _ = std::fs::remove_file(&socket);
    drop(lock);
    log::info("stopped");
    result.map(|()| ExitCode::SUCCESS)
}

async fn serve(listener: UnixListener, state: Arc<State>) -> Result<(), String> {
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
                    tokio::spawn(async move { connection(stream, state).await });
                }
                Err(e) => log::warn(format!("accept: {e}")),
            },
            () = state.shutdown.notified() => break,
            _ = term.recv() => { log::info("SIGTERM"); break }
            _ = int.recv() => { log::info("SIGINT"); break }
            _ = hup.recv() => log::info("SIGHUP ignored"),
        }
    }
    Ok(())
}

async fn connection(stream: UnixStream, state: Arc<State>) {
    let (r, mut w) = stream.into_split();
    let me = &state.identity;
    let mut reader = Reader::new(r, me.limits);
    let accepted = tokio::time::timeout(
        Duration::from_secs(5),
        server_handshake(&mut reader, &mut w, me, |n| {
            welcome(me, n, &state.instance, state.epoch, &host_name(), "default")
        }),
    )
    .await;
    let (hello, negotiated) = match accepted {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            log::warn(format!("refused a client: {e}"));
            return;
        }
        Err(_) => {
            log::warn("a client did not complete the handshake in time");
            return;
        }
    };
    state.clients.fetch_add(1, Ordering::Relaxed);
    let service = DaemonService(state.clone());
    loop {
        let (kind, payload) = match reader.next_frame().await {
            Ok(f) => f,
            Err(wire::WireError::Closed) => break,
            Err(e) => {
                log::warn(format!("{} client (pid {}): {e}", hello.role, hello.pid));
                break;
            }
        };
        let reply = match kind {
            Kind::Request => {
                let response = match wire::Request::decode(payload.as_slice()) {
                    Ok(req) => answer(&service, req).await,
                    Err(e) => {
                        log::warn(format!("undecodable request: {e}"));
                        break;
                    }
                };
                (Kind::Response, response.encode_to_vec())
            }
            Kind::Ping => (Kind::Pong, Vec::new()),
            Kind::Goodbye => break,
            other => {
                log::warn(format!("unexpected {other:?} frame from a client"));
                break;
            }
        };
        if let Err(e) = write_frame(&mut w, reply.0, &reply.1, negotiated.limits.max_frame).await {
            log::warn(format!("writing to a client: {e}"));
            break;
        }
        if state.shutting_down.load(Ordering::SeqCst) {
            state.shutdown.notify_one();
            break;
        }
    }
    state.clients.fetch_sub(1, Ordering::Relaxed);
}

async fn answer(service: &DaemonService, req: wire::Request) -> wire::Response {
    let fail = |e: api::RpcError| wire::Response {
        id: req.id,
        payload: Vec::new(),
        error_code: Some(e.code),
        error_message: e.message,
    };
    let request = match api::Request::decode(&req.method, &req.payload) {
        Ok(r) => r,
        Err(e) => return fail(e.into()),
    };
    match api::dispatch(service, request).await {
        Ok(response) => wire::Response {
            id: req.id,
            payload: response.encode(),
            error_code: None,
            error_message: String::new(),
        },
        Err(e) => fail(e),
    }
}

fn host_name() -> String {
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
