//! Memory through a real daemon on private directories. With
//! `provider: files`: the user scope in the config directory, the host and
//! session scopes in the state directory, a project's in the project; hand
//! edits seen at once; a bad file reported; agent grants that may remember
//! but not forget below `own`. `sqlite`, the default: file memories copied
//! in once when it takes over, at start or live, and never again; import
//! from and export to the files; the `openai` embedder against a stand-in for the API on
//! the loopback (no request leaves the machine); `kosmonaut` refused in a
//! build without it; and, with the feature, Kosmonaut on a server of the
//! test's own, stopped mid-way.

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
    tmux: TmuxServer,
    daemon: Child,
}

const FILES: &str = "memory:\n  provider: files\n";

impl Setup {
    /// A daemon with `provider: files`.
    fn files() -> Setup {
        Setup::start(Some(FILES), &[])
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
        let daemon = daemon(&root, &tmux, env);
        Setup { root, tmux, daemon }
    }

    /// Stop the daemon and start another on the same directories, with
    /// `settings` as its `dreamer.yaml` (none: no settings file).
    fn restart(&mut self, settings: Option<&str>) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let file = self.root.join("config/dreamer.yaml");
        match settings {
            Some(text) => std::fs::write(&file, text).expect("settings"),
            None => {
                let _ = std::fs::remove_file(&file);
            }
        }
        self.daemon = daemon(&self.root, &self.tmux, &[]);
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

fn daemon(root: &Path, tmux: &TmuxServer, env: &[(&str, &str)]) -> Child {
    Command::new(EXE)
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
        .expect("dreamerd")
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
    let s = Setup::files();
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
    let s = Setup::files();
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

/// `memory.provider` until the list says `provider`: a settings change
/// applies within the watcher's period.
async fn list_until(c: &Client, s: &Setup, provider: &str) -> api::MemoryItems {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = c
            .call(Request::MemoryList(api::MemoryListQuery {
                context: s.context(),
                ..api::MemoryListQuery::default()
            }))
            .await;
        match r {
            Ok(Response::MemoryList(l)) if l.provider == provider => return l,
            Ok(Response::MemoryList(_)) => {}
            other => panic!("list: {other:?}"),
        }
        assert!(Instant::now() < deadline, "the provider never changed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `memory.provider: sqlite` applies live, and the files are in it from the
/// first call; import copies them again on request, and export brings
/// them back out. Import and export need a provider other than files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_takes_over_live_and_the_files_move_in_and_out() {
    use api::MemoryScope as S;
    let s = Setup::files();
    let c = s.client().await;
    let first = remember(&c, &s, "Deploys go through staging first", S::Project).await;
    assert_eq!(
        transfer(&c, &s, true).await.expect_err("files in force"),
        api::code::INVALID_PARAMS
    );
    std::fs::write(
        s.root.join("config/dreamer.yaml"),
        "memory:\n  provider: sqlite\n",
    )
    .expect("settings");
    let listed = list_until(&c, &s, "sqlite").await;
    assert_eq!(listed.items.len(), 1, "{listed:?}");
    assert_eq!(listed.items[0].id, first.id);
    assert_eq!(listed.items[0].updated_unix_ms, first.updated_unix_ms);
    assert_eq!(listed.items[0].path, None);
    // Opened on the first call, in the data directory.
    assert!(s.root.join("data/memory.sqlite").is_file());
    let moved = transfer(&c, &s, true).await.expect("import");
    assert_eq!(
        (moved.from.as_str(), moved.to.as_str()),
        ("files", "sqlite")
    );
    assert_eq!(moved.ids, std::slice::from_ref(&first.id));
    let found = recall(&c, &s, "staging", S::Unspecified).await;
    assert_eq!(found.provider, "sqlite");
    assert_eq!(found.items.len(), 1);
    assert_eq!(found.items[0].id, first.id);
    assert_eq!(found.items[0].path, None);
    // A memory kept in the database only reaches the files on export.
    let second = remember(&c, &s, "Releases ship on Tuesdays", S::Project).await;
    assert_eq!(second.path, None);
    let dir = path(&first).parent().expect("dir").to_path_buf();
    let file = dir.join("releases-ship-on-tuesdays.md");
    assert!(!file.exists());
    let moved = transfer(&c, &s, false).await.expect("export");
    assert_eq!(
        (moved.from.as_str(), moved.to.as_str()),
        ("sqlite", "files")
    );
    assert_eq!(moved.ids.len(), 2);
    assert!(moved.ids.contains(&second.id));
    let text = std::fs::read_to_string(&file).expect("exported");
    assert!(text.ends_with("Releases ship on Tuesdays\n"), "{text}");
    let index = std::fs::read_to_string(dir.join("MEMORY.md")).expect("index");
    assert!(index.contains("(releases-ship-on-tuesdays.md)"), "{index}");
}

/// Every file under `dir`, relative, with its bytes.
fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                let rel = p.strip_prefix(base).expect("under").to_path_buf();
                out.push((rel, std::fs::read(&p).expect("read")));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

async fn list(c: &Client, s: &Setup) -> api::MemoryItems {
    match c
        .call(Request::MemoryList(api::MemoryListQuery {
            context: s.context(),
            ..api::MemoryListQuery::default()
        }))
        .await
    {
        Ok(Response::MemoryList(l)) => l,
        other => panic!("list: {other:?}"),
    }
}

/// An update to a Dreamer where `sqlite` is the default: the memories a
/// user kept as files in every scope are there after the switch, with the
/// same ids and times; the files are not touched; and a memory the user
/// forgot is not copied in again on the next start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_by_default_copies_in_the_file_memories_once() {
    use api::MemoryScope as S;
    let mut s = Setup::files();
    let c = s.client().await;
    let mut kept = Vec::new();
    for (text, scope) in [
        ("Prefers short commit subjects", S::User),
        ("This host builds with eight workers", S::Host),
        ("The app squashes every commit", S::Project),
        ("This session renames the commit hook", S::Session),
    ] {
        kept.push(remember(&c, &s, text, scope).await);
    }
    std::fs::write(
        s.root.join("config/memory/broken.md"),
        "no frontmatter here\n",
    )
    .expect("broken");
    drop(c);
    let dirs = ["config/memory", "st/memory", "work/app/.dreamer"];
    let files: Vec<_> = dirs.iter().map(|d| tree(&s.root.join(d))).collect();

    // No memory settings: sqlite.
    s.restart(None);
    let c = s.client().await;
    let first = list(&c, &s).await;
    assert_eq!(first.provider, "sqlite");
    assert!(s.root.join("data/memory.sqlite").is_file());
    let mut got: Vec<_> = first
        .items
        .iter()
        .map(|m| {
            (
                m.id.clone(),
                m.text.clone(),
                m.created_unix_ms,
                m.updated_unix_ms,
            )
        })
        .collect();
    let mut want: Vec<_> = kept
        .iter()
        .map(|m| {
            (
                m.id.clone(),
                m.text.clone(),
                m.created_unix_ms,
                m.updated_unix_ms,
            )
        })
        .collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
    assert!(first.items.iter().all(|m| m.path.is_none()), "{first:?}");
    // The file that does not read is left, and said once.
    assert_eq!(first.problems.len(), 1, "{first:?}");
    assert!(first.problems[0].path.ends_with("broken.md"));
    assert!(list(&c, &s).await.problems.is_empty());
    let found = recall(&c, &s, "squashes", S::Unspecified).await;
    assert_eq!(found.items.first().map(|m| &m.id), Some(&kept[2].id));

    // Forgotten in the database: gone for good, though its file is there.
    match c
        .call(Request::MemoryForget(api::MemoryRef {
            id: kept[1].id.clone(),
            context: s.context(),
        }))
        .await
    {
        Ok(Response::MemoryForget(m)) => assert_eq!(m.id, kept[1].id),
        other => panic!("forget: {other:?}"),
    }
    drop(c);
    s.restart(None);
    let c = s.client().await;
    let again = list(&c, &s).await;
    assert_eq!(again.items.len(), 3, "{again:?}");
    assert!(!again.items.iter().any(|m| m.id == kept[1].id), "{again:?}");
    assert!(again.problems.is_empty(), "{again:?}");
    let now: Vec<_> = dirs.iter().map(|d| tree(&s.root.join(d))).collect();
    assert_eq!(now, files, "the files changed");

    // Back to files: everything is as it was there.
    drop(c);
    s.restart(Some(FILES));
    let c = s.client().await;
    let back = list(&c, &s).await;
    assert_eq!(back.provider, "files");
    assert_eq!(back.items.len(), 4);
}

/// `memory.provider: kosmonaut` in a build without the feature: every
/// memory call says so.
#[cfg(not(feature = "kosmonaut"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kosmonaut_in_a_build_without_it_is_refused_with_why() {
    let s = Setup::start(
        Some(
            "memory:\n  provider: kosmonaut\n  kosmonaut:\n    url: grpc://127.0.0.1:1\n    tls: false\n",
        ),
        &[],
    );
    let c = s.client().await;
    let e = c
        .call(Request::MemoryList(api::MemoryListQuery {
            context: s.context(),
            ..api::MemoryListQuery::default()
        }))
        .await
        .expect_err("no kosmonaut in this build");
    let client::ClientError::Rpc(e) = e else {
        panic!("not an RPC error: {e}");
    };
    assert_eq!(e.code, api::code::INVALID_PARAMS);
    assert!(
        e.message
            .contains("built without the kosmonaut feature: set memory.provider: sqlite"),
        "{e:?}"
    );
}

/// A stand-in for OpenAI's embeddings API on the loopback. Each text gets
/// a vector by its words: rolling out (deploy, staging, release, ship,
/// roll, builds) on the first axis, formatting (indent, tabs, spaces) on
/// the second. With `refuse` set it answers 401. Every request's
/// `Authorization` header and body are kept.
struct StandIn {
    port: u16,
    refuse: std::sync::Arc<std::sync::atomic::AtomicBool>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
}

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

fn openai_settings(key_env: &str, port: u16) -> String {
    format!(
        "memory:\n  provider: sqlite\n  embedder:\n    kind: openai\n    \
         model: test-embedding\n    key_env: {key_env}\n    url: http://127.0.0.1:{port}/v1\n    \
         dimension: 3\n    timeout: 5s\n"
    )
}

/// Recall `query` until a problem says `why` (a settings change applies
/// within the watcher's period), and return that answer.
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
    assert_eq!(found.provider, "sqlite");
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

#[cfg(feature = "kosmonaut")]
#[path = "../../memory/tests/support/kosmonaut_server.rs"]
mod kosmonaut_server;

/// `mode: server`: memory kept on a Kosmonaut server of the test's own
/// (`DREAMER_KOSMONAUT_SERVER`). When the server stops, memory calls fail
/// with `UNAVAILABLE` and say "memory unavailable", and an agent session
/// still starts and holds a turn, without the memory index.
#[cfg(feature = "kosmonaut")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_kosmonaut_server_leaves_sessions_working_without_memory() {
    use api::MemoryScope as S;
    use api::agent_session_event::Kind as K;
    let Some(mut server) = kosmonaut_server::TestServer::start("daemon", false) else {
        return;
    };
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("../providers/testdata/fake-acp");
    let settings = format!(
        "memory:\n  provider: kosmonaut\n  kosmonaut:\n    mode: server\n    url: {}\n    \
         tls: false\nagents:\n  default: helper\n  helper:\n    protocol: acp\n    \
         command: [/bin/sh, {}]\n",
        server.url,
        fake.display()
    );
    let s = Setup::start(Some(&settings), &[("DREAMER_DREAM", "/bin/echo")]);
    let c = s.client().await;
    let kept = remember(&c, &s, "Deploys go through staging first", S::Project).await;
    let found = recall(&c, &s, "staging", S::Unspecified).await;
    assert_eq!(found.provider, "kosmonaut");
    assert_eq!(found.items.first().map(|m| &m.id), Some(&kept.id));
    assert!(!s.root.join("data/memory.sqlite").exists());

