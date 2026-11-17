//! The gate's live preview check (`preview` step, macOS): a template app
//! previewed in the running desktop (`dream app preview --open`), drawn
//! as an image by the desktop (`--png`) in both of Dreamer's schemes, and
//! a save to its `app.ts` followed by the pane within a second.
//!
//! Everything runs against an isolated tmux server, private daemon
//! directories, and a temporary configuration (the reference scenes'
//! [`crate::scenes::launch`]).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::scenes::{Running, Scene};

/// What the check found.
pub enum Outcome {
    Passed(Vec<String>),
    /// It could not be judged here, and why.
    NotRun(String),
}

/// One window with a shell; the preview opens in a window of its own.
const SCENE: Scene = Scene {
    name: "app-preview",
    what: "a live preview of a template app",
    setup: &[],
    type_all: None,
    args: &[],
    after: &[],
    app: None,
};

/// The longest a save may take to show in the preview pane.
const RELOAD_BUDGET: Duration = Duration::from_secs(1);

/// Run the check from the repository (or gate worktree) at `root`, with
/// the images in `out`.
pub fn check(root: &Path, out: &Path) -> Result<Outcome, String> {
    crate::scenes::build(root)?;
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut said = Vec::new();
    // Per scheme: background, and the chrome's surface (an app pane's).
    for (skin, background, surface) in [
        ("dark", 0x14_16_1a, 0x10_1216),
        ("light", 0xf6_f6_f4, 0xec_eb_e7),
    ] {
        let running = crate::scenes::launch(root, &SCENE, skin, "")?;
        let result = in_scene(
            root,
            &running,
            skin,
            [background, surface],
            out,
            skin == "dark",
        );
        running.finish();
        match result? {
            Outcome::Passed(mut lines) => said.append(&mut lines),
            not_run => return Ok(not_run),
        }
    }
    Ok(Outcome::Passed(said))
}

/// `dream` with the scene's private directories.
struct Dream {
    program: PathBuf,
    env: Vec<(&'static str, PathBuf)>,
    tmux: Vec<String>,
}

impl Dream {
    fn new(root: &Path, running: &Running) -> Dream {
        Dream {
            program: root.join("target/debug/dream"),
            env: vec![
                ("DREAMER_RUNTIME_DIR", running.dir.join("rt")),
                ("DREAMER_STATE_DIR", running.dir.join("st")),
                ("DREAMER_CONFIG_DIR", running.dir.join("config")),
            ],
            tmux: vec![
                "-L".into(),
                running.socket.clone(),
                "--tmux".into(),
                running.tmux.clone(),
            ],
        }
    }

    fn command(&self, args: &[&str], on_tmux: bool) -> Command {
        let mut c = Command::new(&self.program);
        c.args(args)
            .envs(self.env.iter().map(|(k, v)| (*k, v.as_os_str())))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1");
        if on_tmux {
            c.args(&self.tmux);
        }
        c
    }

