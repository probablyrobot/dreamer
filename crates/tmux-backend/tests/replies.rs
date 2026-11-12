//! Reply ownership through tmux (spec section 10.3).
//!
//! An application inside a tmux pane sends terminal queries while a Dreamer
//! mirror of that pane is live. tmux answers the queries it owns; the mirror
//! engine never sends anything back. The application must see exactly one
//! answer per query. A second engine with reply capture enabled shows what
//! Ghostty would have answered, which is why suppression is required.
//!
//! OSC 10/11 (foreground/background color) are the delegated case: tmux only
//! answers after a control client has reported the colors with
//! `refresh-client -r`, and then answers exactly once.

use std::path::Path;
use std::time::{Duration, Instant};

use terminal_ghostty::core::GridSize;
use terminal_ghostty::{Engine, EngineConfig};
use tmux_backend::{ControlClient, TmuxServer, bootstrap_pane, quote};
use tmux_protocol::{Event, Notification, PaneId};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Query {
    name: &'static str,
    bytes: &'static [u8],
}

const QUERIES: &[Query] = &[
    Query {
        name: "DA1",
        bytes: b"\x1b[c",
    },
    Query {
        name: "DA2",
        bytes: b"\x1b[>c",
    },
    Query {
        name: "DSR 6 (cursor position)",
        bytes: b"\x1b[6n",
    },
    Query {
        name: "DSR 5 (status)",
        bytes: b"\x1b[5n",
    },
    Query {
        name: "OSC 10 (foreground)",
        bytes: b"\x1b]10;?\x1b\\",
    },
    Query {
        name: "OSC 11 (background)",
        bytes: b"\x1b]11;?\x1b\\",
    },
];

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    (0..s.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Number of complete replies (each starts with ESC).
fn count_replies(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b == 0x1b).count()
        - bytes.windows(2).filter(|w| w == b"\x1b\\").count()
}

struct Outcome {
    /// What the application received for each query.
    received: Vec<Vec<u8>>,
    /// What a reply-capturing Ghostty engine would have produced.
    ghostty_would_send: Vec<u8>,
}

fn run_probe(report_colors: bool) -> Outcome {
    let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/reply_probe.py");
    let queries: Vec<String> = QUERIES.iter().map(|q| hex(q.bytes)).collect();
    let script = format!(
        "printf ready; python3 {} {}",
        probe.display(),
        queries.join(" ")
    );
    let server = TmuxServer::start(200, 20, &script).expect("tmux");
    let mut client = ControlClient::attach(&server, "main").expect("attach");
    let pane_text = server.format("main", "#{pane_id}").expect("pane");
    let pane = PaneId::parse(pane_text.as_bytes()).expect("pane id");

    // Wait until the probe is waiting for its go key.
    let deadline = Instant::now() + TIMEOUT;
    while !server
        .run_checked(&["capture-pane", "-p", "-t", "main"])
        .is_ok_and(|s| s.contains("ready"))
    {
        assert!(Instant::now() < deadline, "probe did not start");
        std::thread::sleep(Duration::from_millis(20));
    }

    // A live mirror with replies suppressed (the production default), and a
    // shadow engine that records what Ghostty itself would answer.
    let mut boot = bootstrap_pane(&mut client, pane, TIMEOUT).expect("bootstrap");
    let mut shadow_config = EngineConfig::new(GridSize {
        cols: 200,
        rows: 20,
    });
    shadow_config.capture_replies = true;
    let mut shadow = Engine::new(shadow_config).expect("shadow");
    let mut ghostty_would_send = Vec::new();

    if report_colors {
        // The GUI's colors, delivered as the reply a real terminal would send.
        let report = format!("{pane}:\x1b]11;rgb:1414/1616/1a1a\x1b\\");
        client
            .command(
                &format!("refresh-client -r {}", quote(&report)),
                TIMEOUT,
                |_| {},
            )
            .expect("report background");
        let report = format!("{pane}:\x1b]10;rgb:e5e5/e7e7/ebeb\x1b\\");
        client
            .command(
                &format!("refresh-client -r {}", quote(&report)),
                TIMEOUT,
                |_| {},
            )
            .expect("report foreground");
    }

    server
        .run_checked(&["send-keys", "-t", "main", "-l", "g"])
        .expect("go");
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = match client.recv(left) {
            Ok(event) => event,
            Err(e) => panic!(
                "probe did not finish ({e}); pane shows:\n{}",
                server
                    .run_checked(&["capture-pane", "-p", "-J", "-t", "main"])
                    .unwrap_or_default()
            ),
        };
        if let Event::Notification(Notification::Output { pane: p, data }) = event
            && p == pane
        {
            let fx = boot.engine.feed(&data).expect("mirror");
            assert!(
                fx.replies.is_empty(),
                "the mirror must never produce replies"
            );
            for (_, bytes) in shadow.feed(&data).expect("shadow").replies {
                ghostty_would_send.extend(bytes);
            }
            seen.extend_from_slice(&data);
            if seen.windows(10).any(|w| w == b"PROBE-DONE") {
                break;
            }
        }
    }

    let text = server
        .run_checked(&["capture-pane", "-p", "-J", "-S", "-", "-t", "main"])
        .expect("capture");
    let mut received = vec![Vec::new(); QUERIES.len()];
    for line in text.lines() {
        let Some(at) = line.find("REPLY ") else {
            continue;
        };
        let mut parts = line[at..].split_whitespace();
        if parts.next() == Some("REPLY")
            && let (Some(i), Some(h)) = (parts.next(), parts.next())
            && let Ok(i) = i.parse::<usize>()
        {
            received[i] = unhex(h);
        }
    }
    Outcome {
        received,
        ghostty_would_send,
    }
}

