//! Memory through a real daemon on private directories: the user scope in
//! the config directory, the host and session scopes in the state
//! directory, a project's in the project; hand edits seen at once; a bad
//! file reported; agent grants that may remember but not forget below
//! `own`; switching to the `kosmonaut` provider, with import from and
//! export to the files; and its `openai` embedder against a stand-in for
//! the API on the loopback (no request leaves the machine).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::Client;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    _tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        Setup::start(None, &[])
    }

    /// A daemon started with `settings` as its `dreamer.yaml` and `env`
    /// added to its environment.
    fn start(settings: Option<&str>, env: &[(&str, &str)]) -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-mem-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("config");
        std::fs::create_dir_all(root.join("work/app/.git")).expect("project");
        std::fs::create_dir_all(root.join("work/app/src")).expect("src");
        if let Some(text) = settings {
            std::fs::write(root.join("config/dreamer.yaml"), text).expect("settings");
        }
        let tmux = TmuxServer::start(80, 24, "/bin/sh").expect("tmux");
        let daemon = Command::new(EXE)
            .envs(env.iter().copied())
            .args(["--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_CONFIG_DIR", root.join("config"))
            .env("DREAMER_DATA_DIR", root.join("data"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        Setup {
            root,
            _tmux: tmux,
            daemon,
        }
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

    fn context(&self) -> Option<api::MemoryContext> {
        Some(api::MemoryContext {
            cwd: Some(self.root.join("work/app/src").display().to_string()),
            session: Some("sess-1".into()),
        })
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn remember(c: &Client, s: &Setup, text: &str, scope: api::MemoryScope) -> api::MemoryItem {
    match c
        .call(Request::MemoryRemember(api::MemoryRemember {
            text: text.into(),
            kind: api::MemoryKind::Project as i32,
            scope: scope as i32,
            context: s.context(),
            ..api::MemoryRemember::default()
        }))
        .await
    {
        Ok(Response::MemoryRemember(m)) => m,
        other => panic!("remember: {other:?}"),
    }
}

async fn recall(c: &Client, s: &Setup, query: &str, scope: api::MemoryScope) -> api::MemoryItems {
    match c
        .call(Request::MemoryRecall(api::MemoryRecall {
            query: query.into(),
            scope: scope as i32,
            context: s.context(),
            ..api::MemoryRecall::default()
        }))
        .await
    {
        Ok(Response::MemoryRecall(r)) => r,
        other => panic!("recall: {other:?}"),
    }
}

fn code(e: client::ClientError) -> i32 {
    match e {
        client::ClientError::Rpc(e) => e.code,
        other => panic!("not an RPC error: {other}"),
    }
}

fn path(m: &api::MemoryItem) -> PathBuf {
    PathBuf::from(m.path.clone().expect("a file"))
}

fn under(p: &Path, dir: &Path) -> bool {
    p.parent() == Some(dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memories_are_files_in_their_scopes_and_hand_edits_count() {
    use api::MemoryScope as S;
    let s = Setup::new();
    let c = s.client().await;
    let user = remember(&c, &s, "Prefers short commit subjects", S::User).await;
    let host = remember(
        &c,
        &s,
        "This host builds with eight commit workers",
        S::Host,
    )
    .await;
    let project = remember(&c, &s, "The app squashes every commit", S::Project).await;
    let session = remember(&c, &s, "This session renames the commit hook", S::Session).await;
    assert!(under(&path(&user), &s.root.join("config/memory")));
    assert!(under(&path(&host), &s.root.join("st/memory/host")));
    assert!(under(
        &path(&project),
        &s.root.join("work/app/.dreamer/memory")
    ));
    assert!(under(
        &path(&session),
        &s.root.join("st/memory/sessions/sess-1")
    ));
    for m in [&user, &host, &project, &session] {
        let index = path(m).with_file_name("MEMORY.md");
        let index = std::fs::read_to_string(&index).expect("index");
        assert_eq!(index.lines().count(), 1, "{index}");
        assert!(
            index.contains(&format!(
                "]({})",
                path(m).file_name().unwrap_or_default().to_string_lossy()
            )),
            "{index}"
        );
    }

    let all = recall(&c, &s, "commit", S::Unspecified).await;
    assert_eq!(all.items.len(), 4);
    assert_eq!(all.provider, "files");
    for (scope, want) in [
        (S::User, &user),
        (S::Host, &host),
        (S::Project, &project),
        (S::Session, &session),
    ] {
        let r = recall(&c, &s, "commit", scope).await;
        assert_eq!(
            r.items.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            [want.id.as_str()]
        );
    }

    // A person edits the project's file; the next call reads their words.
    let p = path(&project);
    let text = std::fs::read_to_string(&p).expect("file");
    std::fs::write(
        &p,
        text.replace("squashes every commit", "rebases, never merges"),
    )
    .expect("edit");
    let got = match c
        .call(Request::MemoryGet(api::MemoryRef {
            id: project.id.clone(),
            context: s.context(),
        }))
        .await
    {
        Ok(Response::MemoryGet(m)) => m,
        other => panic!("get: {other:?}"),
    };
    assert_eq!(got.text, "The app rebases, never merges");
    assert_eq!(recall(&c, &s, "rebases", S::Project).await.items.len(), 1);

    // A file that does not read is reported, not fatal.
    std::fs::write(
        s.root.join("config/memory/broken.md"),
        "no frontmatter here\n",
    )
    .expect("broken");
    let r = recall(&c, &s, "commit", S::User).await;
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.problems.len(), 1);
    assert!(r.problems[0].path.ends_with("broken.md"));
    let e = c
        .call(Request::MemoryGet(api::MemoryRef {
            id: "user/broken".into(),
            context: s.context(),
        }))
        .await
        .expect_err("broken");
    assert_eq!(code(e), api::code::PRECONDITION);

    // Forget takes the file and its index line.
    match c
        .call(Request::MemoryForget(api::MemoryRef {
            id: host.id.clone(),
            context: s.context(),
        }))
        .await
    {
        Ok(Response::MemoryForget(m)) => assert_eq!(m.id, host.id),
        other => panic!("forget: {other:?}"),
    }
    assert!(!path(&host).exists());
    assert_eq!(
        std::fs::read_to_string(s.root.join("st/memory/host/MEMORY.md")).expect("index"),
        ""
    );
    let e = c
        .call(Request::MemoryGet(api::MemoryRef {
            id: host.id.clone(),
            context: s.context(),
        }))
        .await
        .expect_err("gone");
    assert_eq!(code(e), api::code::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_observing_agent_remembers_but_does_not_forget() {
    let s = Setup::new();
    let owner = s.client().await;
    let mine = remember(&owner, &s, "Written by the user", api::MemoryScope::User).await;
    for (level, may_forget) in [(api::ActLevel::Observe, false), (api::ActLevel::Own, true)] {
        let c = s.client().await;
        c.call(Request::GrantsNarrow(api::Grant {
            methods: api::agent_methods(level as i32),
            all_panes: true,
            level: level as i32,
            ..api::Grant::default()
        }))
        .await
        .expect("narrow");
        let new = remember(
            &c,
            &s,
            &format!("Learned at {level:?}"),
            api::MemoryScope::Project,
        )
        .await;
        assert!(
            !recall(&c, &s, "learned", api::MemoryScope::Project)
                .await
                .items
                .is_empty()
        );
        let r = c
            .call(Request::MemoryForget(api::MemoryRef {
                id: new.id.clone(),
                context: s.context(),
            }))
            .await;
        assert_eq!(r.is_ok(), may_forget, "{level:?}: {r:?}");
        if !may_forget {
            assert_eq!(code(r.expect_err("refused")), api::code::FORBIDDEN);
            let u = c
                .call(Request::MemoryUpdate(api::MemoryUpdate {
                    id: mine.id.clone(),
                    text: Some("changed".into()),
                    context: s.context(),
                    ..api::MemoryUpdate::default()
                }))
                .await;
            assert_eq!(code(u.expect_err("refused")), api::code::FORBIDDEN);
        }
    }
    let kept = std::fs::read_to_string(path(&mine)).expect("the user's memory");
    assert!(kept.ends_with("Written by the user\n"), "{kept}");
}

async fn transfer(c: &Client, s: &Setup, import: bool) -> Result<api::MemoryTransferred, i32> {
    let p = api::MemoryTransfer {
        scope: api::MemoryScope::Unspecified as i32,
        context: s.context(),
    };
    let r = if import {
        c.call(Request::MemoryImport(p)).await
    } else {
        c.call(Request::MemoryExport(p)).await
    };
    match r {
        Ok(Response::MemoryImport(t) | Response::MemoryExport(t)) => Ok(t),
        Ok(other) => panic!("transfer: {other:?}"),
        Err(e) => Err(code(e)),
    }
}

/// `memory.provider: kosmonaut` applies live. With the feature, the files
/// go in with import and come back out with export; without it, every
/// memory call says the build lacks it. Import and export need a provider
/// other than files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kosmonaut_takes_over_live_and_the_files_move_in_and_out() {
    use api::MemoryScope as S;
    let s = Setup::new();
    let c = s.client().await;
    let first = remember(&c, &s, "Deploys go through staging first", S::Project).await;
    assert_eq!(
        transfer(&c, &s, true).await.expect_err("files in force"),
        api::code::INVALID_PARAMS
    );
    std::fs::write(
        s.root.join("config/dreamer.yaml"),
        "memory:\n  provider: kosmonaut\n",
    )
    .expect("settings");
    // Applied within the watcher's period.
    let deadline = Instant::now() + Duration::from_secs(10);
    let listed = loop {
        let r = c
            .call(Request::MemoryList(api::MemoryListQuery {
                context: s.context(),
                ..api::MemoryListQuery::default()
            }))
            .await;
        match r {
            Ok(Response::MemoryList(l)) if l.provider == "files" => {}
            other => break other,
        }
        assert!(Instant::now() < deadline, "the provider never changed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    if cfg!(not(feature = "kosmonaut")) {
        let e = listed.expect_err("no kosmonaut in this build");
        assert_eq!(code(e), api::code::INVALID_PARAMS);
        return;
    }
    match listed {
        Ok(Response::MemoryList(l)) => {
            assert_eq!(l.provider, "kosmonaut");
            assert!(l.items.is_empty(), "{l:?}");
        }
        other => panic!("list: {other:?}"),
    }
    let moved = transfer(&c, &s, true).await.expect("import");
    assert_eq!(
        (moved.from.as_str(), moved.to.as_str()),
        ("files", "kosmonaut")
    );
    assert_eq!(moved.ids, std::slice::from_ref(&first.id));
    assert!(s.root.join("data/memory.kosmonaut").is_dir());
    let found = recall(&c, &s, "staging", S::Unspecified).await;
    assert_eq!(found.provider, "kosmonaut");
    assert_eq!(found.items.len(), 1);
    assert_eq!(found.items[0].id, first.id);
    assert_eq!(found.items[0].path, None);
    // A memory kept in Kosmonaut only reaches the files on export.
    let second = remember(&c, &s, "Releases ship on Tuesdays", S::Project).await;
    let dir = path(&first).parent().expect("dir").to_path_buf();
    let file = dir.join("releases-ship-on-tuesdays.md");
    assert!(!file.exists());
    let moved = transfer(&c, &s, false).await.expect("export");
    assert_eq!(
        (moved.from.as_str(), moved.to.as_str()),
        ("kosmonaut", "files")
    );
    assert_eq!(moved.ids.len(), 2);
    assert!(moved.ids.contains(&second.id));
    let text = std::fs::read_to_string(&file).expect("exported");
    assert!(text.ends_with("Releases ship on Tuesdays\n"), "{text}");
    let index = std::fs::read_to_string(dir.join("MEMORY.md")).expect("index");
    assert!(index.contains("(releases-ship-on-tuesdays.md)"), "{index}");
}

/// A stand-in for OpenAI's embeddings API on the loopback. Each text gets
/// a vector by its words: rolling out (deploy, staging, release, ship,
/// roll, builds) on the first axis, formatting (indent, tabs, spaces) on
/// the second. With `refuse` set it answers 401. Every request's
/// `Authorization` header and body are kept.
#[cfg(feature = "kosmonaut")]
struct StandIn {
    port: u16,
    refuse: std::sync::Arc<std::sync::atomic::AtomicBool>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
}

#[cfg(feature = "kosmonaut")]
fn stand_in() -> StandIn {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::atomic::Ordering;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let api = StandIn {
        port,
        refuse: Default::default(),
        seen: Default::default(),
    };
    let (refuse, seen) = (api.refuse.clone(), api.seen.clone());
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut reader = BufReader::new(conn.try_clone().expect("clone"));
            let (mut auth, mut length, mut start) = (String::new(), 0, String::new());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if start.is_empty() {
                    start = line.to_owned();
                } else if let Some((n, v)) = line.split_once(':') {
                    match n.to_ascii_lowercase().as_str() {
                        "authorization" => auth = v.trim().to_owned(),
                        "content-length" => length = v.trim().parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let (status, answer) = if refuse.load(Ordering::SeqCst) {
                (
                    401,
                    serde_json::json!({"error": {"message": "Incorrect API key"}}),
                )
            } else if !start.starts_with("POST /v1/embeddings ") {
                (404, serde_json::json!({"error": {"message": start}}))
            } else {
                let has = |t: &str, words: &[&str]| -> f32 {
                    let t = t.to_lowercase();
                    if words.iter().any(|w| t.contains(w)) {
                        1.0
                    } else {
                        0.0
                    }
                };
                let roll = ["deploy", "staging", "release", "ship", "roll", "builds"];
                let format = ["indent", "tabs", "spaces"];
                let data: Vec<serde_json::Value> = body["input"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .map(|(i, t)| {
                        let t = t.as_str().unwrap_or_default();
                        serde_json::json!({
                            "index": i,
                            "embedding": [has(t, &roll), has(t, &format), 0.01],
                        })
                    })
                    .collect();
                (200, serde_json::json!({"object": "list", "data": data}))
            };
            seen.lock().expect("seen").push((auth, body));
            let text = answer.to_string();
            let _ = write!(
                conn,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    api
}

#[cfg(feature = "kosmonaut")]
fn openai_settings(key_env: &str, port: u16) -> String {
    format!(
        "memory:\n  provider: kosmonaut\n  kosmonaut:\n    embedder:\n      kind: openai\n      \
         model: test-embedding\n      key_env: {key_env}\n      url: http://127.0.0.1:{port}/v1\n      \
         dimension: 3\n      timeout: 5s\n"
    )
}

/// Recall `query` until a problem says `why` (a settings change applies
/// within the watcher's period), and return that answer.
#[cfg(feature = "kosmonaut")]
async fn recall_until_problem(c: &Client, s: &Setup, query: &str, why: &str) -> api::MemoryItems {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = recall(c, s, query, api::MemoryScope::Unspecified).await;
        if r.problems.iter().any(|p| p.message.contains(why)) {
            return r;
        }
        assert!(
            Instant::now() < deadline,
            "never a problem with {why}: {r:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The `openai` embedder against a stand-in for the API: memories get
/// vectors, and recall finds one by meaning that shares no word with the
/// query. A refused key, an unreachable API, or a key missing from the
/// environment each leave a clear problem, and recall still answers by
/// words.
#[cfg(feature = "kosmonaut")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_openai_embedder_gives_memories_vectors_and_recall_uses_them() {
    use api::MemoryScope as S;
    use std::sync::atomic::Ordering;
    let api = stand_in();
    let key_env = "DREAMER_TEST_EMBED_KEY";
    let s = Setup::start(
        Some(&openai_settings(key_env, api.port)),
        &[(key_env, "sk-stand-in")],
    );
    let c = s.client().await;
    let deploys = remember(&c, &s, "Deploys go through staging first", S::Project).await;
    let tabs = remember(&c, &s, "Indent with tabs, not spaces", S::Project).await;
    {
        let seen = api.seen.lock().expect("seen");
        let sent: Vec<String> = seen
            .iter()
            .flat_map(|(_, b)| b["input"].as_array().cloned().unwrap_or_default())
            .filter_map(|t| t.as_str().map(str::to_owned))
            .collect();
        assert!(
            sent.iter()
                .any(|t| t.contains("Deploys go through staging")),
            "{sent:?}"
        );
        assert!(
            sent.iter().any(|t| t.contains("Indent with tabs")),
            "{sent:?}"
        );
        for (auth, body) in seen.iter() {
            assert_eq!(auth, "Bearer sk-stand-in");
            assert_eq!(body["model"], "test-embedding");
            assert_eq!(body["dimensions"], 3);
        }
    }
    // No word in common with either memory: only the vectors find it.
    let found = recall(&c, &s, "how do we roll out builds", S::Unspecified).await;
    assert_eq!(found.provider, "kosmonaut");
    assert!(found.problems.is_empty(), "{found:?}");
    let ids: Vec<&str> = found.items.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec![deploys.id.as_str()], "{found:?}");

    // The API refuses the key: the words still answer.
    api.refuse.store(true, Ordering::SeqCst);
    let r = recall_until_problem(&c, &s, "tabs", "refused the key in DREAMER_TEST_EMBED_KEY").await;
    assert_eq!(
        r.items.first().map(|m| m.id.as_str()),
        Some(tabs.id.as_str())
    );
    assert!(
        r.problems
            .iter()
            .all(|p| !p.message.contains("sk-stand-in")),
        "{r:?}"
    );
    api.refuse.store(false, Ordering::SeqCst);

    // The API cannot be reached.
    let dead = std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .expect("port")
        .port();
    std::fs::write(
        s.root.join("config/dreamer.yaml"),
        openai_settings(key_env, dead),
    )
    .expect("settings");
    let r = recall_until_problem(&c, &s, "staging", "cannot reach the API").await;
    assert_eq!(
        r.items.first().map(|m| m.id.as_str()),
        Some(deploys.id.as_str())
    );

    // The key's variable is not in dreamerd's environment: nothing is sent.
    std::fs::write(
        s.root.join("config/dreamer.yaml"),
        openai_settings("DREAMER_TEST_NO_SUCH_KEY", api.port),
    )
    .expect("settings");
    let before = api.seen.lock().expect("seen").len();
    let r = recall_until_problem(&c, &s, "staging", "DREAMER_TEST_NO_SUCH_KEY").await;
    assert!(
        r.problems.iter().any(|p| p.message.contains("not set")),
        "{r:?}"
    );
    assert_eq!(
        r.items.first().map(|m| m.id.as_str()),
        Some(deploys.id.as_str())
    );
    assert_eq!(api.seen.lock().expect("seen").len(), before);
}
