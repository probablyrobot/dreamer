//! Workspace files → plans, and the errors users see.

use std::path::Path;

use workspace_config::{Arrange, Direction, PaneRef, SizeArg, Start, load, parse};

fn example() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/dreamer-dev.yaml")
}

fn plan_errors(yaml: &str) -> Vec<String> {
    let dir = std::env::temp_dir().join(format!("dreamer-ws-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!("t{n}.yaml"));
    std::fs::write(&path, yaml).expect("write");
    match load(&path) {
        Ok(_) => Vec::new(),
        Err(errs) => errs.iter().map(ToString::to_string).collect(),
    }
}

#[test]
fn the_example_plans_as_written() {
    let plan = load(&example()).expect("example is valid");
    assert_eq!(plan.session, "dreamer-dev");
    assert_eq!(plan.windows.len(), 3);
    assert_eq!(plan.focus_window, Some(0));
    assert!(plan.env.contains(&("RUST_LOG".into(), "info".into())));

    // code: [editor 60% | [check / shell]]
    let code = &plan.windows[0];
    assert_eq!(code.panes.len(), 3);
    assert_eq!(code.splits.len(), 2);
    let first = &code.splits[0];
    assert_eq!((first.target, first.new), (PaneRef(0), PaneRef(1)));
    assert_eq!(first.direction, Direction::Horizontal);
    assert_eq!(first.size, Some(SizeArg::Percent(60)));
    assert_eq!(code.panes[1].start, Start::Run("nvim .".into()));
    assert_eq!(code.focus_pane, Some(PaneRef(1)));
    let second = &code.splits[1];
    assert_eq!(second.direction, Direction::Vertical);
    assert_eq!(second.target, PaneRef(0));
    assert_eq!(second.size, Some(SizeArg::Percent(50)));
    assert_eq!(
        code.panes[2].start,
        Start::Exec(vec![
            "cargo".into(),
            "watch".into(),
            "-x".into(),
            "check".into()
        ])
    );
    assert_eq!(code.panes[0].title.as_deref(), Some("shell"));
    // Declared order: editor, check, shell.
    let titles: Vec<_> = code
        .declared
        .iter()
        .map(|r| code.panes[r.0].title.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(titles, ["editor", "check", "shell"]);

    // tests: flat list, relative root, preset.
    let tests = &plan.windows[1];
    assert_eq!(tests.arrange, Some(Arrange::EvenHorizontal));
    assert!(
        tests
            .panes
            .iter()
            .all(|p| p.cwd.ends_with("crates/tmux-backend"))
    );
    assert!(tests.panes.iter().any(|p| p.keep));

    // logs: fixed 8-cell status pane, env merged per pane.
    let logs = &plan.windows[2];
    assert_eq!(logs.splits[0].size, Some(SizeArg::Cells(8)));
    let tail = logs
        .panes
        .iter()
        .find(|p| p.title.as_deref() == Some("tail"))
        .expect("tail");
    assert!(tail.env.contains(&("LOG_LEVEL".into(), "debug".into())));
    assert!(tail.env.contains(&("RUST_LOG".into(), "info".into())));
}

#[test]
fn three_way_split_sizes_are_exact_shares_of_the_remaining_region() {
    let yaml = "version: 1\nwindows:\n  - layout:\n      split: horizontal\n      panes:\n        - size: 20%\n        - size: 30%\n        - {}\n";
    let file = parse(yaml, None).expect("parse");
    assert_eq!(file.windows.len(), 1);
    let dir = std::env::temp_dir().join(format!("dreamer-ws-3way-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("three.yaml");
    std::fs::write(&path, yaml).expect("write");
    let plan = load(&path).expect("plan");
    let sizes: Vec<_> = plan.windows[0].splits.iter().map(|s| s.size).collect();
    // 20% of the whole, then 30% of the whole = 30/80 of what remains.
    assert_eq!(
        sizes,
        [Some(SizeArg::Percent(20)), Some(SizeArg::Percent(38))]
    );
}

#[test]
fn unknown_keys_report_line_and_column() {
    let errors = plan_errors("version: 1\nwindows:\n  - name: a\n    lyout: {}\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains(":4:"), "{}", errors[0]);
    assert!(errors[0].contains("lyout"), "{}", errors[0]);
}

#[test]
fn semantic_errors_name_the_element() {
    let cases = [
        ("version: 2\nwindows: [{}]\n", "unsupported version"),
        ("version: 1\nwindows: []\n", "at least one window"),
        (
            "version: 1\nwindows:\n  - layout:\n      split: vertical\n      panes:\n        - size: 70%\n        - size: 40%\n",
            "windows[0].layout",
        ),
        (
            "version: 1\nwindows:\n  - panes:\n      - exec: [ls]\n        run: ls\n",
            "at most one of",
        ),
        (
            "version: 1\nwindows:\n  - panes: [{}]\n    arrange: spiral\n",
            "unknown preset",
        ),
        (
            "version: 1\nenv: {\"BAD-NAME\": x}\nwindows: [{}]\n",
            "invalid environment variable",
        ),
        (
            "version: 1\nwindows:\n  - layout:\n      split: horizontal\n      panes: [{}]\n",
            "at least two panes",
        ),
        (
            "version: 1\nwindows:\n  - layout:\n      split: horizontal\n      run: ls\n      panes: [{}, {}]\n",
            "cannot also have pane fields",
        ),
        (
            "version: 1\nname: \"a:b\"\nwindows: [{}]\n",
            "session names",
        ),
        (
            "version: 1\nwindows:\n  - panes:\n      - size: 150%\n      - {}\n",
            "must be a percentage",
        ),
    ];
    for (yaml, expected) in cases {
        let errors = plan_errors(yaml);
        assert!(
            errors.iter().any(|e| e.contains(expected)),
            "expected `{expected}` for:\n{yaml}\ngot {errors:?}"
        );
    }
}

#[test]
fn a_window_without_panes_gets_one_shell() {
    let dir = std::env::temp_dir().join(format!("dreamer-ws-min-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("minimal.yaml");
    std::fs::write(&path, "version: 1\nwindows: [{name: one}]\n").expect("write");
    let plan = load(&path).expect("plan");
    assert_eq!(plan.session, "minimal");
    assert_eq!(plan.windows[0].panes.len(), 1);
    assert_eq!(plan.windows[0].panes[0].start, Start::Shell);
    assert_eq!(plan.windows[0].panes[0].cwd, dir);
}