fn describe(outcome: &Outcome, label: &str) {
    for (q, got) in QUERIES.iter().zip(&outcome.received) {
        eprintln!(
            "[{label}] {:<26} replies={} {:?}",
            q.name,
            count_replies(got),
            String::from_utf8_lossy(got)
        );
    }
    eprintln!(
        "[{label}] a reply-capturing Ghostty engine would have sent {:?}",
        String::from_utf8_lossy(&outcome.ghostty_would_send)
    );
}

#[test]
fn queries_get_exactly_one_reply_and_the_mirror_sends_none() {
    let outcome = run_probe(false);
    describe(&outcome, "no colors reported");
    for (q, got) in QUERIES.iter().zip(&outcome.received).take(4) {
        assert_eq!(
            count_replies(got),
            1,
            "{}: {:?}",
            q.name,
            String::from_utf8_lossy(got)
        );
    }
    // Without reported colors tmux has nothing to answer OSC 10/11 with; the
    // application must not receive a second answer from anywhere.
    for (q, got) in QUERIES.iter().zip(&outcome.received).skip(4) {
        assert!(
            count_replies(got) <= 1,
            "{}: {:?}",
            q.name,
            String::from_utf8_lossy(got)
        );
    }
    // Ghostty would have answered too: suppression is what prevents doubles.
    assert!(
        !outcome.ghostty_would_send.is_empty(),
        "expected Ghostty to generate replies to these queries"
    );
}

#[test]
fn reported_colors_answer_osc_10_and_11_exactly_once() {
    let outcome = run_probe(true);
    describe(&outcome, "colors reported");
    for (q, got) in QUERIES.iter().zip(&outcome.received) {
        assert_eq!(
            count_replies(got),
            1,
            "{}: {:?}",
            q.name,
            String::from_utf8_lossy(got)
        );
    }
    let bg = String::from_utf8_lossy(&outcome.received[5]).into_owned();
    assert!(bg.contains("1414/1616/1a1a"), "background reply {bg:?}");
    let fg = String::from_utf8_lossy(&outcome.received[4]).into_owned();
    assert!(fg.contains("e5e5/e7e7/ebeb"), "foreground reply {fg:?}");
}
