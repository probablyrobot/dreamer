//! Input through the daemon (spec 10.1): encoded against the pane's
//! canonical state and delivered to the real program in the pane.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use api::{Request, Response, terminal_input::Event};
use client::{Client, ClientError};
use tmux_backend::TmuxServer;
use tmux_backend::server::tmux_program;

const EXE: &str = env!("CARGO_BIN_EXE_dreamerd");

struct Setup {
    root: PathBuf,
    tmux: TmuxServer,
    daemon: Child,
}

impl Setup {
    fn new() -> Setup {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/drm-in-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let tmux = TmuxServer::start(100, 30, "/bin/sh").expect("tmux");
        tmux.run_checked(&["set-option", "-g", "default-shell", "/bin/sh"])
            .expect("shell");
        let daemon = Command::new(EXE)
            .args(["--background", "--tmux-socket", tmux.socket(), "--tmux"])
            .arg(tmux_program())
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dreamerd");
        Setup { root, tmux, daemon }
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

    fn type_line(&self, line: &str) {
        self.tmux
            .run_checked(&["send-keys", "-t", "main", "-l", line])
            .expect("type");
        self.tmux
            .run_checked(&["send-keys", "-t", "main", "Enter"])
            .expect("enter");
    }

    fn screen(&self) -> String {
        self.tmux
            .run_checked(&["capture-pane", "-p", "-t", "main"])
            .unwrap_or_default()
    }

    /// Wait until a screen line equals `line` exactly (the shell's echo of
    /// the command never does, because markers are split with quotes).
    fn wait_line(&self, line: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        // `od` spacing differs between platforms: compare words.
        let words = |l: &str| l.split_whitespace().collect::<Vec<_>>().join(" ");
        loop {
            let s = self.screen();
            if s.lines().any(|l| words(l) == line) {
                return s;
            }
            assert!(Instant::now() < deadline, "never showed {line:?}:\n{s}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until the daemon's mirror has a line equal to `line`: the
    /// mirror, not just tmux, must have seen what precedes it.
    async fn wait_mirror(&self, c: &Client, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(Response::TerminalRead(t)) = c
                .call(Request::TerminalRead(api::ReadParams {
                    pane: 0,
                    history: Some(200),
                    endpoint: None,
                }))
                .await
                && t.lines.iter().any(|l| l.trim_end() == line)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the mirror never showed {line:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
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

async fn input(
    c: &Client,
    event: Event,
    generation: Option<u64>,
) -> Result<api::InputResult, ClientError> {
    match c
        .call(Request::TerminalInput(api::TerminalInput {
            pane: 0,
            generation,
            endpoint: None,
            client: None,
            event: Some(event),
            lease: None,
        }))
        .await?
    {
        Response::TerminalInput(r) => Ok(r),
        other => panic!("{other:?}"),
    }
}

fn key(name: &str) -> Event {
    Event::Key(api::KeyInput {
        key: name.into(),
        ..api::KeyInput::default()
    })
}

#[tokio::test]
async fn keys_are_encoded_against_the_pane_s_current_modes() {
    let s = Setup::new();
    let c = s.client().await;
    // Normal cursor mode: the program reads three raw bytes and shows them.
    s.type_line("stty raw -echo; printf 'K''1\\r\\n'; od -An -tx1 -N3; stty sane; echo D''ONE1");
    s.wait_mirror(&c, "K1").await;
    input(&c, key("ArrowUp"), None).await.expect("key");
    s.wait_line("1b 5b 41");
    // Type the next command only once the terminal is back in normal mode.
    s.wait_line("DONE1");
    // The program switches to application cursor mode first.
    s.type_line(
        "printf '\\033[?1h'; stty raw -echo; printf 'K''2\\r\\n'; od -An -tx1 -N3; stty sane",
    );
    s.wait_mirror(&c, "K2").await;
    input(&c, key("ArrowUp"), None).await.expect("key");
    s.wait_line("1b 4f 41");
    let e = input(&c, key("NoSuchKey"), None)
        .await
        .expect_err("unknown key");
    assert!(
        matches!(e, ClientError::Rpc(ref r) if r.code == api::code::INVALID_PARAMS),
        "{e}"
    );
}

#[tokio::test]
async fn a_one_mib_paste_arrives_byte_for_byte() {
    let s = Setup::new();
    let c = s.client().await;
    const N: usize = 1 << 20;
    // Printable bytes only: paste neutralizes control sequences by design.
    let data: Vec<u8> = (0..N).map(|i| b'!' + (i * 7 % 90) as u8).collect();
    let out = s.root.join("paste.bin");
    s.type_line(&format!(
        "stty raw -echo; printf 'P''1\\r\\n'; head -c {N} > '{}'; stty sane; echo PAS''TED",
        out.display()
    ));
    s.wait_mirror(&c, "P1").await;
    // Larger than one request frame: the client sends it in parts and the
    // daemon encodes it as one paste.
    let r = client::paste(&c, 0, None, &data).await.expect("paste");
    assert_eq!(
        r.bytes as usize, N,
        "not bracketed: the program did not ask"
    );
    assert_eq!(r.batches, 64, "16 KiB per tmux batch");
    s.wait_line("PASTED");
    let got = std::fs::read(&out).expect("pasted file");
    assert_eq!(got.len(), N);
    assert!(got == data, "the paste was reordered or corrupted");
}

#[tokio::test]
async fn bracketed_paste_markers_appear_exactly_once() {
    let s = Setup::new();
    let c = s.client().await;
    let out = s.root.join("bracketed.bin");
    // 11 bytes of text plus the 6-byte start and end markers.
    s.type_line(&format!(
        "printf '\\033[?2004h'; stty raw -echo; printf 'B''1\\r\\n'; head -c 23 > '{}'; stty sane; printf '\\033[?2004l'; echo BDO''NE",
        out.display()
    ));
    s.wait_mirror(&c, "B1").await;
    client::paste(&c, 0, None, b"hello world")
        .await
        .expect("paste");
    s.wait_line("BDONE");
    assert_eq!(
        std::fs::read(&out).expect("file"),
        b"\x1b[200~hello world\x1b[201~"
    );
}

#[tokio::test]
async fn input_for_a_replaced_process_is_refused() {
    let s = Setup::new();
    let c = s.client().await;
    s.type_line("echo R''EADY");
    s.wait_mirror(&c, "READY").await;
    let Response::TerminalMirrors(m) = c
        .call(Request::TerminalMirrors(api::TopologyParams::default()))
        .await
        .expect("mirrors")
    else {
        panic!("mirrors")
    };
    let generation = m.mirrors[0].generation;
    input(
        &c,
        Event::Text(api::TextInput {
            text: "echo ok\r".into(),
        }),
        Some(generation),
    )
    .await
    .expect("current generation is accepted");
    s.tmux
        .run_checked(&["respawn-pane", "-k", "-t", "main", "/bin/sh"])
        .expect("respawn");
    // Wait until the daemon has the new process's mirror.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(Response::TerminalMirrors(m)) = c
            .call(Request::TerminalMirrors(api::TopologyParams::default()))
            .await
            && m.mirrors
                .iter()
                .any(|x| x.generation > generation && x.state == api::MirrorState::Live as i32)
        {
            break;
        }
        assert!(Instant::now() < deadline, "never saw the respawned mirror");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let e = input(
        &c,
        Event::Text(api::TextInput {
            text: "echo must-not-run\r".into(),
        }),
        Some(generation),
    )
    .await
    .expect_err("stale generation");
    assert!(
        matches!(e, ClientError::Rpc(ref r) if r.code == api::code::PRECONDITION),
        "{e}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!s.screen().contains("must-not-run"));
}

fn mouse(action: api::MouseAction, button: api::MouseButton, col: u32, row: u32) -> Event {
    Event::Mouse(api::MouseInput {
        action: action as i32,
        button: button as i32,
        col,
        row,
        ..api::MouseInput::default()
    })
}

#[tokio::test]
async fn mouse_reaches_only_programs_that_track_it() {
    use api::{MouseAction as A, MouseButton as B};
    let s = Setup::new();
    let c = s.client().await;
    s.type_line("echo M''0");
    s.wait_mirror(&c, "M0").await;
    let r = input(&c, mouse(A::Press, B::Left, 3, 1), None)
        .await
        .expect("mouse");
    assert_eq!(r.bytes, 0, "the shell did not ask for mouse reports");
    // Normal tracking with SGR coordinates; the program reads one press and
    // one wheel step (9 + 10 bytes).
    s.type_line(
        "printf '\\033[?1000h\\033[?1006h'; stty raw -echo; printf 'M''1\\r\\n'; head -c 19 | tr '\\033' E; printf '\\r\\n'; stty sane; printf '\\033[?1000l\\033[?1006l'",
    );
    s.wait_mirror(&c, "M1").await;
    input(&c, mouse(A::Press, B::Left, 3, 1), None)
        .await
        .expect("press");
    input(&c, mouse(A::Press, B::WheelDown, 0, 0), None)
        .await
        .expect("wheel");
    s.wait_line("E[<0;4;2ME[<65;1;1M");
    let e = input(&c, mouse(A::Unspecified, B::Left, 0, 0), None)
        .await
        .expect_err("no action");
    assert!(
        matches!(e, ClientError::Rpc(ref r) if r.code == api::code::INVALID_PARAMS),
        "{e}"
    );
}

async fn leased(c: &Client, lease: &str, text: &str) -> Result<api::InputResult, ClientError> {
    match c
        .call(Request::TerminalInput(api::TerminalInput {
            pane: 0,
            lease: Some(lease.into()),
            event: Some(Event::Text(api::TextInput { text: text.into() })),
            ..api::TerminalInput::default()
        }))
        .await?
    {
        Response::TerminalInput(r) => Ok(r),
        other => panic!("{other:?}"),
    }
}

/// Spec 12.4: automation types under a lease; a person typing into the
/// pane takes it over, and the automation's later input is refused instead
/// of landing in the middle of what the person is doing.
#[tokio::test]
async fn a_person_typing_takes_over_from_leased_automation() {
    let s = Setup::new();
    let agent = s.client().await;
    let person = s.client().await;
    s.type_line("echo R''EADY");
    s.wait_mirror(&agent, "READY").await;

    let Response::LeasesAcquire(lease) = agent
        .call(Request::LeasesAcquire(api::LeaseAcquire {
            pane: 0,
            ttl_ms: None,
        }))
        .await
        .expect("acquire")
    else {
        panic!("lease")
    };
    leased(&agent, &lease.id, "echo AUTO''MATED\r")
        .await
        .expect("leased input");
    s.wait_line("AUTOMATED");

    // Another connection cannot use or take the lease.
    assert!(leased(&person, &lease.id, "x").await.is_err());
    assert!(
        person
            .call(Request::LeasesAcquire(api::LeaseAcquire {
                pane: 0,
                ttl_ms: None,
            }))
            .await
            .is_err()
    );

    // The person types; the agent's next input is refused.
    input(
        &person,
        Event::Text(api::TextInput {
            text: "echo PER''SON\r".into(),
        }),
        None,
    )
    .await
    .expect("person");
    s.wait_line("PERSON");
    let e = leased(&agent, &lease.id, "echo must-not-run\r")
        .await
        .expect_err("revoked");
    assert!(
        matches!(e, ClientError::Rpc(ref r) if r.code == api::code::PRECONDITION
            && r.message.contains("a person typed")),
        "{e}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!s.screen().contains("must-not-run"));
}
