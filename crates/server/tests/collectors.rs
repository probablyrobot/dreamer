//! Collectors through a real daemon and an isolated tmux server: rows from
//! matched lines with fields and context, command ends with their exit
//! status, a pane's exit, the rate cap and its honest drop count, tables
//! that survive a restart and expire, the row event, and covered panes the
//! tier policy leaves awake.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response};
use client::{Client, Pushed};
use prost::Message;
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

/// No tier policy unless a test sets one.
const NO_POLICY: &str = "tiers:\n  cold_after: never\n  warm_limit: none\n";

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        Setup::with(NO_POLICY)
    }

    fn with(settings: &str) -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-col-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        std::fs::write(root.join("config/dreamer.yaml"), settings).expect("settings");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let daemon = spawn(&root, &tmux);
        Setup { root, tmux, daemon }
    }

    /// Stop the daemon (SIGTERM, as a service manager would) and start a
    /// new one on the same directories.
    fn restart(&mut self) {
        let pid = self.daemon.id();
        let _ = Command::new("kill").arg(pid.to_string()).status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.daemon.try_wait().ok().flatten().is_none() {
            assert!(Instant::now() < deadline, "the daemon did not stop");
            std::thread::sleep(Duration::from_millis(50));
        }
        self.daemon = spawn(&self.root, &self.tmux);
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

    fn type_line(&self, pane: u32, text: &str) {
        self.tmux(&["send-keys", "-t", &format!("%{pane}"), text, "Enter"]);
    }
}

fn spawn(root: &std::path::Path, tmux: &TmuxServer) -> Child {
    // Not --background: the test owns the process and its exit.
    Command::new(EXE)
        .args(["--tmux-socket", tmux.socket(), "--tmux"])
        .arg(tmux_program())
        .env("DREAMER_RUNTIME_DIR", root.join("rt"))
        .env("DREAMER_STATE_DIR", root.join("st"))
        .env("DREAMER_CONFIG_DIR", root.join("config"))
        .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("dreamerd")
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

async fn panes(c: &Client) -> Vec<u32> {
    let Ok(Response::TmuxTopology(t)) = c
        .call(Request::TmuxTopology(api::TopologyParams::default()))
        .await
    else {
        panic!("topology")
    };
    let mut p: Vec<u32> = t.panes.iter().map(|p| p.id).collect();
    p.sort_unstable();
    p
}

fn line_rule(name: &str, pattern: &str) -> api::CollectorSpec {
    api::CollectorSpec {
        name: name.into(),
        from: Some(api::CollectFrom {
            all: true,
            ..api::CollectFrom::default()
        }),
        when: Some(api::CollectWhen {
            line: Some(pattern.into()),
            ..api::CollectWhen::default()
        }),
        ..api::CollectorSpec::default()
    }
}

async fn create(c: &Client, spec: api::CollectorSpec) -> api::CollectorStatus {
    match c.call(Request::CollectCreate(spec)).await {
        Ok(Response::CollectCreate(s)) => s,
        other => panic!("collect.create: {other:?}"),
    }
}

async fn query(c: &Client, q: api::CollectQuery) -> api::CollectResult {
    match c.call(Request::CollectQuery(q)).await {
        Ok(Response::CollectQuery(r)) => r,
        other => panic!("collect.query: {other:?}"),
    }
}

async fn all_rows(c: &Client, name: &str) -> api::CollectResult {
    query(
        c,
        api::CollectQuery {
            name: name.into(),
            limit: Some(10_000),
            ..api::CollectQuery::default()
        },
    )
    .await
}

/// Query until `done` holds for the result, or fail with `what`.
async fn until_rows(
    c: &Client,
    name: &str,
    what: &str,
    done: impl Fn(&api::CollectResult) -> bool,
) -> api::CollectResult {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let r = all_rows(c, name).await;
        if done(&r) {
            return r;
        }
        assert!(Instant::now() < deadline, "{what}: {r:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn status(c: &Client, name: &str) -> api::CollectorStatus {
    let Ok(Response::CollectList(l)) = c.call(Request::CollectList(api::Empty {})).await else {
        panic!("collect.list")
    };
    l.collectors
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no collector {name}"))
}

fn field<'a>(r: &'a api::CollectRow, name: &str) -> Option<&'a str> {
    r.fields
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.value.as_str())
}

