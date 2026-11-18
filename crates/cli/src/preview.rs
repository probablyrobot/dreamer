//! `dream app preview`: an app drawn from its fixtures.
//!
//! - `--text` (the default) draws it once as text, with no daemon.
//! - `--open` opens a live preview in a new tmux window, which the desktop
//!   draws natively like any app pane. The window's program is this
//!   command again with `--pane`: it builds the app's TypeScript whenever
//!   `app.toml` or the source changes, and runs `dreamer-termapp --live`,
//!   the app host, which starts the app again from its fixtures whenever a
//!   file of the package changes (the JavaScript built, the manifest, the
//!   fixtures). The app runs in the host, never in the desktop, and every
//!   call it makes is answered from `fixtures/calls.json`.
//! - `--png FILE` asks the running desktop, through the daemon, for an
//!   image of the app's preview pane as the desktop draws it (opening the
//!   preview first when none is open). With no desktop it fails and says
//!   so. With `--level L` it does this as an agent at that level would:
//!   its connection first narrows itself to the agent's grant, opens a
//!   preview pane of its own, and closes it once the image is written.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::build::{USAGE, checked, manifest, ok_or_error, preview_text, termapp};
use crate::daemon::{DaemonArgs, explain};
use crate::remote::session;
use api::{Request, Response};

/// What `--png` says when no desktop is there to draw the app.
pub const NO_DESKTOP: &str = "no Dreamer window is running; --png needs one";

/// One call on a shared connection.
async fn ask(c: &client::Client, r: Request) -> Result<Response, String> {
    c.call(r).await.map_err(explain)
}

/// How the app is previewed.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Text,
    Open,
    Png(PathBuf),
    /// The preview pane's own program.
    Pane,
}

fn rt() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

