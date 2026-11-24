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