    /// Run it; standard output, or standard error as the error.
    fn run(&self, args: &[&str], on_tmux: bool) -> Result<String, String> {
        let out = self
            .command(args, on_tmux)
            .output()
            .map_err(|e| format!("dream: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    }
}

/// A `dream rpc` session: one JSON-RPC call after another.
struct Rpc {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
    next: u64,
}

impl Rpc {
    fn start(dream: &Dream) -> Result<Rpc, String> {
        let mut child = dream
            .command(&["rpc"], true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("dream rpc: {e}"))?;
        let out = BufReader::new(child.stdout.take().ok_or("dream rpc: no output")?);
        Ok(Rpc {
            child,
            out,
            next: 1,
        })
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let stdin = self.child.stdin.as_mut().ok_or("dream rpc: no input")?;
        writeln!(stdin, "{line}").map_err(|e| format!("dream rpc: {e}"))?;
        loop {
            let mut reply = String::new();
            if self.out.read_line(&mut reply).map_err(|e| e.to_string())? == 0 {
                return Err("dream rpc ended".into());
            }
            let v: Value = serde_json::from_str(&reply).map_err(|e| format!("{e}: {reply}"))?;
            if v["id"] != json!(id) {
                continue;
            }
            if let Some(e) = v.get("error") {
                return Err(format!("{method}: {e}"));
            }
            return Ok(v["result"].clone());
        }
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn number(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

/// The preview check in one launched scene.
fn in_scene(
    root: &Path,
    running: &Running,
    skin: &str,
    theme: [u32; 2],
    out: &Path,
    reload: bool,
) -> Result<Outcome, String> {
    let rt = running.dir.join("rt");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !rt.join("dreamerd.sock").exists() {
        if Instant::now() > deadline {
            return Err("the scene's daemon did not start".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let dream = Dream::new(root, running);
    let app = running.dir.join("app");
    let app_dir = app.display().to_string();
    dream.run(
        &[
            "app",
            "new",
            "gate",
            "--template",
            "table",
            "--dir",
            &app_dir,
        ],
        false,
    )?;
    let opened = dream.run(&["app", "preview", &app_dir, "--open"], true)?;
    let pane: u64 = opened
        .split_whitespace()
        .find_map(|w| w.strip_prefix('%'))
        .and_then(|p| p.trim_end_matches(';').parse().ok())
        .ok_or_else(|| format!("no pane in `{}`", opened.trim()))?;

    // The desktop draws the preview pane, and gives its image.
    let shot = out.join(format!("app-preview-{skin}.png"));
    let shot_path = shot.display().to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match dream.run(&["app", "preview", &app_dir, "--png", &shot_path], true) {
            Ok(_) => break,
            // The desktop has not connected yet.
            Err(e) if e.contains("no Dreamer window is running") && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) if e.contains("could not be captured") => {
                return Ok(Outcome::NotRun(format!(
                    "screen capture is not allowed for this terminal (System Settings > Privacy \
                     & Security > Screen & System Audio Recording): {e}"
                )));
            }
            Err(e) if e.contains("did not draw version") => {
                return Ok(Outcome::NotRun(format!(
                    "the window stopped repainting (screen locked, or not in front): {e}"
                )));
            }
            Err(e) => return Err(format!("dream app preview --png: {e}")),
        }
    }
    let mut said = vec![format!(
        "preview: {} ({})",
        shot.display(),
        themed(&shot, theme)?
    )];
    if !reload {
        return Ok(Outcome::Passed(said));
    }

    // A save to app.ts: the host's tree has it, then the desktop's frame.
    let mut rpc = Rpc::start(&dream)?;
    let mut tree = || -> Result<(u64, String), String> {
        let t = rpc.call(
            "apps.tree",
            json!({ "pane": pane, "after": 0, "seq": 0, "wait_ms": 0 }),
        )?;
        Ok((
            number(&t["seq"]),
            t["tree"].as_str().unwrap_or_default().to_owned(),
        ))
    };
    let (seq, _) = tree()?;
    drop(tree);
    let capture = |rpc: &mut Rpc, path: &Path, seq: u64| -> Result<Duration, String> {
        let at = Instant::now();
        rpc.call(
            "apps.capture",
            json!({ "pane": pane, "path": path.display().to_string(), "seq": seq, "wait_ms": 10_000 }),
        )?;
        Ok(at.elapsed())
    };
    // What a capture of what is drawn already takes.
    let baseline = capture(&mut rpc, &out.join("app-preview-baseline.png"), seq)?;
    let source = app.join("app.ts");
    let ts = std::fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
    let saved = ts.replace(r#"title: "Panes""#, r#"title: "Panes after save""#);
    if saved == ts {
        return Err("the table template has no `title: \"Panes\"` to change".into());
    }
    std::fs::write(&source, saved).map_err(|e| format!("{}: {e}", source.display()))?;
    let at = Instant::now();
    let reloaded = loop {
        let t = rpc.call(
            "apps.tree",
            json!({ "pane": pane, "after": 0, "seq": 0, "wait_ms": 0 }),
        )?;
        if t["tree"]
            .as_str()
            .is_some_and(|t| t.contains("Panes after save"))
        {
            break number(&t["seq"]);
        }
        if at.elapsed() > Duration::from_secs(10) {
            return Err("the preview's host did not take the save within 10 s".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let host = at.elapsed();
    let after = out.join(format!("app-preview-saved-{skin}.png"));
    let drawn = match capture(&mut rpc, &after, reloaded) {
        Ok(d) => d,
        Err(e) if e.contains("did not draw version") => {
            return Ok(Outcome::NotRun(format!(
                "the window stopped repainting (screen locked, or not in front): {e}"
            )));
        }
        Err(e) => return Err(e),
    };
    let desktop = drawn.saturating_sub(baseline);
    let total = host + desktop;
    if pixels(&after)? == pixels(&shot)? {
        return Err(format!(
            "{}: the saved change is not drawn; the image is the one before the save",
            after.display()
        ));
    }
    said.push(format!(
        "preview: {} ({})",
        after.display(),
        themed(&after, theme)?
    ));
    said.push(format!(
        "preview: a save to app.ts was in the host's tree after {} ms and drawn by the desktop \
         {} ms later (capture {} ms, of which {} ms is any capture's own time)",
        host.as_millis(),
        desktop.as_millis(),
        drawn.as_millis(),
        baseline.as_millis()
    ));
    if total > RELOAD_BUDGET {
        return Err(format!(
            "the preview pane showed a save after {} ms, over {} ms",
            total.as_millis(),
            RELOAD_BUDGET.as_millis()
        ));
    }
    Ok(Outcome::Passed(said))
}

/// A PNG's size and pixels as RGB.
fn pixels(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        other => return Err(format!("{}: unexpected {other:?}", path.display())),
    };
    let rgb = buf[..info.buffer_size()]
        .chunks_exact(channels)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    Ok((info.width, info.height, rgb))
}

/// An app pane's image in the scheme `theme` (its background and the
/// chrome's surface): most of it is one of them, and more than a few
/// colors are drawn on it. A one-line summary, or why not.
fn themed(path: &Path, theme: [u32; 2]) -> Result<String, String> {
    let (w, h, rgb) = pixels(path)?;
    let near = |p: &[u8], hex: u32| {
        let want = [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8];
        p.iter()
            .zip(want)
            .all(|(a, b)| (i32::from(*a) - i32::from(b)).abs() <= 3)
    };
    let (mut themed, mut all) = (0usize, 0usize);
    let mut colors = std::collections::HashSet::new();
    for p in rgb.as_chunks::<3>().0 {
        all += 1;
        if theme.iter().any(|t| near(p, *t)) {
            themed += 1;
        }
        colors.insert([p[0], p[1], p[2]]);
    }
    let share = themed as f64 / all.max(1) as f64;
    if share < 0.5 {
        return Err(format!(
            "{}: only {:.1}% of the pane is the scheme's background or surface",
            path.display(),
            share * 100.0
        ));
    }
    if colors.len() < 8 {
        return Err(format!(
            "{}: only {} colors; nothing is drawn on the background",
            path.display(),
            colors.len()
        ));
    }
    Ok(format!(
        "{w}x{h}, {:.1}% the scheme's background or surface, {} colors",
        share * 100.0,
        colors.len()
    ))
}
