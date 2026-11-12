use prost::Message as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
use wire::io::{HandshakeError, READ_CHUNK, Reader, client_handshake, server_handshake};
use wire::{
    Bulk, Decoder, HEADER_LEN, Hello, Identity, Item, Kind, Limits, Outbox, RejectReason,
    WireError, encode_frame, preamble, welcome,
};

fn identity(role: &str) -> Identity {
    Identity::current(role)
}

async fn run_server(
    stream: tokio::io::DuplexStream,
    me: Identity,
) -> Result<(Hello, wire::Negotiated), HandshakeError> {
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = Reader::new(r, me.limits);
    let me2 = me.clone();
    server_handshake(&mut reader, &mut w, &me, move |n| {
        welcome(&me2, n, "instance-1", 7, "host", "default")
    })
    .await
}

#[tokio::test]
async fn a_matching_peer_is_welcomed_with_the_smaller_limits() {
    let (c, s) = duplex(1 << 16);
    let mut server_id = identity("daemon");
    server_id.limits.max_frame = 256 << 10;
    let server = tokio::spawn(run_server(s, server_id));
    let (r, mut w) = tokio::io::split(c);
    let me = identity("cli");
    let mut reader = Reader::new(r, me.limits);
    let (welcome, limits) = client_handshake(&mut reader, &mut w, &me)
        .await
        .expect("welcome");
    assert_eq!(welcome.instance, "instance-1");
    assert_eq!(welcome.epoch, 7);
    assert_eq!(limits.max_frame, 256 << 10);
    let (hello, negotiated) = server.await.expect("join").expect("accepted");
    assert_eq!(hello.role, "cli");
    assert_eq!(negotiated.limits.max_frame, 256 << 10);
}

#[tokio::test]
async fn another_major_version_is_refused_from_its_preamble_alone() {
    let (mut c, s) = duplex(1 << 16);
    let server = tokio::spawn(run_server(s, identity("daemon")));
    // Major 2, then bytes that are not a valid frame or record at all. A
    // server that decoded anything past the preamble would fail on them
    // with a decode error instead of rejecting the version.
    let mut p = preamble();
    p[4..6].copy_from_slice(&2u16.to_be_bytes());
    c.write_all(&p).await.expect("write");
    c.write_all(&[0xff; 64]).await.expect("write");
    match server.await.expect("join") {
        Err(HandshakeError::Rejected(r)) => {
            assert_eq!(r.reason, RejectReason::IncompatibleProtocol as i32);
            assert!(r.message.contains("dream restart"), "{}", r.message);
        }
        other => panic!("expected a protocol rejection, got {other:?}"),
    }
    // The client gets the server's preamble and a Reject frame.
    let mut buf = vec![0; 8];
    c.read_exact(&mut buf).await.expect("preamble");
    assert_eq!(&buf[..4], b"DRMR");
}

#[tokio::test]
async fn a_client_refuses_a_server_of_another_major_before_decoding_it() {
    let (c, mut s) = duplex(1 << 16);
    let fake = tokio::spawn(async move {
        let mut p = preamble();
        p[4..6].copy_from_slice(&9u16.to_be_bytes());
        s.write_all(&p).await.expect("write");
        s.write_all(&[0xee; 64]).await.expect("write");
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink).await;
    });
    let (r, mut w) = tokio::io::split(c);
    let me = identity("cli");
    let mut reader = Reader::new(r, me.limits);
    let err = client_handshake(&mut reader, &mut w, &me)
        .await
        .expect_err("refused");
    assert_eq!(err, HandshakeError::Protocol { major: 9, minor: 0 });
    drop((reader, w));
    fake.await.expect("join");
}

#[tokio::test]
async fn another_build_family_is_refused_and_nothing_after_hello_is_read() {
    let (c, s) = duplex(1 << 16);
    let server = tokio::spawn(run_server(s, identity("daemon")));
    let (r, mut w) = tokio::io::split(c);
    let mut me = identity("cli");
    me.build_family = "ghostty-9.9.9-000000000000-00000000-p1".into();
    let mut reader = Reader::new(r, me.limits);
    match client_handshake(&mut reader, &mut w, &me).await {
        Err(HandshakeError::Rejected(r)) => {
            assert_eq!(r.reason, RejectReason::IncompatibleBuild as i32);
            assert_eq!(r.build_family, terminal_core::family::build_family());
            assert!(r.pid > 0);
        }
        other => panic!("expected a build rejection, got {other:?}"),
    }
    assert!(matches!(
        server.await.expect("join"),
        Err(HandshakeError::Rejected(_))
    ));
}