    server.stop();
    let e = c
        .call(Request::MemoryRecall(api::MemoryRecall {
            query: "staging".into(),
            context: s.context(),
            ..api::MemoryRecall::default()
        }))
        .await
        .expect_err("server stopped");
    let client::ClientError::Rpc(e) = e else {
        panic!("not an RPC error: {e}");
    };
    assert_eq!(e.code, api::code::UNAVAILABLE, "{e:?}");
    assert!(e.message.starts_with("memory unavailable:"), "{e:?}");

    // A session in the project: its first prompt would carry the index,
    // which cannot be read; the turn goes on without it.
    let created = c
        .call(Request::AgentSessionsCreate(api::AgentSessionCreate {
            cwd: Some(s.root.join("work/app").display().to_string()),
            ..api::AgentSessionCreate::default()
        }))
        .await
        .expect("create");
    let Response::AgentSessionsCreate(session) = created else {
        panic!("unexpected reply {created:?}");
    };
    c.call(Request::AgentSessionsSend(api::AgentSessionSend {
        session: session.id.clone(),
        text: "hello".into(),
    }))
    .await
    .expect("send");
    let deadline = Instant::now() + Duration::from_secs(20);
    let ended = loop {
        let r = c
            .call(Request::AgentSessionsEvents(api::AgentSessionEventsQuery {
                session: session.id.clone(),
                after: Some(0),
                ..api::AgentSessionEventsQuery::default()
            }))
            .await
            .expect("events");
        let Response::AgentSessionsEvents(r) = r else {
            panic!("unexpected reply");
        };
        let end = r.events.iter().find_map(|e| match &e.kind {
            Some(K::TurnEnd(t)) => Some(t.stop_reason),
            _ => None,
        });
        if let Some(reason) = end {
            break reason;
        }
        assert!(Instant::now() < deadline, "no turn end: {:?}", r.events);
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(ended, api::AgentStopReason::EndTurn as i32);

    // Back with the server.
    server.restart();
    let found = recall(&c, &s, "staging", S::Unspecified).await;
    assert_eq!(found.items.first().map(|m| &m.id), Some(&kept.id));
}
