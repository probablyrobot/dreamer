//! What every provider does the same way, run against `files` and, with
//! the `kosmonaut` feature, the embedded Kosmonaut provider; and moving
//! memories between them.

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

#[cfg(feature = "kosmonaut")]
mod kosmonaut {
    use std::sync::Arc;

    use super::*;
    use memory::Kosmonaut;
    use memory::embed::StaticVectors;

    fn db(d: &Dirs) -> Kosmonaut {
        Kosmonaut::new(d.root.join("data/memory.kosmonaut"), None)
    }

    #[test]
    fn kosmonaut_keeps_the_contract() {
        let d = Dirs::new("kosmo");
        contract(&db(&d), &d);
    }

    #[test]
    fn memories_last_across_a_reopen() {
        let d = Dirs::new("kosmo-reopen");
        let cx = d.cx();
        let id = {
            let k = db(&d);
            k.remember(&cx, new("Deploys go through staging first", Scope::Project))
                .expect("remember")
                .id
        };
        let k = db(&d);
        assert_eq!(
            k.get(&cx, &id).expect("get").text,
            "Deploys go through staging first"
        );
    }

    #[test]
    fn import_from_files_and_export_back_round_trips_the_files() {
        let d = Dirs::new("kosmo-roundtrip");
        let (a, b) = (d.files("a"), d.files("b"));
        let cx = d.cx();
        seed(&a, &cx);
        let k = db(&d);
        let moved = memory::transfer(&a, &k, &cx, None).expect("import");
        assert_eq!(moved.ids.len(), 4);
        // Kosmonaut answers as the files did.
        let want: Vec<Memory> = a.list(&cx, None, None).expect("a").items;
        let got: Vec<Memory> = k.list(&cx, None, None).expect("k").items;
        assert_eq!(got, want.iter().cloned().map(plain).collect::<Vec<_>>());
        // Word matches come in the same order; Kosmonaut may add related
        // memories after them.
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
        memory::transfer(&a, &k, &cx, None).expect("again");
        assert_eq!(k.list(&cx, None, None).expect("k").items.len(), 4);
        // Export into empty directories gives the same files back.
        memory::transfer(&k, &b, &cx, None).expect("export");
        assert_eq!(tree(&d.root.join("a")), tree(&d.root.join("b")));
    }

    #[test]
    fn links_are_edges_with_valid_times_and_recall_follows_them() {
        let d = Dirs::new("kosmo-graph");
        let cx = d.cx();
        let k = db(&d);
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
        let root = d.root.join("project").display().to_string();
        let file = d.root.join("project/docs/style.md").display().to_string();
        let rel = |r: &str, t: &str| (r.to_owned(), t.to_owned());
        assert_eq!(
            k.relations_at(&cx, &review.id, u64::MAX / 2000)
                .expect("now"),
            vec![
                rel("ABOUT_FILE", &file),
                rel("ABOUT_PANE", "%3"),
                rel("IN_PROJECT", &root)
            ]
        );
        assert_eq!(
            k.relations_at(&cx, &deploy.id, u64::MAX / 2000)
                .expect("deploy"),
            vec![rel("IN_PROJECT", &root), rel("LINKS_TO", &style.id)]
        );
        // "tabs" matches the style guide by its words; the review shares
        // its file and the deploy note links to it, so both come after it.
        let found = k.recall(&cx, "tabs", None, None).expect("recall");
        let ids: Vec<&str> = found.items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids[0], style.id);
        assert_eq!(ids.len(), 3, "{ids:?}");
        assert!(ids.contains(&review.id.as_str()) && ids.contains(&deploy.id.as_str()));
        assert!(found.items[1..].iter().all(|m| m.score < Some(1000)));
        // Dropping a link closes its edge: it held until the change, and
        // not after.
        let before = memory_now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        k.update(
            &cx,
            &review.id,
            Change {
                links: Some(vec!["pane:%3".into()]),
                ..Change::default()
            },
        )
        .expect("update");
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(
            k.relations_at(&cx, &review.id, before)
                .expect("then")
                .contains(&rel("ABOUT_FILE", &file))
        );
        assert!(
            !k.relations_at(&cx, &review.id, memory_now())
                .expect("now")
                .contains(&rel("ABOUT_FILE", &file))
        );
        let found = k.recall(&cx, "tabs", None, None).expect("recall");
        assert!(found.items.iter().all(|m| m.id != review.id));
    }

    fn memory_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis() as u64
    }

    #[test]
    fn with_an_embedder_recall_also_finds_memories_near_in_meaning() {
        let d = Dirs::new("kosmo-vectors");
        let cx = d.cx();
        let vectors = StaticVectors::parse(
            "static:test",
            "deploy 1 0 0\nship 1 0 0\nrelease 0.9 0.1 0\nlunch 0 0 1\n",
        )
        .expect("vectors");
        // Written before the embedder was configured: recall embeds it.
        let plain_k = db(&d);
        let ship = plain_k
            .remember(&cx, new("We ship on Tuesdays", Scope::User))
            .expect("ship");
        drop(plain_k);
        let k = Kosmonaut::new(
            d.root.join("data/memory.kosmonaut"),
            Some(Arc::new(vectors)),
        );
        k.remember(&cx, new("Lunch is at noon", Scope::User))
            .expect("lunch");
        let found = k.recall(&cx, "release", None, None).expect("recall");
        let ids: Vec<&str> = found.items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, [ship.id.as_str()]);
        assert!(found.items[0].score < Some(500));
        // Without an embedder there is nothing near.
        drop(k);
        assert!(
            db(&d)
                .recall(&cx, "release", None, None)
                .expect("recall")
                .items
                .is_empty()
        );
    }

    #[test]
    fn collector_rows_and_session_summaries_share_the_store() {
        let d = Dirs::new("kosmo-rows");
        let k = db(&d);
        let row = |at, pane: &str| memory::CollectorRow {
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
        k.add_row(&memory::CollectorRow {
            collector: "urls".into(),
            ..row(30, "%1")
        })
        .expect("other");
        let rows = k.rows("test-failures", 0, 10).expect("rows");
        assert_eq!(rows, vec![row(10, "%1"), row(20, "%2")]);
        assert_eq!(k.rows("test-failures", 15, 10).expect("since").len(), 1);
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
}