#[test]
fn foreign_protocols_are_refused_at_the_first_bytes() {
    for input in [
        &b"GET / HTTP/1.1\r\n"[..],
        b"SSH-2.0-OpenSSH",
        b"\x16\x03\x01",
        b"D",
    ] {
        let mut d = Decoder::new(Limits::default());
        d.feed(input);
        let r = d.next_item();
        if input == b"D" {
            assert_eq!(r, Ok(None), "a prefix of the magic waits for more");
        } else {
            assert_eq!(r, Err(WireError::NotDreamer), "{input:?}");
        }
    }
}

/// Field numbers and framing are permanent: these bytes must not change.
#[test]
fn encodings_are_frozen() {
    let hello = Hello {
        build_family: "f".into(),
        software_version: "v".into(),
        role: "cli".into(),
        features: vec!["x".into()],
        max_frame: 4096,
        max_message: 65536,
        pid: 42,
    };
    let full: String = hello
        .encode_to_vec()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        full, "0a01661201761a03636c6922017828802030808004382a",
        "Hello field layout changed; see docs/wire.md"
    );
    let frame = encode_frame(Kind::Request, 0, b"ab", 16).expect("frame");
    assert_eq!(frame, [0, 0, 0, 2, 4, 0, b'a', b'b']);
    assert_eq!(preamble(), [b'D', b'R', b'M', b'R', 0, 1, 0, 0]);
}

/// xorshift64*: deterministic, so a failure reproduces from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn random_stream(rng: &mut Rng, limits: &Limits) -> Vec<u8> {
    let mut s = preamble().to_vec();
    if rng.below(10) == 0 {
        s[rng.below(8) as usize] ^= 1 << rng.below(8);
    }
    for _ in 0..rng.below(40) {
        match rng.below(8) {
            // A valid small frame of a random kind.
            0..=2 => {
                let n = rng.below(u64::from(limits.max_frame) + 1) as usize;
                let kind =
                    [Kind::Request, Kind::Response, Kind::Event, Kind::Ping][rng.below(4) as usize];
                s.extend(encode_frame(kind, 0, &rng.bytes(n), limits.max_frame).expect("frame"));
            }
            // A header claiming a huge payload that never arrives.
            3 => {
                let len = (rng.next() as u32) | 0x8000_0000;
                s.extend_from_slice(&len.to_be_bytes());
                s.extend_from_slice(&[Kind::Bulk as u8, 0]);
            }
            // A flood of bulk continuation chunks.
            4 => {
                for _ in 0..rng.below(64) {
                    let n = rng.below(u64::from(limits.max_frame) + 1) as usize;
                    s.extend(
                        encode_frame(Kind::Bulk, wire::FLAG_MORE, &rng.bytes(n), limits.max_frame)
                            .expect("chunk"),
                    );
                }
            }
            // Raw garbage.
            5 => {
                let n = rng.below(256) as usize;
                s.extend(rng.bytes(n));
            }
            // A frame with an unknown kind or stray flags.
            6 => {
                let n = rng.below(64) as u32;
                s.extend_from_slice(&n.to_be_bytes());
                s.extend_from_slice(&[rng.next() as u8, rng.next() as u8]);
                s.extend(rng.bytes(n as usize));
            }
            // Bit flips in what is there.
            _ => {
                if !s.is_empty() {
                    let i = rng.below(s.len() as u64) as usize;
                    s[i] ^= 1 << rng.below(8);
                }
            }
        }
    }
    s
}