#[tokio::test]
async fn a_line_collector_across_all_panes_captures_fields_and_context_from_two_panes() {
    let s = Setup::new();
    let mut c = s.client().await;
    let mut pushed = c.pushed().expect("pushed");
    s.tmux(&["new-window", "-d", "/bin/sh"]);
    let ps = panes(&c).await;
    assert_eq!(ps.len(), 2);
    c.call(Request::EventsSubscribe(api::EventSubscribe {
        kinds: vec!["collect".into()],
        ..api::EventSubscribe::default()
    }))
    .await
    .expect("events.subscribe");

    let mut spec = line_rule("fails", r"^FAIL (?P<test>\S+)");
    spec.capture = Some(api::CollectCapture {
        fields: vec![api::CollectField {
            name: "where".into(),
            value: "in ${test}".into(),
        }],
        before: 1,
        after: 1,
        ..api::CollectCapture::default()
    });
    let st = create(&c, spec).await;
    assert_eq!(st.state, api::CollectorState::Running as i32);
    assert_eq!(st.panes, ps, "it follows every pane");

    s.type_line(ps[0], "printf 'pre one\\nFAIL alpha\\npost one\\n'");
    s.type_line(ps[1], "printf 'pre two\\nFAIL beta\\npost two\\nok\\n'");
    let r = until_rows(&c, "fails", "a row from each pane", |r| r.rows.len() >= 2).await;
    assert_eq!(r.rows.len(), 2, "{r:?}");
    let mut rows = r.rows.clone();
    rows.sort_by_key(|r| r.pane);
    for (row, (pane, test, n)) in rows
        .iter()
        .zip([(ps[0], "alpha", "one"), (ps[1], "beta", "two")])
    {
        assert_eq!(row.pane, pane);
        assert_eq!(row.collector, "fails");
        assert_eq!(row.trigger, "line");
        assert_eq!(row.line, format!("FAIL {test}"));
        assert_eq!(field(row, "test"), Some(test));
        assert_eq!(field(row, "where"), Some(format!("in {test}").as_str()));
        assert_eq!(row.before, [format!("pre {n}")]);
        assert_eq!(row.after, [format!("post {n}")]);
        assert!(!row.session.is_empty() && row.window.is_some(), "{row:?}");
        assert!(row.time_unix_ms > 0);
    }

    // Grouped by a field: one each.
    let g = query(
        &c,
        api::CollectQuery {
            name: "fails".into(),
            group_by: Some("test".into()),
            ..api::CollectQuery::default()
        },
    )
    .await;
    let mut groups: Vec<(String, u64)> = g
        .groups
        .iter()
        .map(|g| (g.value.clone(), g.count))
        .collect();
    groups.sort();
    assert_eq!(groups, [("alpha".to_owned(), 1), ("beta".to_owned(), 1)]);
    // Filtered by pane and by field.
    let one = query(
        &c,
        api::CollectQuery {
            name: "fails".into(),
            fields: vec![api::CollectField {
                name: "test".into(),
                value: "beta".into(),
            }],
            ..api::CollectQuery::default()
        },
    )
    .await;
    assert_eq!(one.rows.len(), 1);
    assert_eq!(one.rows[0].pane, ps[1]);

    // An event per row, naming the collector, the pane, and the row id.
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while seen.len() < 2 {
        match tokio::time::timeout_at(deadline, pushed.recv()).await {
            Ok(Some(Pushed::Event { topic, payload, .. })) if topic == "events.batch" => {
                let b = api::EventBatch::decode(payload.as_slice()).expect("batch");
                for e in b.events {
                    assert_eq!(e.kind, "collect.row");
                    assert_eq!(e.subject.as_deref(), Some("fails"));
                    seen.push((e.pane.expect("pane"), e.state.expect("row id")));
                }
            }
            Ok(Some(_)) => {}
            other => panic!("no collect.row events: {other:?} after {seen:?}"),
        }
    }
    seen.sort();
    let mut want: Vec<(u32, String)> = rows.iter().map(|r| (r.pane, r.id.to_string())).collect();
    want.sort();
    assert_eq!(seen, want);

    // Read-only: a plain tmux client sees what the program wrote.
    let screen = s
        .tmux
        .run_checked(&["capture-pane", "-p", "-t", &format!("%{}", ps[0])])
        .expect("capture");
    assert!(screen.lines().any(|l| l == "FAIL alpha"), "{screen}");

    // Stopped, it records nothing more, and its rows stay.
    c.call(Request::CollectStop(api::CollectRef {
        name: "fails".into(),
        delete: false,
    }))
    .await
    .expect("stop");
    s.type_line(ps[0], "printf 'FAIL gamma\\n'");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(all_rows(&c, "fails").await.rows.len(), 2);
    assert_eq!(
        status(&c, "fails").await.state,
        api::CollectorState::Stopped as i32
    );
    // Deleted, it is gone.
    c.call(Request::CollectStop(api::CollectRef {
        name: "fails".into(),
        delete: true,
    }))
    .await
    .expect("delete");
    assert!(c.call(Request::CollectList(api::Empty {})).await.is_ok());
    let e = c
        .call(Request::CollectQuery(api::CollectQuery {
            name: "fails".into(),
            ..api::CollectQuery::default()
        }))
        .await
        .expect_err("deleted");
    assert!(e.to_string().contains("no collector"), "{e}");
}

