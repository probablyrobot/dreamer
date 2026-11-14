//! Cold-attach feasibility tests against real tmux (baseline 3.8).
//!
//! Each test starts an isolated server (`-L` socket, `-f /dev/null`, minimal
//! environment), bootstraps one pane through a control client, keeps feeding
//! live output, and then compares the mirror with tmux's own view:
//! the full history, the visible rows, the cursor, and relevant modes.
//! Results are summarized in `docs/tmux-bootstrap.md`.

use std::time::{Duration, Instant};

use terminal_ghostty::Engine;
use terminal_ghostty::core::CursorShape;
use tmux_backend::{Bootstrapped, ControlClient, TmuxServer, bootstrap_pane};
use tmux_protocol::{Event, Notification, PaneId};

const TIMEOUT: Duration = Duration::from_secs(20);
const DONE: &str = "DONE-7f3a";

fn require_tmux_baseline() {
    let out = std::process::Command::new(tmux_backend::server::tmux_program())
        .arg("-V")
        .output()
        .expect("tmux not found; set DREAMER_TMUX to a tmux 3.8 binary");
    let version = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let numbers: Vec<u32> = version
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .split(|c: char| !c.is_ascii_digit())
        .take(2)
        .filter_map(|p| p.parse().ok())
        .collect();
    assert!(
        numbers.as_slice() >= [3, 8].as_slice(),
        "tmux 3.8 or newer required, found `{version}`"
    );
}

fn pane_id(server: &TmuxServer, target: &str) -> PaneId {
    let id = server.format(target, "#{pane_id}").expect("pane id");
    PaneId::parse(id.as_bytes()).expect("pane id format")
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Feed live output for `pane` until `marker` has been seen, then drain
/// anything still queued.
fn follow_until(client: &mut ControlClient, engine: &mut Engine, pane: PaneId, marker: &str) {
    // The stream may have finished before the bootstrap captured it.
    if engine.screen_text().expect("text").contains(marker) {
        drain(client, engine, pane, Duration::from_millis(150));
        return;
    }
    let deadline = Instant::now() + TIMEOUT;
    let mut tail: Vec<u8> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "timed out waiting for `{marker}`");
        if let Event::Notification(Notification::Output { pane: p, data }) =
            client.recv(left).expect("event")
            && p == pane
        {
            engine.feed(&data).expect("feed");
            tail.extend_from_slice(&data);
            if find(&tail, marker.as_bytes()) {
                break;
            }
            let keep = tail.len().saturating_sub(marker.len());
            tail.drain(..keep);
        }
    }
    drain(client, engine, pane, Duration::from_millis(150));
}