#[test]
fn fuzzed_streams_never_hold_more_than_the_limits_allow() {
    let limits = Limits {
        max_frame: 4096,
        max_message: 16 << 10,
        bulk_chunk: 1024,
        ..Limits::default()
    };
    // One partial frame plus one read, or one bulk message; doubling growth
    // may at most double each.
    let bound =
        2 * (HEADER_LEN + limits.max_frame as usize + READ_CHUNK) + 2 * limits.max_message as usize;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut frames = 0usize;
    let mut errors = 0usize;
    for _ in 0..2000 {
        let stream = random_stream(&mut rng, &limits);
        let mut d = Decoder::new(limits);
        let mut at = 0;
        'stream: while at < stream.len() {
            let n = (1 + rng.below(READ_CHUNK as u64) as usize).min(stream.len() - at);
            d.feed(&stream[at..at + n]);
            at += n;
            loop {
                assert!(
                    d.held_capacity() <= bound,
                    "decoder holds {} > {bound}",
                    d.held_capacity()
                );
                match d.next_item() {
                    Ok(Some(Item::Frame { kind, payload })) => {
                        frames += 1;
                        assert!(payload.len() <= limits.max_message as usize);
                        // Records decode from bounded payloads or fail cleanly.
                        let _ = wire::Request::decode(payload.as_slice());
                        let _ = wire::Response::decode(payload.as_slice());
                        let _ = Hello::decode(payload.as_slice());
                        if kind == Kind::Bulk {
                            let _ = Bulk::decode(payload.as_slice());
                        }
                    }
                    Ok(Some(Item::Preamble { .. })) => {}
                    Ok(None) => break,
                    Err(_) => {
                        errors += 1;
                        assert!(
                            d.held_capacity() == 0,
                            "a failed decoder releases its buffers"
                        );
                        break 'stream;
                    }
                }
            }
        }
    }
    // The generator exercises both outcomes.
    assert!(
        frames > 1000 && errors > 1000,
        "frames {frames}, errors {errors}"
    );
}

#[test]
fn priority_frames_overtake_a_large_transfer_and_bulk_reassembles() {
    let limits = Limits {
        max_frame: 8192,
        max_message: 1 << 20,
        bulk_chunk: 4096,
        max_priority_queued: 64 << 10,
        max_bulk_queued: 2 << 20,
    };
    let mut out = Outbox::new(limits);
    let big = Bulk {
        stream: 1,
        topic: "checkpoint".into(),
        data: (0..900_000u32).map(|i| i as u8).collect(),
    }
    .encode_to_vec();
    out.push_bulk(big.clone()).expect("bulk");
    let mut wire_bytes = preamble().to_vec();
    for _ in 0..3 {
        wire_bytes.extend(out.next_frame().expect("chunk"));
    }
    out.push_priority(Kind::Request, b"input: q")
        .expect("priority");
    let next = out.next_frame().expect("frame");
    assert_eq!(
        next[4],
        Kind::Request as u8,
        "the request goes before the next chunk"
    );
    wire_bytes.extend(next);
    while let Some(f) = out.next_frame() {
        assert!(f.len() <= HEADER_LEN + 4096);
        wire_bytes.extend(f);
    }
    let mut d = Decoder::new(limits);
    d.feed(&wire_bytes);
    let mut seen = Vec::new();
    while let Some(item) = d.next_item().expect("valid") {
        if let Item::Frame { kind, payload } = item {
            seen.push((kind, payload));
        }
    }
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], (Kind::Request, b"input: q".to_vec()));
    assert_eq!(seen[1].0, Kind::Bulk);
    assert_eq!(seen[1].1, big);
}

#[test]
fn full_queues_push_back_instead_of_growing() {
    let limits = Limits {
        max_frame: 1024,
        max_message: 4096,
        bulk_chunk: 512,
        max_priority_queued: 2048,
        max_bulk_queued: 8192,
    };
    let mut out = Outbox::new(limits);
    assert_eq!(
        out.push_priority(Kind::Event, &[0; 2000]),
        Err(WireError::FrameTooLarge {
            len: 2000,
            max: 1024
        })
    );
    out.push_priority(Kind::Event, &[0; 1000]).expect("fits");
    out.push_priority(Kind::Event, &[0; 1000]).expect("fits");
    assert_eq!(
        out.push_priority(Kind::Event, &[0; 100]),
        Err(WireError::Full)
    );
    assert_eq!(
        out.push_bulk(vec![0; 5000]),
        Err(WireError::MessageTooLarge { max: 4096 })
    );
    out.push_bulk(vec![0; 4096]).expect("fits");
    out.push_bulk(vec![0; 4096]).expect("fits");
    assert_eq!(out.push_bulk(vec![0; 1]), Err(WireError::Full));
    while out.next_frame().is_some() {}
    assert!(out.is_empty());
    assert_eq!(out.queued_bytes(), (0, 0));
    out.push_bulk(vec![0; 4096]).expect("room again");
}