#[tokio::test]
async fn definitions_are_checked() {
    let s = Setup::new();
    let c = s.client().await;
    let mut bad = line_rule("bad", "^(x)");
    bad.when.as_mut().expect("when").exit = true;
    let e = c
        .call(Request::CollectCreate(bad))
        .await
        .expect_err("two triggers");
    assert!(e.to_string().contains("exactly one"), "{e}");
    create(&c, line_rule("dup", "x")).await;
    let e = c
        .call(Request::CollectCreate(line_rule("dup", "y")))
        .await
        .expect_err("duplicate");
    assert!(e.to_string().contains("running"), "{e}");
}

#[tokio::test]
async fn command_end_records_the_exit_status_and_exit_records_how_a_pane_ended() {
    let s = Setup::new();
    let c = s.client().await;
    let ps = panes(&c).await;
    create(
        &c,
        api::CollectorSpec {
            name: "commands".into(),
            from: Some(api::CollectFrom {
                panes: vec![ps[0]],
                ..api::CollectFrom::default()
            }),
            when: Some(api::CollectWhen {
                command_end: true,
                ..api::CollectWhen::default()
            }),
            capture: Some(api::CollectCapture {
                lines: Some(3),
                ..api::CollectCapture::default()
            }),
            ..api::CollectorSpec::default()
        },
    )
    .await;
    // What a shell with shell integration writes around a command (OSC 133:
    // output starts; the command ends with its status).
    s.type_line(
        ps[0],
        r"printf '\033]133;C\007'; echo working; sh -c 'exit 3'; printf '\033]133;D;%s\007' $?",
    );
    let r = until_rows(&c, "commands", "a command_end row", |r| !r.rows.is_empty()).await;
    let row = &r.rows[0];
    assert_eq!(row.trigger, "command_end");
    assert_eq!(row.exit_status, Some(3), "{row:?}");
    assert_eq!(field(row, "exit_status"), Some("3"));
    assert!(field(row, "duration_ms").is_some(), "{row:?}");
    assert!(row.lines.iter().any(|l| l == "working"), "{row:?}");

    // A pane whose process ends: an exit row with its last lines.
    s.tmux(&["new-window", "-d", "/bin/sh"]);
    let second = *panes(&c)
        .await
        .iter()
        .find(|p| !ps.contains(p))
        .expect("new pane");
    create(
        &c,
        api::CollectorSpec {
            name: "exits".into(),
            from: Some(api::CollectFrom {
                panes: vec![second],
                ..api::CollectFrom::default()
            }),
            when: Some(api::CollectWhen {
                exit: true,
                ..api::CollectWhen::default()
            }),
            capture: Some(api::CollectCapture {
                lines: Some(5),
                ..api::CollectCapture::default()
            }),
            ..api::CollectorSpec::default()
        },
    )
    .await;
    s.type_line(second, "echo goodbye");
    tokio::time::sleep(Duration::from_millis(500)).await;
    s.type_line(second, "exit");
    let r = until_rows(&c, "exits", "an exit row", |r| !r.rows.is_empty()).await;
    let row = &r.rows[0];
    assert_eq!(row.trigger, "exit");
    assert_eq!(row.pane, second);
    assert_eq!(field(row, "reason"), Some("closed"), "{row:?}");
    assert_eq!(
        row.exit_status, None,
        "tmux reports no status for a closed pane"
    );
    assert!(row.lines.iter().any(|l| l == "goodbye"), "{row:?}");
}

