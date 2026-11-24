//! What every provider does the same way, run against `files`, `sqlite`,
//! and, with the `kosmonaut` feature, the Kosmonaut provider on a server of
//! the test's own; and moving memories between them.

use std::path::{Path, PathBuf};

use memory::{Change, Context, Error, Files, Kind, Memory, New, Provider, Scope};

struct Dirs {
    root: PathBuf,
}

impl Dirs {
    fn new(name: &str) -> Dirs {
        let root =
            std::env::temp_dir().join(format!("dreamer-provider-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("project/.git")).expect("project");
        std::fs::create_dir_all(root.join("project/src/deep")).expect("deep");
        Dirs { root }
    }
    fn files(&self, under: &str) -> Files {
        let r = self.root.join(under);
        Files::new(
            Some(r.join("config/memory")),
            r.join("state/memory/host"),
            r.join("state/memory/sessions"),
        )
    }
    fn cx(&self) -> Context {
        Context {
            cwd: Some(self.root.join("project/src/deep")),
            session: Some("s-1".into()),
        }
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn new(text: &str, scope: Scope) -> New {
    New {
        text: text.into(),
        kind: Kind::Feedback,
        scope,
        title: None,
        description: None,
        links: vec![],
    }
}

/// Without the provider's own fields (a file's path), for comparing two
/// providers' answers.
fn plain(mut m: Memory) -> Memory {
    m.path = None;
    m
}

/// The contract: ids, scopes, recall order, updates, and forgetting.
fn contract(p: &dyn Provider, d: &Dirs) {
    let cx = d.cx();
    let tabs = p
        .remember(
            &cx,
            New {
                title: Some("Prefers tabs".into()),
                description: Some("Indent with tabs everywhere".into()),
                links: vec!["docs/style.md".into()],
                ..new("The user indents with tabs.", Scope::User)
            },
        )
        .expect("remember");
    assert_eq!(tabs.id, "user/prefers-tabs");
    assert_eq!(tabs.links, ["docs/style.md"]);
    let again = p
        .remember(
            &cx,
            New {
                title: Some("Prefers tabs".into()),
                ..new("again", Scope::User)
            },
        )
        .expect("again");
    assert_eq!(again.id, "user/prefers-tabs-2");
    assert_eq!(again.description, "Prefers tabs");
    assert_eq!(
        plain(p.get(&cx, "user/prefers-tabs").expect("get")).text,
        "The user indents with tabs."
    );
    p.forget(&cx, &again.id).expect("forget again");

    for (text, scope) in [
        ("Rust edition 2024 in every crate", Scope::User),
        ("This mac builds with the arm64 rust target", Scope::Host),
        ("The project pins rust 1.99", Scope::Project),
        ("This session is fixing the rust lints", Scope::Session),
    ] {
        p.remember(&cx, new(text, scope)).expect(text);
    }
    assert_eq!(
        p.recall(&cx, "rust", None, None).expect("all").items.len(),
        4
    );
    for scope in Scope::ALL {
        let got = p.recall(&cx, "rust", Some(scope), None).expect("one");
        assert_eq!(got.items.len(), 1, "{scope:?}");
        assert_eq!(got.items[0].scope, scope);
    }
    let other = Context {
        session: Some("s-2".into()),
        ..cx.clone()
    };
    assert!(
        p.recall(&other, "rust", Some(Scope::Session), None)
            .expect("other")
            .items
            .is_empty()
    );
    let bare = Context::default();
    assert_eq!(
        p.recall(&bare, "rust", None, None)
            .expect("bare")
            .items
            .len(),
        2
    );
    assert!(matches!(
        p.recall(&bare, "rust", Some(Scope::Project), None),
        Err(Error::Invalid(_))
    ));
    // The project is the git root, from any directory below it; another
    // project does not see this one's memories.
    let top = Context {
        cwd: Some(d.root.join("project")),
        session: None,
    };
    assert_eq!(
        p.recall(&top, "pins", Some(Scope::Project), None)
            .expect("top")
            .items
            .len(),
        1
    );
    let elsewhere = Context {
        cwd: Some(d.root.join("elsewhere")),
        session: None,
    };
    assert!(
        p.recall(&elsewhere, "pins", Some(Scope::Project), None)
            .expect("elsewhere")
            .items
            .is_empty()
    );
    let best = p
        .recall(&cx, "rust lints session", None, Some(1))
        .expect("best");
    assert_eq!(best.items.len(), 1);
    assert_eq!(best.items[0].scope, Scope::Session);
    assert!(best.items[0].score.is_some_and(|s| s >= 1000));

    // Update changes what is given and keeps the rest.
    let pins = p
        .recall(&cx, "pins", Some(Scope::Project), None)
        .expect("pins")
        .items
        .remove(0);
    let changed = p
        .update(
            &cx,
            &pins.id,
            Change {
                description: Some("Pinned toolchain".into()),
                kind: Some(Kind::Project),
                ..Change::default()
            },
        )
        .expect("update");
    assert_eq!(changed.id, pins.id);
    assert_eq!(changed.kind, Kind::Project);
    assert_eq!(changed.description, "Pinned toolchain");
    assert_eq!(changed.text, pins.text);
    assert!(changed.updated_unix_ms >= pins.updated_unix_ms);
    assert!(matches!(
        p.update(
            &cx,
            &pins.id,
            Change {
                text: Some("  ".into()),
                ..Change::default()
            }
        ),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        p.list(&cx, Some(Scope::Project), Some(Kind::Project))
            .expect("kind")
            .items
            .len(),
        1
    );

    // Forget removes it; it is not found after.
    let gone = p.forget(&cx, &pins.id).expect("forget");
    assert_eq!(gone.id, pins.id);
    assert!(matches!(p.get(&cx, &pins.id), Err(Error::NotFound(_))));
    assert!(matches!(p.forget(&cx, &pins.id), Err(Error::NotFound(_))));
    assert!(matches!(p.get(&cx, "nope"), Err(Error::Invalid(_))));
    assert!(matches!(
        p.remember(&cx, new(" ", Scope::User)),
        Err(Error::Invalid(_))
    ));
    // Newest first.
    let all = p.list(&cx, None, None).expect("list").items;
    assert!(
        all.windows(2)
            .all(|w| w[0].updated_unix_ms >= w[1].updated_unix_ms)
    );
}

#[test]
fn files_keeps_the_contract() {
    let d = Dirs::new("files");
    contract(&d.files("a"), &d);
}

/// Dreamer-written files: what an export must give back byte for byte.
fn seed(f: &Files, cx: &Context) {
    f.remember(
        cx,
        New {
            title: Some("Small commits".into()),
            description: Some("One change per commit".into()),
            links: vec!["docs/contributing.md".into(), "user/prefers-tabs".into()],
            ..new("Keep commits small: one change each.", Scope::Project)
        },
    )
    .expect("project");
    f.remember(
        cx,
        New {
            title: Some("Prefers tabs".into()),
            ..new("The user indents with tabs.\n\nAlso in YAML.", Scope::User)
        },
    )
    .expect("user");
    f.remember(cx, new("Staging listens on 8461", Scope::Host))
        .expect("host");
    f.remember(cx, new("Fixing the lints in this session", Scope::Session))
        .expect("session");
}

/// Every file under `dir`, relative, with its text.
fn tree(dir: &Path) -> Vec<(PathBuf, String)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                let rel = p.strip_prefix(base).expect("under").to_path_buf();
                out.push((rel, std::fs::read_to_string(&p).expect("read")));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn files_to_files_copies_every_scope_byte_for_byte() {
    let d = Dirs::new("files-copy");
    let (a, b) = (d.files("a"), d.files("b"));
    let cx = d.cx();
    seed(&a, &cx);
    // Project memories live in the project; give the copy its own.
    let moved = memory::transfer(&a, &b, &cx, None).expect("copy");
    assert_eq!(moved.ids.len(), 4);
    assert!(moved.problems.is_empty());
    assert_eq!(tree(&d.root.join("a")), tree(&d.root.join("b")));
    let got: Vec<Memory> = b.list(&cx, None, None).expect("b").items;
    let want: Vec<Memory> = a.list(&cx, None, None).expect("a").items;
    assert_eq!(
        got.into_iter().map(plain).collect::<Vec<_>>(),
        want.into_iter().map(plain).collect::<Vec<_>>()
    );
}

/// The database providers, `sqlite` and (with the `kosmonaut` feature)
/// Kosmonaut on a server, keep the same things and answer alike; each
/// test below runs on both: on a SQLite file of its own (`sqlite::`), and
/// on a Kosmonaut server of its own over gRPC (`server::`, when
/// `DREAMER_KOSMONAUT_SERVER` names the server program; see
/// `support/kosmonaut_server.rs`).
#[cfg(feature = "kosmonaut")]
#[path = "support/kosmonaut_server.rs"]
mod kosmonaut_server;

mod graph {
    use std::sync::Arc;

    use super::*;
    use memory::embed::{Embedder, StaticVectors};
    use memory::{CollectorRow, Sqlite, Summary};

    #[cfg(feature = "kosmonaut")]
    use super::kosmonaut_server::TestServer;

    /// What both database providers have besides [`Provider`].
    trait Graph: Provider {
        fn add_row(&self, row: &CollectorRow) -> Result<(), Error>;
        fn rows(
            &self,
            collector: &str,
            since: u64,
            limit: usize,
        ) -> Result<Vec<CollectorRow>, Error>;
        fn set_summary(&self, session: &str, text: &str) -> Result<Summary, Error>;
        fn summary(&self, session: &str) -> Result<Option<Summary>, Error>;
        fn relations_at(
            &self,
            cx: &Context,
            id: &str,
            at: u64,
        ) -> Result<Vec<(String, String)>, Error>;
    }

    macro_rules! graph {
        ($t:ty) => {
            impl Graph for $t {
                fn add_row(&self, row: &CollectorRow) -> Result<(), Error> {
                    <$t>::add_row(self, row)
                }
                fn rows(
                    &self,
                    collector: &str,
                    since: u64,
                    limit: usize,
                ) -> Result<Vec<CollectorRow>, Error> {
                    <$t>::rows(self, collector, since, limit)
                }
                fn set_summary(&self, session: &str, text: &str) -> Result<Summary, Error> {
                    <$t>::set_summary(self, session, text)
                }
                fn summary(&self, session: &str) -> Result<Option<Summary>, Error> {
                    <$t>::summary(self, session)
                }
                fn relations_at(
                    &self,
                    cx: &Context,
                    id: &str,
                    at: u64,
                ) -> Result<Vec<(String, String)>, Error> {
                    <$t>::relations_at(self, cx, id, at)
                }
            }
        };
    }
    graph!(Sqlite);
    #[cfg(feature = "kosmonaut")]
    graph!(memory::Kosmonaut);

    /// Where a test's database is.
    enum Db {
        Sqlite(PathBuf),
        #[cfg(feature = "kosmonaut")]
        Server(TestServer),
    }

    impl Db {
        fn open(&self, embedder: Option<Arc<dyn Embedder>>) -> Box<dyn Graph> {
            match self {
                Db::Sqlite(path) => Box::new(Sqlite::new(path.clone(), embedder)),
                #[cfg(feature = "kosmonaut")]
                Db::Server(s) => Box::new(memory::Kosmonaut::server(
                    memory::Server::new(&s.url, false, None).expect("server"),
                    embedder,
                )),
            }
        }
    }

    fn db(at: &Db) -> Box<dyn Graph> {
        at.open(None)
    }

    macro_rules! on_each {
        ($($name:ident),* $(,)?) => {
            mod sqlite {
                $(
                    #[test]
                    fn $name() {
                        let d = super::Dirs::new(concat!("sqlite-", stringify!($name)));
                        let at = super::Db::Sqlite(d.root.join("data/memory.sqlite"));
                        super::$name(&d, &at);
                    }
                )*
            }
            #[cfg(feature = "kosmonaut")]
            mod server {
                $(
                    #[test]
                    fn $name() {
                        let Some(s) = super::TestServer::start(stringify!($name), false) else {
                            return;
                        };
                        let d = super::Dirs::new(concat!("kserver-", stringify!($name)));
                        super::$name(&d, &super::Db::Server(s));
                    }
                )*
            }
        };
    }

    on_each!(
        keeps_the_contract,
        memories_last_across_a_reopen,
        recall_gives_the_order_files_gives,
        import_from_files_and_export_back_round_trips_the_files,
        links_are_edges_with_valid_times_and_recall_follows_them,
        unlinking_closes_an_edge_and_forgetting_deletes_it,
        with_an_embedder_recall_also_finds_memories_near_in_meaning,
        collector_rows_and_session_summaries_share_the_store,
    );

    fn keeps_the_contract(d: &Dirs, at: &Db) {
        contract(db(at).as_ref(), d);
    }

    fn memories_last_across_a_reopen(d: &Dirs, at: &Db) {
        let cx = d.cx();
        let id = {
            let k = db(at);
            k.remember(&cx, new("Deploys go through staging first", Scope::Project))
                .expect("remember")
                .id
        };
        let k = db(at);
        assert_eq!(
            k.get(&cx, &id).expect("get").text,
            "Deploys go through staging first"
        );
    }

    /// The same memories, without links (so no neighbours join), give the
    /// same recall as `files`: the same ids, scores, and order.
    fn recall_gives_the_order_files_gives(d: &Dirs, at: &Db) {
        let cx = d.cx();
        let f = d.files("a");
        let notes = [
            (
                "Rust toolchain",
                "Pinned rust 1.99 for every crate",
                Scope::Project,
            ),
            ("Rustfmt", "Run rustfmt before each commit", Scope::User),
            (
                "Trusted hosts",
                "The build host trusts the rust mirror",
                Scope::Host,
            ),
            (
                "Commit style",
                "Small commits with short subjects",
                Scope::User,
            ),
            (
                "Session goal",
                "Fix the rust lints in this session",
                Scope::Session,
            ),
            ("Release day", "Releases ship on Tuesdays", Scope::Project),
            ("Rust rust rust", "Nothing else", Scope::User),
        ];
        for (title, text, scope) in notes {
            f.remember(
                &cx,
                New {
                    title: Some(title.into()),
                    ..new(text, scope)
                },
            )
            .expect(title);
            // Distinct update times, so the newer-first tie break is
            // exercised and not left to the id.
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        let k = db(at);
        memory::transfer(&f, k.as_ref(), &cx, None).expect("import");
        for (q, limit) in [
            ("rust", None),
            ("rust", Some(2)),
            ("ru", None),
            ("commit rust", None),
            ("ship tuesdays", None),
            ("lints session rust", Some(3)),
            ("", Some(4)),
            ("nothing-matches-this", None),
        ] {
            let ranked = |found: memory::Found| -> Vec<(String, Option<u32>)> {
                found.items.into_iter().map(|m| (m.id, m.score)).collect()
            };
            let want = ranked(f.recall(&cx, q, None, limit).expect("files"));
            let got = ranked(k.recall(&cx, q, None, limit).expect("graph"));
            assert_eq!(got, want, "{q:?} {limit:?}");
        }
    }

    fn import_from_files_and_export_back_round_trips_the_files(d: &Dirs, at: &Db) {
        let (a, b) = (d.files("a"), d.files("b"));
        let cx = d.cx();
        seed(&a, &cx);
        let k = db(at);
        let moved = memory::transfer(&a, k.as_ref(), &cx, None).expect("import");
        assert_eq!(moved.ids.len(), 4);
        // The database answers as the files did.
        let want: Vec<Memory> = a.list(&cx, None, None).expect("a").items;
        let got: Vec<Memory> = k.list(&cx, None, None).expect("k").items;
        assert_eq!(got, want.iter().cloned().map(plain).collect::<Vec<_>>());
        // Word matches come in the same order; the database may add
        // related memories after them.
        for q in ["commits", "tabs yaml", "8461", "lints"] {
            let words = |f: memory::Found| {
                f.items
                    .into_iter()
                    .filter(|m| m.score >= Some(1000))
                    .map(|m| (m.id, m.score))
                    .collect::<Vec<_>>()
            };
            let want = words(a.recall(&cx, q, None, None).expect("a"));
            assert!(!want.is_empty(), "{q}");
            assert_eq!(words(k.recall(&cx, q, None, None).expect("k")), want, "{q}");
        }
        // Importing again replaces, it does not duplicate.
        memory::transfer(&a, k.as_ref(), &cx, None).expect("again");
        assert_eq!(k.list(&cx, None, None).expect("k").items.len(), 4);
        // Export into empty directories gives the same files back.
        memory::transfer(k.as_ref(), &b, &cx, None).expect("export");
        assert_eq!(tree(&d.root.join("a")), tree(&d.root.join("b")));
    }

    fn rel(r: &str, t: &str) -> (String, String) {
        (r.to_owned(), t.to_owned())
    }

    /// Far enough ahead to be after every edge opened now.
    const LATER: u64 = u64::MAX / 2000;

    fn links_are_edges_with_valid_times_and_recall_follows_them(d: &Dirs, at: &Db) {
        let cx = d.cx();
        let k = db(at);
        let style = k
            .remember(
                &cx,
                New {
                    title: Some("Style guide".into()),
                    links: vec!["docs/style.md".into()],
                    ..new("Indent with tabs.", Scope::Project)
                },
            )
            .expect("style");
        let review = k
            .remember(
                &cx,
                New {
                    title: Some("Review checklist".into()),
                    links: vec!["docs/style.md".into(), "pane:%3".into()],
                    ..new("Check the formatting before merging.", Scope::Project)
                },
            )
            .expect("review");
        let deploy = k
            .remember(
                &cx,
                New {
                    title: Some("Deploy".into()),
                    links: vec![style.id.clone()],
                    ..new("Ship on Tuesdays.", Scope::Project)
                },
            )
            .expect("deploy");
        // A project is its real path, links resolved.
        let real = std::fs::canonicalize(d.root.join("project")).expect("real");
        let root = real.display().to_string();
        let file = real.join("docs/style.md").display().to_string();
        assert_eq!(
            k.relations_at(&cx, &review.id, LATER).expect("now"),
            vec![
                rel("ABOUT_FILE", &file),
                rel("ABOUT_PANE", "%3"),
                rel("IN_PROJECT", &root)
            ]
        );
        assert_eq!(
            k.relations_at(&cx, &deploy.id, LATER).expect("deploy"),
            vec![rel("IN_PROJECT", &root), rel("LINKS_TO", &style.id)]
        );
        // "tabs" matches the style guide by its words; the review shares
        // its file and the deploy note links to it, so both come after it.
        let found = k.recall(&cx, "tabs", None, None).expect("recall");
        let ids: Vec<&str> = found.items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids[0], style.id);
        assert_eq!(ids.len(), 3, "{ids:?}");
        assert!(ids.contains(&review.id.as_str()) && ids.contains(&deploy.id.as_str()));
        assert!(
            found.items[1..].iter().all(|m| m.score == Some(510)),
            "{found:?}"
        );
        // Dropping a link: recall no longer follows it.
        k.update(
            &cx,
            &review.id,
            Change {
                links: Some(vec!["pane:%3".into()]),
                ..Change::default()
            },
        )
        .expect("update");
        let found = k.recall(&cx, "tabs", None, None).expect("recall");
        assert!(found.items.iter().all(|m| m.id != review.id));
    }

    /// Dropping a link closes its edge: it held until the change, and not
    /// after. Forgetting a memory deletes it with its edges, and the
    /// edges other memories had to it.
    fn unlinking_closes_an_edge_and_forgetting_deletes_it(d: &Dirs, at: &Db) {
        let cx = d.cx();
        let k = db(at);
        let port = || New {
            title: Some("Port".into()),
            ..new("Staging listens on 8461", Scope::User)
        };
        let target = k.remember(&cx, port()).expect("target");
        let note = k
            .remember(
                &cx,
                New {
                    links: vec![target.id.clone(), "pane:%7".into()],
                    ..new("Deploys check staging first", Scope::User)
                },
            )
            .expect("note");
        let both = vec![rel("ABOUT_PANE", "%7"), rel("LINKS_TO", &target.id)];
        assert_eq!(k.relations_at(&cx, &note.id, LATER).expect("now"), both);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let before = memory_now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        k.update(
            &cx,
            &note.id,
            Change {
                links: Some(vec![target.id.clone()]),
                ..Change::default()
            },
        )
        .expect("unlink");
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(k.relations_at(&cx, &note.id, before).expect("then"), both);
        assert_eq!(
            k.relations_at(&cx, &note.id, memory_now()).expect("now"),
            vec![rel("LINKS_TO", &target.id)]
        );
        // Before the memory was made, it had no edges.
        assert!(k.relations_at(&cx, &note.id, 1).expect("before").is_empty());
        // Linking the pane again opens a new edge; the closed one stays
        // closed.
        k.update(
            &cx,
            &note.id,
            Change {
                links: Some(vec![target.id.clone(), "pane:%7".into()]),
                ..Change::default()
            },
        )
        .expect("relink");
        assert_eq!(k.relations_at(&cx, &note.id, LATER).expect("again"), both);
        // Forgetting the target takes the link to it away.
        k.forget(&cx, &target.id).expect("forget");
        assert_eq!(
            k.relations_at(&cx, &note.id, LATER).expect("after"),
            vec![rel("ABOUT_PANE", "%7")]
        );
        // Remembered again under the same id, nothing links to it.
        let back = k.remember(&cx, port()).expect("back");
        assert_eq!(back.id, target.id);
        let found = k.recall(&cx, "8461", None, None).expect("recall");
        assert_eq!(found.items.len(), 1, "{found:?}");
    }

    fn memory_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis() as u64
    }

    fn with_an_embedder_recall_also_finds_memories_near_in_meaning(d: &Dirs, at: &Db) {
        let cx = d.cx();
        let vectors = StaticVectors::parse(
            "static:test",
            "deploy 1 0 0\nship 1 0 0\nrelease 0.9 0.1 0\nlunch 0 0 1\n",
        )
        .expect("vectors");
        // Written before the embedder was configured: recall embeds it.
        let plain_k = db(at);
        let ship = plain_k
            .remember(&cx, new("We ship on Tuesdays", Scope::User))
            .expect("ship");
        drop(plain_k);
        let k = at.open(Some(Arc::new(vectors)));
        k.remember(&cx, new("Lunch is at noon", Scope::User))
            .expect("lunch");
        let found = k.recall(&cx, "release", None, None).expect("recall");
        let ids: Vec<&str> = found.items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, [ship.id.as_str()]);
        assert!(found.items[0].score < Some(500));
        // Without an embedder there is nothing near.
        drop(k);
        assert!(
            db(at)
                .recall(&cx, "release", None, None)
                .expect("recall")
                .items
                .is_empty()
        );
    }

    /// An embedder that always fails: recall still answers by words and
    /// says why there were no vectors.
    #[derive(Debug)]
    struct Broken;

    impl Embedder for Broken {
        fn model(&self) -> &str {
            "broken"
        }
        fn embed(&self, _: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
            Err(Error::Unavailable("no model".into()))
        }
        fn dimension(&self) -> usize {
            3
        }
    }

    #[test]
    fn sqlite_recalls_by_words_when_the_embedder_fails() {
        let d = Dirs::new("sqlite-broken-embedder");
        let cx = d.cx();
        let path = d.root.join("data/memory.sqlite");
        let k = Sqlite::new(path.clone(), Some(Arc::new(Broken)));
        k.remember(&cx, new("Deploys go through staging", Scope::User))
            .expect("remember");
        k.remember(&cx, new("Lunch is at noon", Scope::User))
            .expect("other");
        let found = k.recall(&cx, "staging", None, None).expect("recall");
        assert_eq!(found.items.len(), 1);
        assert_eq!(found.problems.len(), 1, "{found:?}");
        assert_eq!(found.problems[0].path, path);
        assert!(
            found.problems[0]
                .message
                .contains("recalled without vectors: embedder broken"),
            "{found:?}"
        );
    }

    fn collector_rows_and_session_summaries_share_the_store(d: &Dirs, at: &Db) {
        let k = db(at);
        let row = |at, pane: &str| CollectorRow {
            collector: "test-failures".into(),
            at_unix_ms: at,
            pane: pane.into(),
            session: Some("s-1".into()),
            cwd: Some(d.root.join("project/src")),
            command: Some("cargo test".into()),
            fields: vec![("kind".into(), "FAILED".into())],
            context: vec!["test a ... FAILED".into()],
        };
        k.add_row(&row(20, "%2")).expect("row");
        k.add_row(&row(10, "%1")).expect("row");
        k.add_row(&CollectorRow {
            collector: "urls".into(),
            ..row(30, "%1")
        })
        .expect("other");
        let rows = k.rows("test-failures", 0, 10).expect("rows");
        assert_eq!(rows, vec![row(10, "%1"), row(20, "%2")]);
        assert_eq!(k.rows("test-failures", 15, 10).expect("since").len(), 1);
        assert_eq!(k.rows("test-failures", 0, 1).expect("limit").len(), 1);
        assert!(k.summary("s-1").expect("none").is_none());
        k.set_summary("s-1", "Fixed the lints.").expect("summary");
        k.set_summary("s-1", "Fixed the lints; tests pass.")
            .expect("again");
        assert_eq!(
            k.summary("s-1").expect("summary").map(|s| s.text),
            Some("Fixed the lints; tests pass.".into())
        );
        assert!(k.set_summary("../x", "no").is_err());
    }

    /// The schema is versioned: a new database gets this Dreamer's, and a
    /// database from a newer Dreamer is refused, not changed.
    #[test]
    fn sqlite_records_its_schema_and_refuses_a_newer_one() {
        let d = Dirs::new("sqlite-schema");
        let cx = d.cx();
        let path = d.root.join("data/memory.sqlite");
        // Nothing is made until the first call.
        let k = Sqlite::new(path.clone(), None);
        assert!(!path.exists());
        k.remember(&cx, new("Deploys go through staging", Scope::User))
            .expect("remember");
        assert!(path.is_file());
        drop(k);
        let c = rusqlite::Connection::open(&path).expect("open");
        let version: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, 2);
        let mode: String = c
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("mode");
        assert_eq!(mode, "wal");
        c.pragma_update(None, "user_version", 99).expect("newer");
        drop(c);
        let e = Sqlite::new(path, None)
            .list(&cx, None, None)
            .expect_err("newer");
        assert!(e.to_string().contains("has schema 99"), "{e}");
    }

    /// One project is one owner whatever path names it: through a
    /// symbolic link, a memory kept under one name is found under the
    /// other.
    #[cfg(unix)]
    #[test]
    fn sqlite_finds_a_project_under_any_name_for_it() {
        let d = Dirs::new("sqlite-alias");
        let alias = d.root.join("alias");
        std::os::unix::fs::symlink(d.root.join("project"), &alias).expect("link");
        let k = Sqlite::new(d.root.join("data/memory.sqlite"), None);
        let kept = k
            .remember(&d.cx(), new("Deploys go through staging", Scope::Project))
            .expect("remember");
        let via = Context {
            cwd: Some(alias.join("src")),
            session: None,
        };
        let found = k.list(&via, Some(Scope::Project), None).expect("list");
        assert_eq!(
            found.items.iter().map(|m| &m.id).collect::<Vec<_>>(),
            [&kept.id]
        );
    }

    /// What the files layout holds is copied in once per scope owner, the
    /// first time a call reaches it: same ids, text, links, and times; the
    /// files untouched; a memory forgotten here not copied again, even by
    /// a new provider on the same database; a file that does not read
    /// reported once; a memory already here under the same id kept.
    #[test]
    fn sqlite_copies_in_the_files_once_per_scope_owner() {
        let d = Dirs::new("sqlite-importing");
        let cx = d.cx();
        let files = Arc::new(d.files("a"));
        seed(&files, &cx);
        // A link between two memories of one scope, the later one first
        // in the listing.
        files
            .remember(
                &cx,
                New {
                    title: Some("Tabs in YAML too".into()),
                    links: vec!["user/prefers-tabs".into()],
                    ..new("YAML is indented with tabs as well.", Scope::User)
                },
            )
            .expect("linked");
        std::fs::write(
            d.root.join("a/config/memory/broken.md"),
            "no frontmatter here\n",
        )
        .expect("broken");
        let before = tree(&d.root.join("a"));
        let project_before = tree(&d.root.join("project/.dreamer"));
        let want: Vec<Memory> = files
            .list(&cx, None, None)
            .expect("files")
            .items
            .into_iter()
            .map(plain)
            .collect();
        assert_eq!(want.len(), 5);

        let path = d.root.join("data/memory.sqlite");
        let open = || Sqlite::new(path.clone(), None).importing(files.clone());
        let k = open();
        // Nothing is opened or copied until a call.
        assert!(!path.exists());
        let found = k.list(&cx, None, None).expect("list");
        assert_eq!(found.items, want);
        assert_eq!(found.problems.len(), 1, "{found:?}");
        assert!(found.problems[0].path.ends_with("broken.md"));
        assert_eq!(
            k.relations_at(&cx, "user/tabs-in-yaml-too", LATER)
                .expect("relations"),
            [rel("LINKS_TO", "user/prefers-tabs")]
        );
        // Reported once.
        assert!(k.list(&cx, None, None).expect("again").problems.is_empty());
        assert_eq!(tree(&d.root.join("a")), before);
        assert_eq!(tree(&d.root.join("project/.dreamer")), project_before);

        // Forgotten here, it stays forgotten: a fresh provider on the same
        // database does not copy it again.
        k.forget(&cx, "host/staging-listens-on-8461")
            .expect("forget");
        drop(k);
        let k = open();
        let ids: Vec<String> = k
            .list(&cx, None, None)
            .expect("reopened")
            .items
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids.len(), 4, "{ids:?}");
        assert!(!ids.iter().any(|i| i.starts_with("host/")), "{ids:?}");
        assert!(
            d.root
                .join("a/state/memory/host/staging-listens-on-8461.md")
                .is_file()
        );

        // A session reached for the first time is copied then; one the
        // database holds already under the same id keeps its own.
        let later = Context {
            session: Some("s-2".into()),
            ..cx.clone()
        };
        files
            .remember(&later, new("Second session note", Scope::Session))
            .expect("s-2 file");
        files
            .remember(&later, new("Kept in the database", Scope::Session))
            .expect("s-2 clash");
        let other = Sqlite::new(path.clone(), None);
        // Written without the copy, as a provider without files would.
        other
            .put(
                &later,
                &Memory {
                    text: "The database's own words".into(),
                    ..files
                        .get(&later, "session/kept-in-the-database")
                        .expect("clash")
                },
            )
            .expect("put");
        drop(other);
        let got = k
            .list(&later, Some(Scope::Session), None)
            .expect("s-2")
            .items;
        let texts: Vec<&str> = got.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(texts.contains(&"Second session note"), "{texts:?}");
        assert!(texts.contains(&"The database's own words"), "{texts:?}");
    }
}

/// Server mode only: a server that stops, and its token.
#[cfg(feature = "kosmonaut")]
mod kosmonaut {
    use std::time::{Duration, Instant};

