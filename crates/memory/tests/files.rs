//! The `files` provider: memories are plain files a person can read and
//! edit, and what they change is what the next call sees.

use std::path::{Path, PathBuf};

use memory::{Change, Context, Error, Files, Kind, New, Provider, Scope};

struct Dirs {
    root: PathBuf,
}

impl Dirs {
    fn new(name: &str) -> Dirs {
        let root =
            std::env::temp_dir().join(format!("dreamer-memory-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("project/.git")).expect("project");
        std::fs::create_dir_all(root.join("project/src/deep")).expect("deep");
        Dirs { root }
    }
    fn files(&self) -> Files {
        Files::new(
            Some(self.root.join("config/memory")),
            self.root.join("state/memory/host"),
            self.root.join("state/memory/sessions"),
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

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).expect("read")
}

#[test]
fn a_memory_is_a_plain_file_with_an_index_line() {
    let d = Dirs::new("plain");
    let f = d.files();
    let m = f
        .remember(
            &d.cx(),
            New {
                title: Some("Prefers tabs".into()),
                description: Some("Indent with tabs everywhere".into()),
                links: vec!["docs/style.md".into()],
                ..new("The user indents with tabs.", Scope::User)
            },
        )
        .expect("remember");
    assert_eq!(m.id, "user/prefers-tabs");
    let path = d.root.join("config/memory/prefers-tabs.md");
    assert_eq!(m.path.as_deref(), Some(path.as_path()));
    let text = read(&path);
    assert!(text.starts_with("---\nname: Prefers tabs\n"), "{text}");
    assert!(text.contains("\ntype: feedback\n"), "{text}");
    assert!(
        text.contains("\n  - docs/style.md\n") || text.contains("\n- docs/style.md\n"),
        "{text}"
    );
    assert!(
        text.ends_with("---\n\nThe user indents with tabs.\n"),
        "{text}"
    );
    assert_eq!(
        read(&d.root.join("config/memory/MEMORY.md")),
        "- [Prefers tabs](prefers-tabs.md) — Indent with tabs everywhere\n"
    );
    // The same title again gets a file of its own.
    let again = f
        .remember(
            &d.cx(),
            New {
                title: Some("Prefers tabs".into()),
                ..new("again", Scope::User)
            },
        )
        .expect("again");
    assert_eq!(again.id, "user/prefers-tabs-2");
    assert_eq!(f.get(&d.cx(), "user/prefers-tabs").expect("get"), m);
}

#[test]
fn a_hand_edit_is_what_the_next_call_sees() {
    let d = Dirs::new("edit");
    let f = d.files();
    let m = f
        .remember(
            &d.cx(),
            new("Deploys go through staging first.", Scope::Project),
        )
        .expect("remember");
    let path = d
        .root
        .join("project/.dreamer/memory/deploys-go-through-staging-first.md");
    assert_eq!(m.path.as_deref(), Some(path.as_path()));
    // A person rewrites the file, adds a key of their own, and saves.
    std::fs::write(
        &path,
        "---\nname: Deploys\ndescription: Staging, then production\ntype: project\nowner: ops\n---\n\nDeploys go through canary now.\n",
    )
    .expect("edit");
    let got = f.get(&d.cx(), &m.id).expect("get");
    assert_eq!(got.title, "Deploys");
    assert_eq!(got.kind, Kind::Project);
    assert_eq!(got.text, "Deploys go through canary now.");
    let hits = f.recall(&d.cx(), "canary", None, None).expect("recall");
    assert_eq!(hits.items.len(), 1);
    assert!(
        f.recall(&d.cx(), "staging first", None, None)
            .expect("recall")
            .items
            .iter()
            .all(|i| i.text.contains("canary"))
    );
    // An update keeps the person's own key and rewrites the index line.
    f.update(
        &d.cx(),
        &m.id,
        Change {
            description: Some("Canary first".into()),
            ..Change::default()
        },
    )
    .expect("update");
    let text = read(&path);
    assert!(text.contains("owner: ops"), "{text}");
    assert!(text.contains("Deploys go through canary now."), "{text}");
    assert_eq!(
        read(&d.root.join("project/.dreamer/memory/MEMORY.md")),
        "- [Deploys](deploys-go-through-staging-first.md) — Canary first\n"
    );
    // A file a person adds by hand is a memory too; `kind` reads as `type`.
    std::fs::write(
        d.root.join("project/.dreamer/memory/by-hand.md"),
        "---\nname: Written by hand\nkind: reference\n---\nThe runbook is in the wiki.\n",
    )
    .expect("by hand");
    let hand = f.get(&d.cx(), "project/by-hand").expect("by hand");
    assert_eq!(
        (hand.kind, hand.text.as_str()),
        (Kind::Reference, "The runbook is in the wiki.")
    );
}

#[test]
fn recall_looks_in_the_scopes_asked_for() {
    let d = Dirs::new("scopes");
    let f = d.files();
    let cx = d.cx();
    for (text, scope) in [
        ("Rust edition 2024 in every crate", Scope::User),
        ("This mac builds with the arm64 rust target", Scope::Host),
        ("The project pins rust 1.99", Scope::Project),
        ("This session is fixing the rust lints", Scope::Session),
    ] {
        f.remember(&cx, new(text, scope)).expect(text);
    }
    assert_eq!(
        f.recall(&cx, "rust", None, None).expect("all").items.len(),
        4
    );
    for scope in Scope::ALL {
        let got = f.recall(&cx, "rust", Some(scope), None).expect("one");
        assert_eq!(got.items.len(), 1, "{scope:?}");
        assert_eq!(got.items[0].scope, scope);
        assert!(got.items[0].id.starts_with(scope.name()));
    }
    // The session scope is the session's; project and session need context.
    let other = Context {
        session: Some("s-2".into()),
        ..cx.clone()
    };
    assert!(
        f.recall(&other, "rust", Some(Scope::Session), None)
            .expect("other")
            .items
            .is_empty()
    );
    let bare = Context::default();
    assert_eq!(
        f.recall(&bare, "rust", None, None)
            .expect("bare")
            .items
            .len(),
        2
    );
    assert!(matches!(
        f.recall(&bare, "rust", Some(Scope::Project), None),
        Err(Error::Invalid(_))
    ));
    // The project is the git root, from any directory below it.
    let top = Context {
        cwd: Some(d.root.join("project")),
        session: None,
    };
    assert_eq!(
        f.recall(&top, "pins", Some(Scope::Project), None)
            .expect("top")
            .items
            .len(),
        1
    );
    // Best match first, and the limit holds.
    let best = f
        .recall(&cx, "rust lints session", None, Some(1))
        .expect("best");
    assert_eq!(best.items.len(), 1);
    assert_eq!(best.items[0].scope, Scope::Session);
    assert!(best.items[0].score.is_some());
}

#[test]
fn forget_removes_the_file_and_its_index_line_only() {
    let d = Dirs::new("forget");
    let f = d.files();
    let a = f
        .remember(&d.cx(), new("First thing", Scope::Host))
        .expect("a");
    let b = f
        .remember(&d.cx(), new("Second thing", Scope::Host))
        .expect("b");
    let index = d.root.join("state/memory/host/MEMORY.md");
    // A line a person wrote stays.
    let mut text = read(&index);
    text.insert_str(0, "# Notes for this mac\n");
    std::fs::write(&index, &text).expect("index");
    let gone = f.forget(&d.cx(), &a.id).expect("forget");
    assert_eq!(gone.id, a.id);
    assert!(!d.root.join("state/memory/host/first-thing.md").exists());
    assert_eq!(
        read(&index),
        "# Notes for this mac\n- [Second thing](second-thing.md) — Second thing\n"
    );
    assert!(matches!(f.get(&d.cx(), &a.id), Err(Error::NotFound(_))));
    assert!(matches!(f.forget(&d.cx(), &a.id), Err(Error::NotFound(_))));
    assert_eq!(
        f.list(&d.cx(), None, None).expect("list").items,
        vec![f.get(&d.cx(), &b.id).expect("b")]
    );
}

#[test]
fn a_malformed_file_is_reported_and_left_alone() {
    let d = Dirs::new("malformed");
    let f = d.files();
    f.remember(&d.cx(), new("A good one about tabs", Scope::User))
        .expect("good");
    let dir = d.root.join("config/memory");
    let cases = [
        ("no-front.md", "just text about tabs\n", "no frontmatter"),
        (
            "bad-yaml.md",
            "---\nname: [unclosed\ntype: user\n---\ntabs\n",
            "frontmatter",
        ),
        ("no-type.md", "---\nname: tabs\n---\ntabs\n", "no `type:`"),
        (
            "odd-type.md",
            "---\nname: tabs\ntype: gossip\n---\ntabs\n",
            "gossip",
        ),
    ];
    for (name, text, _) in cases {
        std::fs::write(dir.join(name), text).expect(name);
    }
    let found = f.recall(&d.cx(), "tabs", None, None).expect("recall");
    assert_eq!(found.items.len(), 1);
    assert_eq!(found.problems.len(), cases.len());
    for (name, text, why) in cases {
        let p = found
            .problems
            .iter()
            .find(|p| p.path.ends_with(name))
            .expect(name);
        assert!(p.message.contains(why), "{name}: {}", p.message);
        let id = format!("user/{}", name.trim_end_matches(".md"));
        assert!(
            matches!(f.get(&d.cx(), &id), Err(Error::Malformed(_))),
            "{name}"
        );
        assert!(
            matches!(f.forget(&d.cx(), &id), Err(Error::Malformed(_))),
            "{name}"
        );
        assert!(
            matches!(
                f.update(&d.cx(), &id, Change::default()),
                Err(Error::Malformed(_))
            ),
            "{name}"
        );
        assert_eq!(read(&dir.join(name)), text, "{name} must be untouched");
    }
}