#[tokio::test]
async fn a_flooding_pane_is_rate_capped_and_the_drops_are_counted() {
    let s = Setup::new();
    let c = s.client().await;
    let ps = panes(&c).await;
    let mut spec = line_rule("flood", r"^x\d+$");
    spec.per_second = Some(5);
    create(&c, spec).await;
    // 150 matching lines at once: within one look of the pane.
    s.type_line(
        ps[0],
        "i=0; while [ $i -lt 150 ]; do echo x$i; i=$((i+1)); done; echo done",
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let st = loop {
        let st = status(&c, "flood").await;
        if st.collected + st.dropped >= 150 {
            break st;
        }
        assert!(Instant::now() < deadline, "not every line was seen: {st:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        st.collected + st.dropped,
        150,
        "every match is a row or a drop: {st:?}"
    );
    assert!(st.collected <= 10, "the cap held: {st:?}");
    assert!(st.dropped >= 140, "{st:?}");
    let r = all_rows(&c, "flood").await;
    assert_eq!(r.dropped, st.dropped);
    assert_eq!(r.rows.len() as u64, st.collected);
    assert_eq!(st.last_error, None, "no line went unseen: {st:?}");
    // The cap refills: a later match is recorded, with the drops before it.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    s.type_line(ps[0], "echo x999");
    let r = until_rows(&c, "flood", "the next row", |r| {
        r.rows.iter().any(|row| row.line == "x999")
    })
    .await;
    let last = r.rows.last().expect("row");
    assert_eq!(last.line, "x999");
    let reported: u64 = r.rows.iter().map(|r| r.dropped).sum();
    assert_eq!(
        reported, st.dropped,
        "each drop is reported on the pane's next row"
    );
}

#[tokio::test]
async fn rows_survive_a_restart_and_expire_by_count_and_age() {
    let mut s = Setup::new();
    let c = s.client().await;
    let ps = panes(&c).await;
    let mut count = line_rule("count", r"^item (\d+)$");
    count.keep = Some(api::CollectKeep {
        rows: Some(3),
        max_age_s: None,
    });
    create(&c, count).await;
    let mut age = line_rule("age", r"^item (\d+)$");
    age.keep = Some(api::CollectKeep {
        rows: None,
        max_age_s: Some(2),
    });
    create(&c, age).await;
    s.type_line(ps[0], "for i in 1 2 3 4 5; do echo item $i; done");
    let r = until_rows(&c, "count", "the newest three rows", |r| {
        r.rows.last().is_some_and(|row| row.line == "item 5")
    })
    .await;
    let lines: Vec<&str> = r.rows.iter().map(|r| r.line.as_str()).collect();
    assert_eq!(lines, ["item 3", "item 4", "item 5"], "kept by count");
    let ids: Vec<u64> = r.rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, [3, 4, 5]);
    assert_eq!(status(&c, "count").await.collected, 5);
    let r = until_rows(&c, "age", "five rows", |r| r.rows.len() == 5).await;
    assert_eq!(r.rows.len(), 5);
    let r = until_rows(&c, "age", "rows gone by age", |r| r.rows.is_empty()).await;
    assert!(r.rows.is_empty());
    drop(c);

    s.restart();
    let c = s.client().await;
    let st = status(&c, "count").await;
    assert_eq!(
        st.state,
        api::CollectorState::Running as i32,
        "it runs again"
    );
    assert_eq!(st.collected, 5);
    let r = all_rows(&c, "count").await;
    assert_eq!(
        r.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        [3, 4, 5],
        "rows survive the restart"
    );
    assert_eq!(r.rows[2].fields[0].value, "5");
    assert!(
        all_rows(&c, "age").await.rows.is_empty(),
        "expired stays expired"
    );
    // It follows the panes again, and ids continue.
    let deadline = Instant::now() + Duration::from_secs(10);
    while status(&c, "count").await.panes.is_empty() {
        assert!(Instant::now() < deadline, "not following after the restart");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    s.type_line(ps[0], "echo item 6");
    let r = until_rows(&c, "count", "a row after the restart", |r| {
        r.rows.last().is_some_and(|row| row.line == "item 6")
    })
    .await;
    assert_eq!(r.rows.last().expect("row").id, 6);
}

#[tokio::test]
async fn a_pane_a_collector_covers_is_not_put_to_sleep() {
    use api::MirrorState::{Cold, Live};
    let s = Setup::new();
    let c = s.client().await;
    s.tmux(&["new-window", "-d", "/bin/sh"]);
    let ps = panes(&c).await;
    assert_eq!(ps.len(), 2);
    let mut spec = line_rule("one", "^never$");
    spec.from = Some(api::CollectFrom {
        panes: vec![ps[0]],
        ..api::CollectFrom::default()
    });
    create(&c, spec).await;
    // Quiet panes go Cold after a second, unless something follows them.
    std::fs::write(
        s.root.join("config/dreamer.yaml.new"),
        "tiers:\n  cold_after: 1s\n  warm_limit: none\n",
    )
    .expect("settings");
    std::fs::rename(
        s.root.join("config/dreamer.yaml.new"),
        s.root.join("config/dreamer.yaml"),
    )
    .expect("settings");
    let states = || async {
        let Ok(Response::TerminalMirrors(l)) = c
            .call(Request::TerminalMirrors(api::TopologyParams::default()))
            .await
        else {
            panic!("mirrors")
        };
        ps.iter()
            .map(|p| {
                l.mirrors
                    .iter()
                    .find(|m| m.pane == *p)
                    .map_or(-1, |m| m.state)
            })
            .collect::<Vec<i32>>()
    };
    let want = vec![Live as i32, Cold as i32];
    let deadline = Instant::now() + Duration::from_secs(15);
    while states().await != want {
        assert!(Instant::now() < deadline, "{:?}", states().await);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(states().await, want, "the covered pane stays awake");
}
