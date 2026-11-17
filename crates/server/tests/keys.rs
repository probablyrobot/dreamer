//! The user's tmux keys through the daemon: `tmux.key_options` reads the
//! prefix and mode keys (never changing them), `tmux.prefix_key` types a
//! key after the prefix so tmux's own bindings run, and
//! `clipboard.list_tmux_buffers` lists the paste buffers for a chooser.

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
    fn new(tag: &str) -> Setup {
        let root = PathBuf::from(format!("/tmp/drm-keys-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        let tmux = TmuxServer::start(80, 20, "/bin/sh").expect("tmux");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_CONFIG_DIR", root.join("config"))
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
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("daemon never answered: {e}"),
            }
        }
    }

    fn tmux(&self, args: &[&str]) -> String {
        self.tmux.run_checked(args).expect("tmux")
    }

    fn windows(&self) -> Vec<String> {
        self.tmux(&["list-windows", "-a", "-F", "#{session_name}:#{window_name}"])
            .lines()
            .map(str::to_owned)
            .collect()
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

async fn key_options(c: &Client) -> api::KeyOptions {
    match c
        .call(Request::TmuxKeyOptions(api::KeyOptionsParams::default()))
        .await
        .expect("key options")
    {
        Response::TmuxKeyOptions(o) => o,
        other => panic!("unexpected {other:?}"),
    }
}

async fn prefix_key(c: &Client, pane: u32, key: &str) -> Result<api::PrefixKeyResult, String> {
    match c
        .call(Request::TmuxPrefixKey(api::PrefixKey {
            endpoint: None,
            pane,
            key: key.into(),
        }))
        .await
    {
        Ok(Response::TmuxPrefixKey(r)) => Ok(r),
        Ok(other) => panic!("unexpected {other:?}"),
        Err(e) => Err(format!("{e:?}")),
    }
}

#[tokio::test]
async fn a_custom_prefix_and_mode_keys_are_read_and_prefix_keys_reach_tmux() {
    let s = Setup::new("opts");
    s.tmux(&["set-option", "-g", "prefix", "C-a"]);
    s.tmux(&["set-option", "-gw", "mode-keys", "vi"]);
    let c = s.client().await;
    let o = key_options(&c).await;
    assert_eq!(
        (o.prefix.as_str(), o.prefix2.as_str(), o.mode_keys.as_str()),
        ("C-a", "None", "vi")
    );
    assert!(!o.mode_style.is_empty());

    // Changes are followed: the next read sees them.
    s.tmux(&["set-option", "-gw", "mode-keys", "emacs"]);
    assert_eq!(key_options(&c).await.mode_keys, "emacs");

    let pane: u32 = s
        .tmux(&["display-message", "-p", "-t", "main:", "#{pane_id}"])
        .trim()
        .trim_start_matches('%')
        .parse()
        .expect("pane");
    // An unbound-by-Dreamer prefix key goes to tmux: prefix c makes a
    // window, as tmux's default binding says.
    let r = prefix_key(&c, pane, "c").await.expect("prefix c");
    assert_eq!(r.mode_cancelled, None);
    assert_eq!(s.windows().len(), 2, "{:?}", s.windows());
    // A binding the user added works the same way.
    s.tmux(&["bind-key", "-T", "prefix", "x", "new-window", "-n", "mine"]);
    prefix_key(&c, pane, "x").await.expect("prefix x");
    assert!(s.windows().contains(&"main:mine".to_owned()));
    // The options were only read.
    assert_eq!(s.tmux(&["show-options", "-gv", "prefix"]).trim(), "C-a");
    // One key name, never a command line.
    assert!(prefix_key(&c, pane, "c ; kill-server").await.is_err());
    assert!(prefix_key(&c, 9999, "c").await.is_err());
}

#[tokio::test]
async fn paste_buffers_are_listed_for_a_chooser() {
    let s = Setup::new("bufs");
    s.tmux(&["set-buffer", "older"]);
    s.tmux(&["set-buffer", "newer text"]);
    let c = s.client().await;
    let list = match c
        .call(Request::ClipboardListTmuxBuffers(
            api::TmuxBufferListParams::default(),
        ))
        .await
        .expect("list")
    {
        Response::ClipboardListTmuxBuffers(l) => l,
        other => panic!("unexpected {other:?}"),
    };
    let got: Vec<(&str, u64)> = list
        .buffers
        .iter()
        .map(|b| (b.sample.as_str(), b.size))
        .collect();
    assert_eq!(got, vec![("newer text", 10), ("older", 5)]);
    assert!(list.buffers.iter().all(|b| !b.name.is_empty()));
}