    use super::*;
    use memory::{Kosmonaut, Server};

    use super::kosmonaut_server::TestServer;

    /// A server that stops is "memory unavailable", promptly, and memory
    /// is back when the server is.
    #[test]
    fn a_stopped_server_is_memory_unavailable_until_it_is_back() {
        let Some(mut s) = TestServer::start("stopped", false) else {
            return;
        };
        let d = Dirs::new("kserver-stopped");
        let cx = d.cx();
        let k = Kosmonaut::server(Server::new(&s.url, false, None).expect("server"), None);
        let m = k
            .remember(&cx, new("Deploys go through staging first", Scope::Project))
            .expect("remember");
        s.stop();
        let asked = Instant::now();
        let e = k.recall(&cx, "staging", None, None).expect_err("stopped");
        assert!(matches!(e, Error::Unavailable(_)), "{e:?}");
        assert!(
            e.to_string()
                .starts_with(&format!("memory unavailable: kosmonaut server {}", s.url)),
            "{e}"
        );
        assert!(asked.elapsed() < Duration::from_secs(10));
        assert!(matches!(
            k.remember(&cx, new("Releases ship on Tuesdays", Scope::Project)),
            Err(Error::Unavailable(_))
        ));
        s.restart();
        assert_eq!(k.get(&cx, &m.id).expect("back").text, m.text);
    }

    /// The token comes from the environment variable the settings name;
    /// with none there, or one the server refuses, memory is unavailable
    /// and says why.
    #[test]
    fn a_missing_or_refused_token_is_memory_unavailable() {
        let Some(s) = TestServer::start("token", true) else {
            return;
        };
        let d = Dirs::new("kserver-token");
        let cx = d.cx();
        let var = "DREAMER_TEST_KOSMONAUT_TOKEN_NEVER_SET";
        let k = Kosmonaut::server(
            Server::new(&s.url, false, Some(var.into())).expect("server"),
            None,
        );
        let e = k.list(&cx, None, None).expect_err("no token");
        assert!(matches!(e, Error::Unavailable(_)), "{e:?}");
        assert!(e.to_string().contains(&format!("{var} is not set")), "{e}");
        // No token at all, which a server that requires one refuses.
        let k = Kosmonaut::server(Server::new(&s.url, false, None).expect("server"), None);
        let e = k.list(&cx, None, None).expect_err("refused");
        assert!(matches!(e, Error::Unavailable(_)), "{e:?}");
        assert!(e.to_string().contains("refused the token"), "{e}");
    }
}
