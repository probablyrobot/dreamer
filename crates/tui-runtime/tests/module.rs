//! Luau modules, forms, and hot reload in a real tmux pane (spec 13.3): a
//! module computes a column and handles a command through the daemon, a
//! changed module is applied live, a broken update keeps the last-good
//! version running, and a form submits typed values.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const APP: &str = env!("CARGO_BIN_EXE_dreamer-termapp");

fn target_dir() -> PathBuf {
    PathBuf::from(APP)
        .parent()
        .expect("target dir")
        .to_path_buf()
}

const MANIFEST: &str = r#"
schema_version = 1
id = "test.module"
version = "0.1.0"
title = "Labelled"
module = "app.luau"

[data.panes]
method = "tmux.topology"
path = "panes"
events = ["tmux.topology"]

[view]
type = "table"
source = "panes"
row_key = "id"
columns = ["id", "label"]

[commands.note]
key = "n"
handler = "note"
"#;

const MODULE: &str = r#"
function transform(rows)
  local out = {}
  for _, r in ipairs(rows) do
    r.label = "v1-" .. r.id
    table.insert(out, r)
  end
  return out
end

function note(arg)
  local v = dreamer.daemon.version()
  return "noted " .. arg.selected.id .. " on protocol " .. v.protocol_major
end
"#;

const FORM: &str = r#"
schema_version = 1
id = "test.form"
version = "0.1.0"
title = "Ask"
module = "form.luau"

[data.none]
method = "daemon.version"

[view]
type = "form"
source = "none"
submit = "send"
fields = [
  { name = "name", label = "Name", kind = "text", required = true },
  { name = "count", label = "Count", kind = "number" },
  { name = "loud", label = "Loud", kind = "toggle" },
]

[commands.send]
key = "F2"
handler = "send"
"#;

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: std::cell::RefCell<Option<std::process::Child>>,
}

impl Setup {
    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "DREAMER_RUNTIME_DIR",
                self.root.join("rt").display().to_string(),
            ),
            (
                "DREAMER_STATE_DIR",
                self.root.join("st").display().to_string(),
            ),
            ("DREAMER_FORBID_DEFAULT_TMUX", "1".into()),
        ]
    }

    fn start_daemon(&self) {
        let daemon = target_dir().join("dreamerd");
        assert!(
            daemon.exists(),
            "{} is missing; build dreamer-server",
            daemon.display()
        );
        let child = Command::new(daemon)
            .args([
                "--background",
                "--tmux-socket",
                self.tmux.socket(),
                "--tmux",
            ])
            .arg(tmux_program())
            .envs(self.env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        *self.daemon.borrow_mut() = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.root.join("rt/dreamerd.sock").exists() {
            assert!(Instant::now() < deadline, "daemon never started");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Stop the daemon (SIGTERM) and reap it; `false` if it would not stop.
    fn stop_daemon(&self) -> bool {
        let Some(mut child) = self.daemon.borrow_mut().take() else {
            return true;
        };
        let _ = Command::new("kill").arg(child.id().to_string()).status();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                return true;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn screen(&self, pane: &str) -> String {
        self.tmux
            .run_checked(&["capture-pane", "-p", "-t", pane])
            .unwrap_or_default()
    }

    fn wait(&self, pane: &str, what: &str, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let s = self.screen(pane);
            if ok(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "never {what}:\n{s}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = self.stop_daemon();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn launch(s: &Setup, manifest: &std::path::Path) -> String {
    let env: Vec<String> = s
        .env()
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let mut args = vec!["new-window", "-d", "-P", "-F", "#{pane_id}", "env"];
    args.extend(env.iter().map(String::as_str));
    let manifest_s = manifest.display().to_string();
    args.extend([APP, manifest_s.as_str()]);
    s.tmux
        .run_checked(&args)
        .expect("new-window")
        .trim()
        .to_owned()
}

fn setup(name: &str) -> Setup {
    let root = PathBuf::from(format!("/tmp/drm-tmod-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let s = Setup {
        root,
        tmux: TmuxServer::start(120, 30, "/bin/sh").expect("tmux"),
        daemon: std::cell::RefCell::new(None),
    };
    s.start_daemon();
    s
}

/// Rewrite a file so its modification time surely changes.
fn rewrite(path: &std::path::Path, text: &str) {
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(path, text).expect("write");
}

#[test]
fn a_module_computes_handles_and_reloads_keeping_the_last_good_version() {
    let s = setup("module");
    let manifest = s.root.join("app.toml");
    let module = s.root.join("app.luau");
    std::fs::write(&manifest, MANIFEST).expect("manifest");
    std::fs::write(&module, MODULE).expect("module");
    let pane = launch(&s, &manifest);
    s.wait(&pane, "showed the computed column", |t| {
        t.contains("v1-0") && t.contains("[live]")
    });

    // A handler calls the daemon through the module, under the app's grant.
    s.tmux
        .run_checked(&["send-keys", "-t", &pane, "n"])
        .expect("key");
    s.wait(&pane, "ran the handler", |t| {
        t.contains("note: noted 0 on protocol 1")
    });

    // A changed module is applied live.
    rewrite(&module, &MODULE.replace("\"v1-\"", "\"v2-\""));
    s.wait(&pane, "reloaded", |t| {
        t.contains("v2-0") && t.contains("(reloaded 1)")
    });

    // A module that does not compile, then a manifest that does not
    // validate: version 2 keeps running, and says why.
    rewrite(&module, "function transform(rows) return = end");
    let shown = s.wait(&pane, "refused the bad module", |t| {
        t.contains("reload failed; version 2 keeps running")
    });
    assert!(shown.contains("v2-0"), "{shown}");
    rewrite(&module, &MODULE.replace("\"v1-\"", "\"v2-\""));
    s.wait(&pane, "took the fixed module", |t| {
        t.contains("(reloaded 2)")
    });
    rewrite(&manifest, &MANIFEST.replace("row_key", "rowkey"));
    let shown = s.wait(&pane, "refused the bad manifest", |t| {
        t.contains("reload failed; version 3 keeps running")
    });
    assert!(shown.contains("v2-0"), "{shown}");
}

#[test]
fn a_form_submits_typed_values() {
    let s = setup("form");
    let manifest = s.root.join("form.toml");
    std::fs::write(&manifest, FORM).expect("manifest");
    std::fs::write(
        s.root.join("form.luau"),
        r#"
function send(arg)
  local f = arg.form
  return "got " .. f.name .. " " .. tostring(f.count + 1) .. " " .. tostring(f.loud)
end
"#,
    )
    .expect("module");
    let pane = launch(&s, &manifest);
    s.wait(&pane, "showed the form", |t| {
        t.contains("Name*:") && t.contains("[live]")
    });
    // Enter with the required field empty.
    s.tmux
        .run_checked(&["send-keys", "-t", &pane, "Enter"])
        .expect("key");
    s.wait(&pane, "said what is missing", |t| {
        t.contains("Name is required")
    });
    // Typed, tabbed, toggled, submitted; q is just a letter in a form.
    for keys in [
        &["-l", "abq"][..],
        &["Tab"],
        &["-l", "41"],
        &["Tab"],
        &["Space"],
        &["Enter"],
    ] {
        let mut args = vec!["send-keys", "-t", pane.as_str()];
        args.extend(keys.iter().copied());
        s.tmux.run_checked(&args).expect("keys");
    }
    s.wait(&pane, "submitted", |t| t.contains("send: got abq 42 true"));
}