/// Apply any output that arrives within `quiet` of the last event.
fn drain(client: &mut ControlClient, engine: &mut Engine, pane: PaneId, quiet: Duration) {
    loop {
        match client.recv(quiet) {
            Ok(Event::Notification(Notification::Output { pane: p, data })) if p == pane => {
                engine.feed(&data).expect("feed");
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn trimmed_lines(text: &str) -> Vec<String> {
    trimmed_lines_with_tabs(text, &[])
}

/// tmux keeps literal tabs in its grid and prints them in captures; Ghostty
/// formats the cells they skipped as spaces. Expand tabs with the pane's
/// tab stops before comparing.
fn trimmed_lines_with_tabs(text: &str, stops: &[usize]) -> Vec<String> {
    let expand = |line: &str| -> String {
        let mut out = String::new();
        let mut col = 0usize;
        for ch in line.chars() {
            if ch == '\t' {
                let next = stops.iter().copied().find(|&s| s > col).unwrap_or(col + 1);
                out.extend(std::iter::repeat_n(' ', next - col));
                col = next;
            } else {
                out.push(ch);
                col += 1;
            }
        }
        out
    };
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| expand(l).trim_end().to_owned())
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// Compare the mirror with tmux: full history, visible rows, and cursor.
fn assert_matches_tmux(server: &TmuxServer, target: &str, engine: &mut Engine) {
    assert_matches_tmux_inner(server, target, engine, true);
}

/// Visible rows and cursor only. Used where tmux and Ghostty differ in
/// history semantics (tmux 3.8 keeps lines scrolled out of a scroll region
/// with a top margin; Ghostty, like xterm, does not).
fn assert_visible_matches_tmux(server: &TmuxServer, target: &str, engine: &mut Engine) {
    assert_matches_tmux_inner(server, target, engine, false);
}

fn assert_matches_tmux_inner(
    server: &TmuxServer,
    target: &str,
    engine: &mut Engine,
    with_history: bool,
) {
    let tmux_all = server
        .run_checked(&["capture-pane", "-p", "-S", "-", "-E", "-", "-t", target])
        .expect("capture all");
    let ours_all = engine.screen_text().expect("screen text");
    let stops: Vec<usize> = server
        .format(target, "#{pane_tabs}")
        .expect("tabs")
        .split(',')
        .filter_map(|t| t.parse().ok())
        .collect();
    let (t, o) = (
        trimmed_lines_with_tabs(&tmux_all, &stops),
        trimmed_lines(&ours_all),
    );
    if with_history && t != o {
        if std::env::var_os("DREAMER_DEBUG_BOOTSTRAP").is_some() {
            eprintln!(
                "--- tmux ({})\n{}\n--- mirror\n{}",
                server
                    .format(target, "#{pane_width}x#{pane_height} region #{scroll_region_upper}-#{scroll_region_lower} cursor #{cursor_x},#{cursor_y}")
                    .unwrap_or_default(),
                t.join("\n"),
                o.join("\n")
            );
        }
        let first = t
            .iter()
            .zip(&o)
            .position(|(a, b)| a != b)
            .unwrap_or(t.len().min(o.len()));
        panic!(
            "history differs at line {first} (tmux {} lines, mirror {} lines)\n tmux:   {:?}\n mirror: {:?}",
            t.len(),
            o.len(),
            t.get(first..(first + 3).min(t.len())),
            o.get(first..(first + 3).min(o.len())),
        );
    }

    let tmux_visible = server
        .run_checked(&["capture-pane", "-p", "-S", "0", "-E", "-", "-t", target])
        .expect("capture visible");
    let frame = engine.frame().expect("frame");
    let ours_visible: Vec<String> = frame.rows.iter().map(|r| r.text()).collect();
    assert_eq!(
        trimmed_lines_with_tabs(&tmux_visible, &stops),
        trimmed_lines(&ours_visible.join("\n")),
        "visible rows differ"
    );

    let cursor = server
        .format(target, "#{cursor_x},#{cursor_y}")
        .expect("cursor");
    let (x, y) = engine.cursor_position().expect("cursor");
    assert_eq!(cursor, format!("{x},{y}"), "cursor differs");
}

fn start(cols: u16, rows: u16, script: &str) -> (TmuxServer, ControlClient, PaneId) {
    require_tmux_baseline();
    let server =
        TmuxServer::start(cols, rows, &format!("sh -c {}", sh_quote(script))).expect("start tmux");
    let client = ControlClient::attach(&server, "main").expect("attach");
    let pane = pane_id(&server, "main");
    (server, client, pane)
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn bootstrap(client: &mut ControlClient, pane: PaneId) -> Bootstrapped {
    bootstrap_pane(client, pane, TIMEOUT).expect("bootstrap")
}

fn report(name: &str, b: &Bootstrapped) {
    eprintln!(
        "[{name}] history={} visible={} alt={} pending={}B dropped_before_pause={}B continuation={}B generation={}",
        b.captures.history.len(),
        b.captures.visible.len(),
        b.state.alternate_on,
        b.captures.pending.len(),
        b.dropped_before_pause,
        b.continuation,
        b.state.output_generation,
    );
}

#[test]
fn idle_pane_with_styled_text() {
    let script = "printf 'hello \\033[1;34mworld\\033[0m\\nwide \\344\\270\\226\\347\\225\\214 ok\\nlast line'; sleep 60";
    let (server, mut client, pane) = start(30, 6, script);
    wait_for("output", || {
        server
            .run_checked(&["capture-pane", "-p", "-t", "main"])
            .is_ok_and(|s| s.contains("last line"))
    });
    let mut b = bootstrap(&mut client, pane);
    report("idle", &b);
    assert!(b.captures.pending.is_empty());
    assert_matches_tmux(&server, "main", &mut b.engine);
    let style = b.engine.frame().expect("frame").style_dump();
    assert!(style.contains("Palette(4)"), "{style}");
}

fn busy_script(lines: u32) -> String {
    format!(
        "i=0; while [ $i -lt {lines} ]; do i=$((i+1)); echo \"line $i\"; done; echo {DONE}; sleep 60"
    )
}

#[test]
fn busy_stream_bootstraps_without_loss_or_duplication() {
    for round in 0..5 {
        let (server, mut client, pane) = start(40, 10, &busy_script(15000));
        std::thread::sleep(Duration::from_millis(30 + 40 * round));
        let mut b = bootstrap(&mut client, pane);
        report(&format!("busy#{round}"), &b);
        follow_until(&mut client, &mut b.engine, pane, DONE);
        assert_matches_tmux(&server, "main", &mut b.engine);
    }
}

#[test]
fn busy_pane_next_to_another_busy_pane() {
    for round in 0..3 {
        let (server, mut client, pane) = start(60, 12, &busy_script(12000));
        server
            .run_checked(&[
                "split-window",
                "-h",
                "-t",
                "main",
                &format!("sh -c {}", sh_quote(&busy_script(40000))),
            ])
            .expect("split");
        std::thread::sleep(Duration::from_millis(40 + 30 * round));
        let mut b = bootstrap(&mut client, pane);
        report(&format!("two-busy#{round}"), &b);
        follow_until(&mut client, &mut b.engine, pane, DONE);
        assert_matches_tmux(&server, &pane.to_string(), &mut b.engine);
    }
}

#[test]
fn alternate_screen_is_restored_and_left_correctly() {
    let script = "printf 'normal one\\nnormal two\\n'; printf '\\033[?1049h\\033[2;3Halt content'; sleep 1.5; printf '\\033[5;1Hmore alt'; sleep 0.5; printf '\\033[?1049lafter alt DONE-7f3a'; sleep 60";
    let (server, mut client, pane) = start(30, 8, script);
    wait_for("alternate screen", || {
        server
            .format("main", "#{alternate_on}")
            .is_ok_and(|v| v == "1")
            && server
                .run_checked(&["capture-pane", "-p", "-t", "main"])
                .is_ok_and(|s| s.contains("alt content"))
    });
    let mut b = bootstrap(&mut client, pane);
    report("alt", &b);
    assert!(b.state.alternate_on);
    assert!(!b.captures.saved_normal.is_empty());
    assert_matches_tmux(&server, "main", &mut b.engine);
    follow_until(&mut client, &mut b.engine, pane, DONE);
    assert_matches_tmux(&server, "main", &mut b.engine);
}

#[test]
fn incomplete_escape_and_utf8_continue_correctly() {
    // The program stops in the middle of an escape until the test has
    // bootstrapped (the go file), so the partial escape is pending then
    // however slow the machine is.
    let go = std::env::temp_dir().join(format!("dreamer-bootstrap-go-{}", std::process::id()));
    let _ = std::fs::remove_file(&go);
    let script = format!(
        "printf 'before \\033[3'; while [ ! -e '{}' ]; do sleep 0.05; done; printf '1mred\\033[0m \\344'; sleep 0.3; printf '\\270\\226 DONE-7f3a'; sleep 60",
        go.display()
    );
    let (server, mut client, pane) = start(30, 4, &script);
    wait_for("partial output", || {
        server
            .run_checked(&["capture-pane", "-p", "-t", "main"])
            .is_ok_and(|s| s.contains("before"))
    });
    let mut b = bootstrap(&mut client, pane);
    report("incomplete", &b);
    std::fs::write(&go, b"").expect("go file");
    assert_eq!(b.captures.pending, b"\x1b[3");
    follow_until(&mut client, &mut b.engine, pane, DONE);
    assert_matches_tmux(&server, "main", &mut b.engine);
    let frame = b.engine.frame().expect("frame");
    assert_eq!(frame.rows[0].text(), "before red 世 DONE-7f3a");
    assert!(frame.style_dump().contains("Palette(1)"));
    let _ = std::fs::remove_file(&go);
}

#[test]
fn modes_scroll_region_and_tabs_are_restored() {
    let script = "printf '\\033[?1h\\033=\\033[?2004h\\033[?1000h\\033[?1006h\\033[4h\\033[?25l\\033[5 q\\033[3g\\033[5G\\033H\\033[13G\\033H\\033[2;5r\\033[3;1Hregion'; sleep 1.5; printf '\\r\\n\\ta\\tb\\n\\n\\n\\nscrolled DONE-7f3a'; sleep 60";
    let (server, mut client, pane) = start(30, 8, script);
    wait_for("modes", || {
        server
            .format("main", "#{bracket_paste_flag}")
            .is_ok_and(|v| v == "1")
    });
    let mut b = bootstrap(&mut client, pane);
    report("modes", &b);
    let e = &b.engine;
    assert!(e.mode(1, false).expect("decckm"));
    assert!(e.mode(66, false).expect("deckpam"));
    assert!(e.mode(2004, false).expect("bracketed paste"));
    assert!(e.mode(1000, false).expect("mouse"));
    assert!(e.mode(1006, false).expect("sgr mouse"));
    assert!(e.mode(4, true).expect("insert"));
    assert!(!e.mode(25, false).expect("cursor visible"));
    assert_eq!(b.state.scroll_region, (1, 4));
    assert_eq!(b.state.tabs, Some(vec![4, 12]));
    let frame = b.engine.frame().expect("frame");
    assert_eq!(frame.cursor.shape, CursorShape::Bar);
    assert!(frame.cursor.blinking);
    // Tabs, the scroll region, and insert mode all shape what follows.
    follow_until(&mut client, &mut b.engine, pane, DONE);
    assert_visible_matches_tmux(&server, "main", &mut b.engine);
}

#[test]
fn resize_just_before_bootstrap_uses_the_new_size() {
    let (server, mut client, pane) = start(40, 10, &busy_script(8000));
    std::thread::sleep(Duration::from_millis(50));
    server
        .run_checked(&["resize-window", "-t", "main", "-x", "33", "-y", "7"])
        .expect("resize");
    let mut b = bootstrap(&mut client, pane);
    report("resize", &b);
    assert_eq!((b.state.width, b.state.height), (33, 7));
    follow_until(&mut client, &mut b.engine, pane, DONE);
    assert_matches_tmux(&server, "main", &mut b.engine);
}

#[test]
fn failed_capture_resumes_the_pane() {
    let (server, mut client, pane) = start(30, 4, "echo hi; sleep 60");
    wait_for("output", || {
        server
            .run_checked(&["capture-pane", "-p", "-t", "main"])
            .is_ok_and(|s| s.contains("hi"))
    });
    let missing = PaneId(pane.0 + 999);
    let started = Instant::now();
    let err = bootstrap_pane(&mut client, missing, TIMEOUT).expect_err("missing pane");
    eprintln!("[missing] {err}");
    assert!(
        matches!(err, tmux_backend::BootstrapError::PaneNotFound(_)),
        "{err}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    // The real pane still bootstraps afterwards.
    let mut b = bootstrap(&mut client, pane);
    assert_matches_tmux(&server, "main", &mut b.engine);
}