/// `dream app preview PATH [--text [--size WxH] | --open | --png FILE] [--session $N]`.
pub fn run(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let [path, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    if path.starts_with('-') {
        return Err(USAGE.into());
    }
    let (mut modes, mut size, mut session_id, mut level) = (Vec::new(), None, None, None);
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--text" => modes.push(Mode::Text),
            "--open" => modes.push(Mode::Open),
            "--pane" => modes.push(Mode::Pane),
            "--png" => modes.push(Mode::Png(PathBuf::from(value()?))),
            "--size" => size = Some(value()?),
            "--level" => level = Some(crate::mcp::parse_level(&value()?)?),
            "--session" => {
                let v = value()?;
                session_id = Some(
                    v.trim_start_matches('$')
                        .parse::<u32>()
                        .map_err(|_| format!("`{v}` is not a session id"))?,
                );
            }
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    let mode = match modes.len() {
        0 => Mode::Text,
        1 => modes.remove(0),
        _ => return Err(format!("choose one of --text, --open, --png\n{USAGE}")),
    };
    if size.is_some() && mode != Mode::Text {
        return Err("--size is for --text; the desktop draws the pane at its size".into());
    }
    if level.is_some() && !matches!(mode, Mode::Png(_)) {
        return Err("--level is for --png".into());
    }
    match mode {
        Mode::Text => {
            let m = manifest(path);
            crate::strip::build(&m)?;
            print!("{}", preview_text(&m, size.as_deref())?);
            Ok(())
        }
        Mode::Open => open(d, path, session_id),
        Mode::Png(file) => match level {
            Some(level) => png_as_agent(d, path, &file, level),
            None => png(d, path, &file, session_id),
        },
        Mode::Pane => pane(path),
    }
}

/// A package ready to preview live: built, checked, and drawing its own
/// view tree.
pub(crate) struct Package {
    dir: PathBuf,
    id: String,
    problems: Vec<crate::typecheck::Problem>,
}

impl Package {
    /// The app id of its preview instances.
    fn preview_id(&self) -> String {
        format!("{}.preview", self.id)
    }
}

pub(crate) fn prepare(path: &str) -> Result<Package, String> {
    let m = manifest(path);
    let dir = m
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let m = dir.join(m.file_name().ok_or("no manifest")?);
    crate::strip::build(&m)?;
    let v = checked(&m)?;
    ok_or_error(&v)?;
    let text = std::fs::read_to_string(&m).map_err(|e| format!("{}: {e}", m.display()))?;
    let table: toml::Table = text.parse().map_err(|e| format!("{}: {e}", m.display()))?;
    // A web app's view is its page (`[web]`, no view of its own).
    let web_app =
        table.contains_key("web") && !table.contains_key("root") && !table.contains_key("nodes");
    if !table.contains_key("script") && !table.contains_key("process") && !web_app {
        return Err(
            "--open and --png preview script, process, and web apps; preview a manifest app with --text"
                .into(),
        );
    }
    Ok(Package {
        problems: crate::strip::type_problems(&m),
        dir,
        id: v["id"].as_str().unwrap_or_default().to_owned(),
    })
}

/// Start a preview pane for `pkg` in a new tmux window; its pane.
async fn launch(c: &client::Client, pkg: &Package, session_id: Option<u32>) -> Result<u32, String> {
    let session_id = match session_id {
        Some(s) => s,
        None => {
            let Response::TmuxTopology(t) =
                ask(c, Request::TmuxTopology(api::TopologyParams::default())).await?
            else {
                return Err("unexpected reply".into());
            };
            t.sessions
                .first()
                .map(|s| s.id)
                .ok_or("tmux has no sessions")?
        }
    };
    let me = std::env::current_exe().map_err(|e| format!("this program: {e}"))?;
    let name = pkg
        .dir
        .file_name()
        .map_or_else(|| pkg.id.clone(), |n| n.to_string_lossy().into_owned());
    let Response::AppsLaunch(l) = ask(
        c,
        Request::AppsLaunch(api::LaunchApp {
            app_id: pkg.preview_id(),
            // The app's calls are answered from its fixtures; its host
            // needs nothing beyond publishing its own view.
            grant: Some(api::Grant::default()),
            argv: vec![
                me.display().to_string(),
                "app".into(),
                "preview".into(),
                pkg.dir.display().to_string(),
                "--pane".into(),
            ],
            session: session_id,
            name: Some(format!("preview {name}")),
            cwd: Some(pkg.dir.display().to_string()),
            ttl_ms: None,
            split: None,
        }),
    )
    .await?
    else {
        return Err("unexpected reply".into());
    };
    Ok(l.pane)
}

async fn desktops(c: &client::Client) -> Result<u32, String> {
    let Response::DaemonStatus(s) = ask(c, Request::DaemonStatus(api::Empty {})).await? else {
        return Err("unexpected reply".into());
    };
    Ok(s.desktops)
}

fn warn(problems: &[crate::typecheck::Problem]) {
    for p in problems {
        eprintln!("warning: {p}");
    }
}

/// `--open`.
fn open(d: &DaemonArgs, path: &str, session_id: Option<u32>) -> Result<(), String> {
    let pkg = prepare(path)?;
    warn(&pkg.problems);
    rt()?.block_on(async {
        let c = session(d).await?;
        let pane = launch(&c, &pkg, session_id).await?;
        println!(
            "previewing {} in pane %{pane}; it reloads when the package changes (q in the pane ends it)",
            pkg.id
        );
        if desktops(&c).await? == 0 {
            println!("no Dreamer window is running: the pane shows the app's text interface");
        }
        Ok(())
    })
}

/// A live preview pane of `pkg` open now: the newest.
async fn open_preview(c: &client::Client, pkg: &Package) -> Result<Option<u32>, String> {
    let Response::TmuxTopology(t) =
        ask(c, Request::TmuxTopology(api::TopologyParams::default())).await?
    else {
        return Err("unexpected reply".into());
    };
    let id = pkg.preview_id();
    Ok(t.panes
        .iter()
        .filter(|p| p.app.as_ref().is_some_and(|a| a.live && a.app_id == id))
        .map(|p| p.id)
        .max())
}

/// The version of the pane's view tree once its host has published one.
async fn published(c: &client::Client, pane: u32) -> Result<u64, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let answer = c
            .call(Request::AppsTree(api::AppTreeQuery {
                pane,
                after: 0,
                seq: 0,
                wait_ms: Some(1_000),
            }))
            .await;
        match answer {
            Ok(Response::AppsTree(t)) if t.seq > 0 => return Ok(t.seq),
            // Not registered yet, or no tree yet.
            Ok(_) | Err(client::ClientError::Rpc(_)) => {}
            Err(e) => return Err(e.to_string()),
        }
        if Instant::now() > deadline {
            return Err(format!(
                "the preview in pane %{pane} drew nothing within 15 s"
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `--png FILE`.
fn png(d: &DaemonArgs, path: &str, file: &Path, session_id: Option<u32>) -> Result<(), String> {
    let file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(file)
    };
    rt()?.block_on(async {
        // A desktop starts the daemon it uses: none running, no desktop.
        let Ok(c) = crate::daemon::connect().await else {
            return Err(NO_DESKTOP.to_owned());
        };
        if desktops(&c).await? == 0 {
            return Err(NO_DESKTOP.to_owned());
        }
        drop(c);
        let pkg = prepare(path)?;
        warn(&pkg.problems);
        let c = session(d).await?;
        let pane = match open_preview(&c, &pkg).await? {
            Some(p) => p,
            None => {
                let p = launch(&c, &pkg, session_id).await?;
                println!("opened a preview of {} in pane %{p}", pkg.id);
                p
            }
        };
        let got = capture(&c, pane, &file).await?;
        println!(
            "wrote {} ({}x{}) from pane %{pane}",
            got.path, got.width, got.height
        );
        Ok(())
    })
}

/// The desktop's drawing of `pane`, once the pane has drawn, as a PNG
/// file at `file`.
async fn capture(c: &client::Client, pane: u32, file: &Path) -> Result<api::AppCaptured, String> {
    let seq = published(c, pane).await?;
    let Response::AppsCapture(got) = ask(
        c,
        Request::AppsCapture(api::AppCapture {
            pane,
            path: file.display().to_string(),
            seq,
            wait_ms: Some(20_000),
        }),
    )
    .await?
    else {
        return Err("unexpected reply".into());
    };
    Ok(got)
}

/// An image preview as an agent takes it, on the agent's own connection:
/// with no desktop, `NO_DESKTOP` before anything opens; else a preview pane
/// of its own in a new window, the desktop's drawing of it written to
/// `file`, and the pane closed again. The package's type problems, which
/// do not stop a preview, come back with the image.
pub(crate) async fn agent_png(
    c: &client::Client,
    path: &str,
    file: &Path,
) -> Result<(api::AppCaptured, Vec<crate::typecheck::Problem>), String> {
    // A grant without daemon.status learns of a missing desktop from
    // apps.capture instead.
    if let Ok(0) = desktops(c).await {
        return Err(NO_DESKTOP.to_owned());
    }
    let owned = path.to_owned();
    let pkg = tokio::task::spawn_blocking(move || prepare(&owned))
        .await
        .map_err(|e| e.to_string())??;
    let pane = launch(c, &pkg, None).await?;
    let got = capture(c, pane, file).await;
    // The pane was only for this image.
    let _ = c
        .call(Request::ActionsSubmit(api::SubmitAction {
            action: Some(api::Action {
                kind: Some(api::action::Kind::KillPane(api::KillPane { pane })),
            }),
            ..api::SubmitAction::default()
        }))
        .await;
    Ok((got?, pkg.problems))
}

/// `--png FILE --level L`.
fn png_as_agent(d: &DaemonArgs, path: &str, file: &Path, level: i32) -> Result<(), String> {
    let file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(file)
    };
    rt()?.block_on(async {
        let c = session(d).await?;
        ask(&c, Request::GrantsNarrow(crate::mcp::agent_grant(level))).await?;
        let (got, problems) = agent_png(&c, path, &file).await?;
        warn(&problems);
        println!("wrote {} ({}x{})", got.path, got.width, got.height);
        Ok(())
    })
}

/// The files `--pane` rebuilds from: the manifest and a script app's
/// TypeScript, by content.
#[derive(Debug)]
pub(crate) struct Sources {
    manifest: PathBuf,
    last: u64,
}

impl Sources {
    pub(crate) fn new(manifest: &Path) -> Sources {
        let mut s = Sources {
            manifest: manifest.to_path_buf(),
            last: 0,
        };
        s.last = s.fingerprint();
        s
    }

    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let text = std::fs::read(&self.manifest).unwrap_or_default();
        text.hash(&mut h);
        if let Ok(Some(source)) = crate::strip::script_source(&self.manifest) {
            let dir = self.manifest.parent().unwrap_or(Path::new("."));
            std::fs::read(dir.join(source))
                .unwrap_or_default()
                .hash(&mut h);
        }
        h.finish()
    }

    /// Whether they changed since the last time this was asked.
    pub(crate) fn changed(&mut self) -> bool {
        let now = self.fingerprint();
        let changed = now != self.last;
        self.last = now;
        changed
    }
}

/// Build the app's JavaScript from its TypeScript, and write what the
/// build says to `status` for the preview's status line: the first error,
/// or the first type problem; nothing when it is clean.
pub(crate) fn rebuild(manifest: &Path, status: &Path) -> Result<(), String> {
    let said = match crate::strip::build(manifest) {
        Err(e) => {
            let first = e.lines().next().unwrap_or_default().to_owned();
            format!("not built: {first}")
        }
        Ok(()) => {
            let problems = crate::strip::type_problems(manifest);
            match problems.as_slice() {
                [] => String::new(),
                [p] => format!("{p}"),
                [p, rest @ ..] => format!("{p} (and {} more)", rest.len()),
            }
        }
    };
    std::fs::write(status, said).map_err(|e| format!("{}: {e}", status.display()))
}

/// `--pane`: the preview pane's program. Rebuilds the app when its sources
/// change and runs its host until the host ends; exits as it did.
fn pane(path: &str) -> Result<(), String> {
    let m = manifest(path);
    let status = std::env::temp_dir().join(format!("dreamer-preview-{}.txt", std::process::id()));
    rebuild(&m, &status)?;
    let mut sources = Sources::new(&m);
    let mut host = std::process::Command::new(termapp())
        .arg("--live")
        .arg(&m)
        .arg("--status")
        .arg(&status)
        .spawn()
        .map_err(|e| format!("{}: {e}", termapp().display()))?;
    let code = loop {
        match host.try_wait() {
            Ok(Some(s)) => break s.code().unwrap_or(1),
            Ok(None) => {}
            Err(e) => {
                let _ = std::fs::remove_file(&status);
                return Err(e.to_string());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        if sources.changed() {
            let _ = rebuild(&m, &status);
        }
    };
    let _ = std::fs::remove_file(&status);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("drm-preview-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("app.toml"),
            "schema_version = 2\nid = \"local.p\"\nversion = \"0.1.0\"\ntitle = \"P\"\n\n[script]\nsource = \"app.ts\"\n",
        )
        .expect("manifest");
        std::fs::write(dir.join("app.ts"), "const n: number = 1;\n").expect("source");
        dir
    }

    #[test]
    fn a_save_of_the_source_or_manifest_rebuilds_and_says_what_the_build_said() {
        let dir = package("rebuild");
        let m = dir.join("app.toml");
        let status = dir.join("status.txt");
        rebuild(&m, &status).expect("rebuild");
        let built = dir.join(app_protocol::build::output("app.ts"));
        let js = std::fs::read_to_string(&built).expect("built");
        assert!(app_protocol::build::built_from(
            &js,
            "const n: number = 1;\n"
        ));
        let mut sources = Sources::new(&m);
        assert!(!sources.changed(), "nothing changed yet");
        // Building again changes no source.
        rebuild(&m, &status).expect("rebuild");
        assert!(!sources.changed());

        std::fs::write(dir.join("app.ts"), "const n: number = 2;\nlet b: = 3;\n").expect("save");
        assert!(sources.changed());
        assert!(!sources.changed(), "said once");
        rebuild(&m, &status).expect("rebuild");
        let said = std::fs::read_to_string(&status).expect("status");
        assert!(said.starts_with("not built: app.ts:2:"), "{said}");
        // The last good build stays for the running app.
        assert_eq!(std::fs::read_to_string(&built).expect("kept"), js);

        std::fs::write(dir.join("app.ts"), "const n: number = 3;\n").expect("fix");
        assert!(sources.changed());
        rebuild(&m, &status).expect("rebuild");
        assert_eq!(std::fs::read_to_string(&status).expect("status"), "");
        let js = std::fs::read_to_string(&built).expect("built");
        assert!(app_protocol::build::built_from(
            &js,
            "const n: number = 3;\n"
        ));

        std::fs::write(&m, std::fs::read_to_string(&m).expect("m") + "# more\n").expect("m");
        assert!(sources.changed(), "the manifest counts too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn modes_do_not_mix() {
        let d = DaemonArgs::default();
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let e = run(&d, &args(&["x", "--open", "--png", "a.png"])).expect_err("two modes");
        assert!(e.contains("choose one"), "{e}");
        let e = run(&d, &args(&["x", "--open", "--size", "80x24"])).expect_err("size");
        assert!(e.contains("--size is for --text"), "{e}");
    }
}
