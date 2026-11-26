//! The process registry through a real daemon: an agent CLI
//! running in a tmux pane is found as a run of its provider, in that pane,
//! by evidence, with its identity and working directory.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::Client;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-proc-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        Setup { root, tmux, daemon }
    }

    async fn client(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(&self.root.join("rt/dreamerd.sock"), "test").await {
                Ok(c) => return c,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    fn tmux(&self, args: &[&str]) {
        self.tmux.run_checked(args).expect("tmux command");
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.root.join("rt/dreamerd.lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| platform::process_alive(*p))
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn list(c: &Client, q: api::ProcessQuery) -> api::ProcessList {
    match c.call(Request::ProcessesList(q)).await.expect("list") {
        Response::ProcessesList(l) => l,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_agent_cli_in_a_pane_is_found_as_a_run_with_its_evidence() {
    let s = Setup::new();
    let c = s.client().await;
    // A program whose name is the provider's CLI: a link named `claude`.
    let bin = s.root.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    std::os::unix::fs::symlink("/bin/sleep", bin.join("claude")).expect("link");
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    let pane = t.panes[0].id;
    let work = s.root.join("project");
    std::fs::create_dir_all(&work).expect("project");
    s.tmux(&[
        "send-keys",
        "-t",
        &format!("%{pane}"),
        &format!(
            "cd {} && {} 300",
            work.display(),
            bin.join("claude").display()
        ),
        "Enter",
    ]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let run = loop {
        let l = list(
            &c,
            api::ProcessQuery {
                runs_only: true,
                provider: Some("claude".into()),
                ..api::ProcessQuery::default()
            },
        )
        .await;
        if let Some(r) = l
            .processes
            .into_iter()
            .find(|p| p.args.contains(&bin.join("claude").display().to_string()))
        {
            break r;
        }
        assert!(Instant::now() < deadline, "the run never showed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(run.pane, Some(pane));
    assert_eq!(run.pane_evidence.as_deref(), Some("ancestry"));
    assert_eq!(run.confidence.as_deref(), Some("high"));
    assert!(
        run.evidence
            .as_deref()
            .unwrap_or_default()
            .contains("provider's CLI")
    );
    assert!(run.started_unix_ms > 0);
    let cwd = std::fs::canonicalize(&work).expect("canonical");
    assert_eq!(
        run.cwd
            .as_deref()
            .map(|c| std::fs::canonicalize(c).unwrap_or_default()),
        Some(cwd),
        "{run:?}"
    );
    // Its pane's shell is in the pane but no run.
    let in_pane = list(
        &c,
        api::ProcessQuery {
            panes: vec![pane],
            ..api::ProcessQuery::default()
        },
    )
    .await;
    assert!(
        in_pane
            .processes
            .iter()
            .any(|p| p.provider.is_none() && p.pane == Some(pane))
    );
    assert!(in_pane.processes.iter().all(|p| p.pane == Some(pane)));
    // The same pid listed twice has the same identity.
    let again = list(
        &c,
        api::ProcessQuery {
            runs_only: true,
            ..api::ProcessQuery::default()
        },
    )
    .await;
    let same = again
        .processes
        .iter()
        .find(|p| p.pid == run.pid)
        .expect("still there");
    assert!(same.started_unix_ms.abs_diff(run.started_unix_ms) <= 1000);
}

/// Detaches a program from its parent the way daemons do: fork, start a
/// new session (no controlling terminal), fork again, then exec.
const DETACH: &str = "import os,sys; os.fork() and os._exit(0); os.setsid(); \
os.fork() and os._exit(0); os.execv(sys.argv[1], sys.argv[1:])";

/// Kills the processes a test left running when it ends, even by panic.
struct Strays(Vec<u32>);

impl Drop for Strays {
    fn drop(&mut self) {
        for pid in &self.0 {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

async fn wait_for_run(c: &Client, args: &str) -> api::ProcessRecord {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let l = list(
            c,
            api::ProcessQuery {
                runs_only: true,
                provider: Some("claude".into()),
                ..api::ProcessQuery::default()
            },
        )
        .await;
        if let Some(r) = l.processes.into_iter().find(|p| p.args.ends_with(args)) {
            return r;
        }
        assert!(Instant::now() < deadline, "the run `{args}` never showed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn detached_descendants_are_attributed_only_by_what_remains() {
    let s = Setup::new();
    let c = s.client().await;
    let bin = s.root.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let claude = bin.join("claude");
    std::os::unix::fs::symlink("/bin/sleep", &claude).expect("link");
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    let pane = t.panes[0].id;
    let mut strays = Strays(Vec::new());
    // Forked twice from the pane's shell: reparented, but still on the
    // pane's terminal. Then the same, detached into a session of its own,
    // which leaves it no terminal either.
    s.tmux(&[
        "send-keys",
        "-t",
        &format!("%{pane}"),
        &format!(
            "( {claude} 121 & ) ; python3 -c '{DETACH}' {claude} 122",
            claude = claude.display()
        ),
        "Enter",
    ]);
    let kept = wait_for_run(&c, "claude 121").await;
    strays.0.push(kept.pid);
    let detached = wait_for_run(&c, "claude 122").await;
    strays.0.push(detached.pid);

    // Its ancestry is gone, its terminal says where it runs.
    assert_eq!(kept.pane, Some(pane), "{kept:?}");
    assert_eq!(kept.pane_evidence.as_deref(), Some("terminal"));
    assert_eq!(kept.confidence.as_deref(), Some("high"));
    assert!(kept.tty.is_some());

    // Nothing ties the detached one to the pane: it is in no pane, and a
    // run outside any pane is never more than medium confidence.
    assert_eq!(detached.pane, None, "{detached:?}");
    assert_eq!(detached.pane_evidence, None);
    assert_eq!(detached.tty, None);
    assert_eq!(detached.confidence.as_deref(), Some("medium"));
    assert_eq!(detached.helper_of, None);
}

/// A process that is not dumpable keeps `/proc/PID/cwd` from its own user,
/// the way another user's process does: the kernel makes root its owner.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_run_whose_metadata_cannot_be_read_is_listed_without_it() {
    let s = Setup::new();
    let c = s.client().await;
    let python = std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("python3"))
                .find(|p| p.is_file())
        })
        .expect("python3 on PATH");
    let bin = s.root.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let claude = bin.join("claude");
    std::os::unix::fs::symlink(&python, &claude).expect("link");
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    let pane = t.panes[0].id;
    s.tmux(&[
        "send-keys",
        "-t",
        &format!("%{pane}"),
        &format!(
            "cd {} && {} -c 'import ctypes,time; ctypes.CDLL(None).prctl(4,0,0,0,0); time.sleep(120)'",
            s.root.display(),
            claude.display()
        ),
        "Enter",
    ]);
    let run = wait_for_run(&c, "time.sleep(120)").await;
    let _strays = Strays(vec![run.pid]);
    // Root (as in the ARM64 container stage) may read any process's
    // working directory, so there the case cannot be made: say so.
    let denied = match std::fs::read_link(format!("/proc/{}/cwd", run.pid)) {
        Ok(cwd) => {
            println!("not run: this user can read a non-dumpable process's cwd ({cwd:?})");
            return;
        }
        Err(e) => e,
    };
    assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
    // Still a run, in its pane, with the evidence there is; the working
    // directory the system would not give is absent, not guessed.
    assert_eq!(run.pane, Some(pane), "{run:?}");
    assert_eq!(run.pane_evidence.as_deref(), Some("ancestry"));
    assert_eq!(run.confidence.as_deref(), Some("high"));
    assert_eq!(run.cwd, None, "{run:?}");
}
