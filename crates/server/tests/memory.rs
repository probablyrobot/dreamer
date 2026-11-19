//! Memory through a real daemon on private directories: the user scope in
//! the config directory, the host and session scopes in the state
//! directory, a project's in the project; hand edits seen at once; a bad
//! file reported; agent grants that may remember but not forget below
//! `own`; switching to the `kosmonaut` provider, with import from and
//! export to the files.

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
        let tmux = TmuxServer::start(80, 24, "/bin/sh").expect("tmux");
        let daemon = Command::new(EXE)
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
