//! Stream-level tests: a recorded tmux 3.8 session and generated streams
//! must parse identically however the bytes are split into reads.
//!
//! Fixture provenance: `fixtures/tmux-3.8-session.bin` was recorded from
//! `tmux -C` 3.8 on an isolated `-L` socket with `-f /dev/null`, `env -i`,
//! and explicit pane commands (no user shell), so it contains only synthetic
//! content: a command response whose body reads `%begin ...`, pane output
//! containing `%%end`/`%%output` text, tabs, backslashes, SGR escapes, UTF-8,
//! a raw 0xFF byte, a split, a capture, and a failed command (`%error`).

use proptest::prelude::*;
use tmux_protocol::{Event, Guard, Notification, PaneId, Parser, Response, escape};

const SESSION: &[u8] = include_bytes!("fixtures/tmux-3.8-session.bin");

fn parse_in_chunks(input: &[u8], cuts: &[usize]) -> Vec<Event> {
    let mut parser = Parser::default();
    let mut out = Vec::new();
    let mut start = 0;
    for &cut in cuts.iter().chain(std::iter::once(&input.len())) {
        let cut = cut.clamp(start, input.len());
        parser.feed(&input[start..cut], &mut out).expect("feed");
        start = cut;
    }
    parser.finish().expect("finish");
    out
}

fn outputs(events: &[Event], pane: PaneId) -> Vec<u8> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Notification(Notification::Output { pane: p, data }) if *p == pane => {
                Some(data.as_slice())
            }
            _ => None,
        })
        .flatten()
        .copied()
        .collect()
}

#[test]
fn recorded_session_parses_to_the_expected_events() {
    let events = parse_in_chunks(SESSION, &[]);

    assert_eq!(
        outputs(&events, PaneId(0)),
        b"hello\tworld \\ back\x1b[1;31mred\x1b[0m \xc3\xa9 \xff\r\n%%output fake\r\n"
    );
    assert_eq!(outputs(&events, PaneId(1)), b"%%end 1 1 1 lookalike\r\n");

    let responses: Vec<&Response> = events
        .iter()
        .filter_map(|e| match e {
            Event::Response(r) => Some(r),
            Event::Notification(_) => None,
        })
        .collect();
    assert_eq!(responses.len(), 7);
    // 313 attach, 315 display, 316 split, 317 list, 318 capture, 319 bogus, 320 kill.
    for (i, r) in responses.iter().enumerate() {
        assert_eq!(r.ok, i != 5, "response {i}");
    }
    assert_eq!(responses[1].body, [b"%begin 1 2 3 literal".to_vec()]);
    assert_eq!(
        responses[3].body,
        [b"%0 20x6".to_vec(), b"%1 19x6".to_vec()]
    );
    assert!(!responses[5].ok);
    assert_eq!(
        responses[5].body,
        [b"parse error: unknown command: bogus-command".to_vec()]
    );

    assert!(
        events.contains(&Event::Notification(Notification::WindowPaneChanged {
            window: tmux_protocol::WindowId(0),
            pane: PaneId(1),
        }))
    );
    assert_eq!(
        events.last(),
        Some(&Event::Notification(Notification::Exit { reason: None }))
    );
}

#[test]
fn recorded_session_parses_identically_at_every_split_point() {
    let whole = parse_in_chunks(SESSION, &[]);
    for cut in 0..=SESSION.len() {
        assert_eq!(parse_in_chunks(SESSION, &[cut]), whole, "split at {cut}");
    }
    let bytewise: Vec<usize> = (1..SESSION.len()).collect();
    assert_eq!(parse_in_chunks(SESSION, &bytewise), whole);
}

/// A model item and the bytes tmux would send for it.
#[derive(Debug, Clone)]
enum Item {
    Output(u32, Vec<u8>),
    Response(u64, bool, Vec<Vec<u8>>),
    Note(Vec<u8>),
}

fn item() -> impl Strategy<Value = Item> {
    let body_line = prop_oneof![
        proptest::collection::vec(
            any::<u8>().prop_filter("no newline", |b| *b != b'\n'),
            0..40
        ),
        Just(b"%end 1 1 1".to_vec()),
        Just(b"%error 9 9 9".to_vec()),
        Just(b"%output %1 not output".to_vec()),
        Just(b"%begin 2 2 2".to_vec()),
        Just(b"%exit".to_vec()),
    ];
    prop_oneof![
        (0u32..50, proptest::collection::vec(any::<u8>(), 0..200))
            .prop_map(|(p, d)| Item::Output(p, d)),
        (
            any::<u64>(),
            any::<bool>(),
            proptest::collection::vec(body_line, 0..6)
        )
            .prop_map(|(n, ok, body)| Item::Response(n, ok, body)),
        prop_oneof![
            Just(b"%sessions-changed".to_vec()),
            Just(b"%window-add @3".to_vec()),
            Just(b"%layout-change @1 abcd,80x24,0,0,2 abcd,80x24,0,0,2 *".to_vec()),
            Just(b"%some-future-notification x y".to_vec()),
        ]
        .prop_map(Item::Note),
    ]
}

fn render(items: &[Item]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Item::Output(pane, data) => {
                out.extend_from_slice(format!("%output %{pane} ").as_bytes());
                out.extend_from_slice(&escape(data));
                out.push(b'\n');
            }
            Item::Response(number, ok, body) => {
                // Use a time no body line can match, so lookalike guards in
                // the body never close the block (as with real tmux times).
                out.extend_from_slice(format!("%begin 1700000000 {number} 1\n").as_bytes());
                for line in body {
                    out.extend_from_slice(line);
                    out.push(b'\n');
                }
                let end = if *ok { "%end" } else { "%error" };
                out.extend_from_slice(format!("{end} 1700000000 {number} 1\n").as_bytes());
            }
            Item::Note(line) => {
                out.extend_from_slice(line);
                out.push(b'\n');
            }
        }
    }
    out
}

proptest! {
    #[test]
    fn generated_streams_survive_arbitrary_splits(
        items in proptest::collection::vec(item(), 0..30),
        mut cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..20),
    ) {
        let bytes = render(&items);
        let mut cuts: Vec<usize> = cuts.drain(..).map(|i| i.index(bytes.len() + 1)).collect();
        cuts.sort_unstable();
        let events = parse_in_chunks(&bytes, &cuts);
        prop_assert_eq!(events.len(), items.len());
        for (event, item) in events.iter().zip(&items) {
            match (event, item) {
                (Event::Notification(Notification::Output { pane, data }), Item::Output(p, d)) => {
                    prop_assert_eq!(pane.0, *p);
                    prop_assert_eq!(data, d);
                }
                (Event::Response(r), Item::Response(n, ok, body)) => {
                    prop_assert_eq!(r.guard, Guard { time: 1_700_000_000, number: *n, flags: 1 });
                    prop_assert_eq!(r.ok, *ok);
                    prop_assert_eq!(&r.body, body);
                }
                (Event::Notification(n), Item::Note(_)) => {
                    let is_output = matches!(n, Notification::Output { .. });
                    prop_assert!(!is_output);
                }
                (e, i) => prop_assert!(false, "event {:?} for item {:?}", e, i),
            }
        }
    }
}
