//! Dreamer Web, the component that holds Chromium apart from the app, and
//! the desktop with web panes for development.
//!
//! - `cargo xtask web bundle [--release] [--install]` writes, in
//!   `<target>/web-bundle`, `Dreamer.app` (the app built with its `web`
//!   feature, no Chromium inside) and `dreamer-web/`, the component built
//!   beside it ([`build_component`]): CEF's framework and the five helper
//!   apps CEF starts its renderer, GPU, and utility processes from, each
//!   signed, with the manifest naming this Dreamer and CEF release.
//!   `--install` then installs that component with the bundle's own `dream
//!   web install`, into `web/` of your Dreamer data directory.
//! - `cargo xtask web smoke` builds both and runs the gate's web pane
//!   check ([`smoke`]) alone, with the component added from a web pane
//!   into a private data directory.
//!
//! `cargo xtask package` makes the same two things for a release, as two
//! artifacts: the base zip and [`zip_component`]'s. The pinned CEF
//! distribution, cmake, and Ninja come from `cargo xtask deps web`. macOS
//! only so far; the Linux component (#330's `dreamer-web` archive) is not
//! built yet.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use web_component::{FRAMEWORK, HELPER_EXE, HELPERS, MANIFEST, State, TOP};

use crate::deps::WebDeps;
use crate::package::Profile;
use crate::util::{cargo_target, repo_root};

/// The platform the component is built for.
const TARGET: &str = "aarch64-apple-darwin";

pub fn run(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: cargo xtask web bundle [--release] [--install] | web smoke";
    let mut profile = Profile::Debug;
    let mut install = false;
    let smoke_too = match args {
        [cmd, rest @ ..] if cmd == "bundle" => {
            for a in rest {
                match a.as_str() {
                    "--release" => profile = Profile::Release,
                    "--install" => install = true,
                    other => return Err(format!("unknown web bundle argument `{other}`\n{USAGE}")),
                }
            }
            false
        }
        [cmd] if cmd == "smoke" => true,
        _ => return Err(USAGE.into()),
    };
    let web = provisioned()?
        .ok_or("the web engine's dependencies are not provisioned: run `cargo xtask deps web`")?;
    let root = repo_root();
    let (app, component) = bundle(&root, profile, &web)?;
    println!("web: {}", app.display());
    println!("web: {}", component.display());
    if install {
        print!(
            "{}",
            dream(&app, &["web", "install"], Some(&component), &[])?
        );
    }
    if smoke_too {
        // The gate's web pane check by itself, its captures in
        // `<target>/web-smoke`.
        match smoke(&app, &component, &cargo_target(&root).join("web-smoke"))? {
            Smoke::Passed(lines) => println!("{}\nweb: smoke passed", lines.join("\n")),
            Smoke::NotRun(lines) => println!("{lines}\nweb: smoke not run: nothing was drawn"),
        }
    }
    Ok(())
}

/// The web engine's dependencies, if `cargo xtask deps web` provisioned
/// them; `None` on a host without web panes.
pub fn provisioned() -> Result<Option<WebDeps>, String> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }
    crate::deps::web_deps(&crate::manifest::Manifest::load()?)
}

/// Build and verify the development bundle, and the component beside it,
/// in `root`'s target directory.
pub fn bundle(root: &Path, profile: Profile, web: &WebDeps) -> Result<(PathBuf, PathBuf), String> {
    let out = cargo_target(root).join("web-bundle");
    let app = crate::package::build_bundle_with(root, profile, &out, Some(web))?;
    crate::package::verify_bundle(&app)?;
    let component = build_component(
        &out,
        &crate::package::bin_dir(root, profile),
        web,
        &crate::package::version(root)?,
    )?;
    verify_component(&component)?;
    Ok((app, component))
}

fn run_checked(cmd: &mut Command, what: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn sign(path: &Path) -> Result<(), String> {
    run_checked(
        Command::new("codesign")
            .args(["--force", "--sign", "-", "--timestamp=none"])
            .arg(path),
        "codesign",
    )
    .map(drop)
}

/// Build the component as `out/dreamer-web`: the framework from the CEF
/// distribution, the helper apps around the helper executable in
/// `bin_dir`, each signed ad hoc, CEF's license and Chromium's notices,
/// and the manifest naming Dreamer `version` and the CEF release.
pub fn build_component(
    out: &Path,
    bin_dir: &Path,
    web: &WebDeps,
    version: &str,
) -> Result<PathBuf, String> {
    // The release the component claims is the one the build used.
    let baseline = crate::manifest::Manifest::load()?;
    let pinned = baseline.get("cef", "version")?;
    if !web_component::CEF_VERSION.starts_with(&format!("{pinned}+")) {
        return Err(format!(
            "web_component::CEF_VERSION is {}, but deps/baseline.toml pins CEF {pinned}",
            web_component::CEF_VERSION
        ));
    }
    let dir = out.join(TOP);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let framework = dir.join(FRAMEWORK);
    // ditto keeps the framework's links and permissions.
    run_checked(
        Command::new("ditto").arg(web.framework()).arg(&framework),
        "ditto",
    )?;
    sign(&framework)?;
    let helper = bin_dir.join(HELPER_EXE);
    for (suffix, id) in HELPERS {
        let name = web_component::helper_name(suffix);
        let app = dir.join(web_component::helper_app(suffix));
        let contents = app.join("Contents");
        let macos = contents.join("MacOS");
        std::fs::create_dir_all(&macos).map_err(|e| e.to_string())?;
        std::fs::copy(&helper, macos.join(&name)).map_err(|e| format!("copy {HELPER_EXE}: {e}"))?;
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>{name}</string>
  <key>CFBundleIdentifier</key><string>dev.dreamer.Dreamer.{id}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>{name}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>LSUIElement</key><string>1</string>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
"#
        );
        std::fs::write(contents.join("Info.plist"), plist).map_err(|e| e.to_string())?;
        sign(&macos.join(&name))?;
        sign(&app)?;
    }
    for notice in ["LICENSE.txt", "CREDITS.html"] {
        std::fs::copy(web.cef_dir.join(notice), dir.join(notice))
            .map_err(|e| format!("copy {notice}: {e}"))?;
    }
    let manifest = web_component::Manifest {
        version: version.to_owned(),
        cef: web_component::CEF_VERSION.to_owned(),
        target: TARGET.to_owned(),
    };
    std::fs::write(dir.join(MANIFEST), manifest.to_json()).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// The component is this build's and complete, the framework and every
/// helper app pass a strict signature check, and a helper loads the
/// framework from where it sits. Opens no window.
pub fn verify_component(dir: &Path) -> Result<(), String> {
    match web_component::check(dir) {
        State::Ready(_) => {}
        other => {
            return Err(format!(
                "{}: {}",
                dir.display(),
                other.problem(dir).unwrap_or_default()
            ));
        }
    }
    let mut signed = vec![dir.join(FRAMEWORK)];
    signed.extend(
        HELPERS
            .iter()
            .map(|(suffix, _)| dir.join(web_component::helper_app(suffix))),
    );
    for path in &signed {
        run_checked(
            Command::new("codesign")
                .args(["--verify", "--strict", "--deep", "--verbose=1"])
                .arg(path),
            "codesign --verify",
        )?;
    }
    // Without a process type CEF's entry point returns at once (-1): this
    // checks only that the helper finds and loads the framework.
    let exe = web_component::helper_executable(dir, "");
    let out = Command::new(&exe)
        .output()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.code() != Some(255) || stderr.contains("dreamer-web-helper:") {
        return Err(format!(
            "the helper did not load the framework ({}): {}",
            out.status,
            stderr.trim()
        ));
    }
    println!(
        "web: {}: CEF framework and {} helper apps, each signed; a helper loads the framework; {}",
        dir.display(),
        HELPERS.len(),
        du(dir)
    );
    Ok(())
}

/// `du -sh`'s size of `path`.
pub fn du(path: &Path) -> String {
    Command::new("du")
        .args(["-sh"])
        .arg(path)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_owned()
        })
        .unwrap_or_default()
}

/// Zip the component as the release's
/// `dreamer-web-<version>-aarch64-apple-darwin.zip` in `out`, with its
/// `.sha256` and its record (`.json`, as the base zip has).
pub fn zip_component(dir: &Path, out: &Path, version: &str) -> Result<PathBuf, String> {
    let name = web_component::archive_name(version, TARGET);
    let zip = out.join(&name);
    let _ = std::fs::remove_file(&zip);
    run_checked(
        Command::new("ditto")
            .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
            .arg(dir)
            .arg(&zip),
        "ditto",
    )?;
    let sha = crate::util::sha256_file(&zip)?;
    std::fs::write(
        out.join(format!("{name}.sha256")),
        format!("{sha}  {name}\n"),
    )
    .map_err(|e| e.to_string())?;
    let (kind, team, stapled) =
        crate::package::signing_state(&web_component::helper_executable(dir, ""));
    let mut contents = vec![FRAMEWORK.to_owned()];
    contents.extend(HELPERS.iter().map(|(s, _)| web_component::helper_app(s)));
    let record = serde_json::json!({
        "artifact": name,
        "bytes": std::fs::metadata(&zip).map_or(0, |m| m.len()),
        "sha256": sha,
        "target": TARGET,
        "version": version,
        "commit": crate::package::commit(&repo_root()),
        "component": TOP,
        "cef": web_component::CEF_VERSION,
        "contents": contents,
        "signing": { "kind": kind, "team": team, "notarized": stapled },
    });
    std::fs::write(
        out.join(format!("{name}.json")),
        serde_json::to_string_pretty(&record).map_err(|e| e.to_string())? + "\n",
    )
    .map_err(|e| e.to_string())?;
    Ok(zip)
}

/// Run the bundle's `dream` with `args` (then `extra`, a path) and `env`;
/// its output, or why it failed.
pub fn dream(
    app: &Path,
    args: &[&str],
    extra: Option<&Path>,
    env: &[(&str, OsString)],
) -> Result<String, String> {
    let mut cmd = Command::new(app.join("Contents/MacOS/dream"));
    cmd.args(args);
    if let Some(p) = extra {
        cmd.arg(p);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(|e| format!("dream: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("dream {} failed: {}", args.join(" "), text.trim()))
    }
}

/// Adding the component from `from` (an archive or a directory) with the
/// bundle's `dream`, into the empty data directory `data`: before it, `dream
/// web status` says it is missing and how to add it; `dream web install`
/// checks and installs it; then `dream web status` says it is ready.
/// Opens no window. The lines it printed.
pub fn check_install(app: &Path, from: &Path, data: &Path) -> Result<Vec<String>, String> {
    // Not there at all: `dream` makes it (private), as on a new machine.
    let _ = std::fs::remove_dir_all(data);
    let env = [("DREAMER_DATA_DIR", data.as_os_str().to_owned())];
    let mut lines = Vec::new();
    match dream(app, &["web", "status"], None, &env) {
        Ok(text) => {
            return Err(format!(
                "with no component, `dream web status` said: {text}"
            ));
        }
        Err(e) if e.contains("dream web install") => {
            lines.push(format!("without it: {}", e.trim()));
        }
        Err(e) => return Err(format!("with no component: {e}")),
    }
    for line in dream(app, &["web", "install"], Some(from), &env)?.lines() {
        lines.push(line.to_owned());
    }
    lines.push(
        dream(app, &["web", "status"], None, &env)?
            .trim()
            .to_owned(),
    );
    Ok(lines)
}

/// What the web smoke saw, or why it could not judge.
pub enum Smoke {
    Passed(Vec<String>),
    /// The window or the page was not being drawn.
    NotRun(String),
}

/// The captures the web smoke makes, each checked at least once: the page
/// loaded, scrolled, its pane resized, zoomed, on a hidden tab, shown
/// again, under the palette, and after the palette.
const CAPTURES: [&str; 8] = [
    "before.png",
    "after.png",
    "resized.png",
    "zoomed.png",
    "tab.png",
    "back.png",
    "palette.png",
    "closed.png",
];

/// The input checks the web smoke reports passing (`smoke: web input:`):
/// a click focuses the page, typing and an input method composition reach
/// a field, Cmd+J shows the drawer from the page and hides it again, and
/// Cmd+A is Chromium's select all. Copy, cut, and paste are not checked:
/// they would use the system clipboard, which the gate never touches.
const INPUT_CHECKS: usize = 5;

/// The web app checks the web smoke reports passing (`smoke: web app:`):
/// the example app's page lists the panes through its bridge, a call
/// through it lists them, a call outside its grant is refused, so is
/// `web.read` without `web.inspect`, `apps.capture` of its pane is its
/// page's image, another origin has no bridge, a navigation outside its
/// allow-list is refused, and so is a download (it lacks `web.download`).
const APP_CHECKS: usize = 8;

/// The agent checks the web smoke reports passing (`smoke: web inspect:`):
/// `web.read` refused without `web.inspect`; with it, `web.read` finds the
/// form's field, `web.type` fills and submits it, `web.click` follows a
/// link, and `web.screenshot` writes the page.
const INSPECT_CHECKS: usize = 5;

/// The install checks the web smoke reports passing (`smoke: web
/// install:`), in its first run, with no Dreamer Web: the tab strip offers
/// it, a web pane shows "Add Dreamer Web", its button adds it from
/// `DREAMER_WEB_URL`, the offer is then gone, and the page loads and draws
/// in that pane with no restart.
const INSTALL_CHECKS: usize = 5;

/// The name the smoke's agent sends the form.
const SENT_NAME: &str = "Ada Lovelace";

/// The browser checks the web smoke reports passing (`smoke: web
/// browser:`): `web.navigate` loads a page in the pane; Cmd+L, a typed
/// address, and Enter load it; a link is followed; back and forward by
/// key, and by the mouse's side buttons; reload; find in the page; zoom; a
/// download asks first; Save writes it to the downloads folder; Cancel at
/// the prompt writes nothing; Cancel while saving leaves no partial file;
/// a new window's link in the same pane; and, in the second run, the
/// login's session cookie after a restart.
const BROWSER_CHECKS: usize = 14;

/// The color checks the web smoke reports (`smoke: web color:`), in its
/// two `--smoke-web-color` runs: with `web.color: srgb`, Display P3's red
/// is clamped to sRGB live; with `display`, the page's snapshot under the
/// palette matches the live page, and Display P3's red is out of sRGB's
/// range live. That last one needs a wide-gamut display: without one it
/// is a `smoke: web color not run:` line instead.
const COLOR_CHECKS: usize = 3;

/// Set `web.color` to `mode` in the smoke's `dreamer.yaml` (its `web:`
/// section is the file's last).
fn with_color(config: &Path, mode: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(config).map_err(|e| format!("{}: {e}", config.display()))?;
    let kept: String = text
        .lines()
        .filter(|l| !l.starts_with("  color:"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(config, format!("{kept}  color: {mode}\n"))
        .map_err(|e| format!("{}: {e}", config.display()))
}

/// Run the bundle's `dreamer --smoke-web` against an isolated tmux server
/// with private daemon, desktop, and configuration directories, then check
/// its captures as its `checks.txt` lists: the page's color fills the web
/// pane's rectangle after each move, and is absent beside it, from it on
/// another tab, and from the palette over it. The smoke's input checks
/// (typing, an input method, Cmd+J, select all in the page) pass or
/// fail in the app itself; each one passed is a line of its output. Last,
/// the agent's checks: the site got the form with the name it typed, and
/// its screenshot of the form, and the web app's image, are those pages.
/// Then `web.color`, in two more runs (`--smoke-web-color`, after
/// `web.color: srgb` and then `display` are written to the configuration):
/// see [`COLOR_CHECKS`].
///
/// Chromium comes from `component`, added first as a user adds Dreamer
/// Web: the smoke's private data directory starts empty, and its first run
/// (`--smoke-web-install`) presses a web pane's "Add Dreamer Web", which
/// downloads the zip made here from `component` (`DREAMER_WEB_URL`, a
/// `file://` directory). The other runs use what it installed.
pub fn smoke(app: &Path, component: &Path, out: &Path) -> Result<Smoke, String> {
    use std::os::unix::fs::PermissionsExt as _;
    let tmux = std::env::var("DREAMER_TMUX").unwrap_or_else(|_| "tmux".into());
    let pid = std::process::id();
    let socket = format!("dreamer-gate-web-{pid}");
    let root = PathBuf::from(format!("/tmp/drm-gate-web-{pid}"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(out);
    let zips = root.join("zip");
    let made = std::fs::create_dir_all(root.join("config"))
        .and_then(|()| std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)))
        .and_then(|()| std::fs::create_dir_all(&zips))
        .map_err(|e| format!("{}: {e}", root.display()))
        .and_then(|()| zip_component(component, &zips, web_component::VERSION));
    if let Err(e) = made {
        let _ = std::fs::remove_dir_all(&root);
        return Err(e);
    }
    // The drawer Cmd+J shows from inside the page runs a plain shell: the
    // check is about the shortcut, not about any agent. Dreamer Web is
    // offered, as by default. Downloads go to a folder of the smoke's own,
    // never the user's.
    let downloads = root.join("downloads");
    std::fs::write(
        root.join("config/dreamer.yaml"),
        format!(
            "agents:\n  pi:\n    command: [env, 'PS1=$ ', /bin/sh]\n\
             web:\n  component: ask\n  downloads:\n    folder: {:?}\n",
            downloads.display().to_string()
        ),
    )
    .map_err(|e| e.to_string())?;
    let started = Command::new(&tmux)
        .args(["-L", &socket, "-f", "/dev/null", "new-session", "-d"])
        .args(["-s", "main", "-x", "120", "-y", "36"])
        .arg("env PS1='$ ' /bin/sh")
        .status()
        .map_err(|e| format!("{tmux}: {e}"))?;
    if !started.success() {
        return Err("could not start an isolated tmux server".into());
    }
    let host = site::start()?;
    let dreamer = |first: &[&std::ffi::OsStr]| {
        let mut c = Command::new(app.join("Contents/MacOS/dreamer"));
        c.args(first)
            .env("DREAMER_SMOKE_WEB_SITE", &host)
            .args(["-L", &socket, "--tmux", &tmux])
            .env("DREAMER_RUNTIME_DIR", root.join("rt"))
            .env("DREAMER_STATE_DIR", root.join("st"))
            .env("DREAMER_DESKTOP_STATE_DIR", root.join("desktop"))
            .env("DREAMER_CONFIG_DIR", root.join("config"))
            .env("DREAMER_DATA_DIR", root.join("data"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1");
        c
    };
    // Dreamer Web added from a web pane; then the pages; then the browser
    // after a restart: a new desktop process on the same profile. Each run
    // once the one before it passed.
    let then = |before: std::io::Result<std::process::Output>, mut next: Command| match before {
        Ok(first) if first.status.success() => next.output().map(|again| {
            let mut stdout = first.stdout;
            stdout.extend(&again.stdout);
            std::process::Output { stdout, ..again }
        }),
        other => other,
    };
    let mut adding = dreamer(&["--smoke-web-install".as_ref()]);
    adding.env(web_component::URL_ENV, format!("file://{}", zips.display()));
    let result = then(
        adding.output(),
        dreamer(&["--smoke-web".as_ref(), out.as_os_str()]),
    );
    let result = then(result, dreamer(&["--smoke-web-restart".as_ref()]));
    // `web.color`, a run for each: Chromium takes it as it starts. The
    // configuration is written before each run starts.
    let color = |mode: &str| dreamer(&["--smoke-web-color".as_ref(), out.join(mode).as_os_str()]);
    let config = root.join("config/dreamer.yaml");
    let result = match result {
        Ok(r) if r.status.success() => with_color(&config, "srgb")
            .map_err(std::io::Error::other)
            .and_then(|()| then(Ok(r), color("srgb"))),
        other => other,
    };
    let result = match result {
        Ok(r) if r.status.success() => with_color(&config, "display")
            .map_err(std::io::Error::other)
            .and_then(|()| then(Ok(r), color("display"))),
        other => other,
    };
    if let Some(pid) = std::fs::read_to_string(root.join("rt/dreamerd.lock"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    let _ = Command::new(&tmux)
        .args(["-L", &socket, "kill-server"])
        .status();
    let _ = std::fs::remove_dir_all(&root);
    let result = result.map_err(|e| format!("dreamer: {e}"))?;
    let mut lines: Vec<String> = String::from_utf8_lossy(&result.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    match result.status.code() {
        Some(0) => {}
        Some(5) => return Ok(Smoke::NotRun(lines.join("\n"))),
        _ => {
            return Err(format!(
                "web smoke failed ({}): {}\n{}",
                result.status,
                lines.join("\n"),
                String::from_utf8_lossy(&result.stderr).trim()
            ));
        }
    }
    let checks =
        std::fs::read_to_string(out.join("checks.txt")).map_err(|e| format!("checks.txt: {e}"))?;
    let mut rows = checks.lines();
    let window_w = rows
        .next()
        .and_then(|l| l.strip_prefix("window "))
        .and_then(|l| l.split(' ').next()?.parse::<f64>().ok())
        .ok_or("checks.txt: no window size")?;
    let mut checked = std::collections::BTreeSet::new();
    for row in rows {
        let bad = || format!("checks.txt: `{row}`");
        let f: Vec<&str> = row.split(' ').collect();
        let [name, color, bound, share, x, y, w, h] = f[..] else {
            return Err(bad());
        };
        let color = u32::from_str_radix(color, 16).map_err(|_| bad())?;
        let num = |s: &str| s.parse::<f64>().map_err(|_| bad());
        let (limit, rect) = (num(share)?, (num(x)?, num(y)?, num(w)?, num(h)?));
        let path = out.join(name);
        let (got, main) = crate::scenes::share_in(&path, color, window_w, rect)?;
        let ok = match bound {
            "min" => got >= limit,
            "max" => got <= limit,
            _ => return Err(bad()),
        };
        lines.push(format!(
            "{}: #{color:06x} covers {:.1}% of {} {} {}x{} ({bound} {:.0}%; most common #{main:06x})",
            path.display(),
            got * 100.0,
            rect.0,
            rect.1,
            rect.2,
            rect.3,
            limit * 100.0
        ));
        if !ok {
            return Err(format!(
                "{}: #{color:06x} covers {:.1}% of {} {} {}x{}, against a {bound} of {:.0}%\n{}",
                path.display(),
                got * 100.0,
                rect.0,
                rect.1,
                rect.2,
                rect.3,
                limit * 100.0,
                lines.join("\n")
            ));
        }
        checked.insert(name.to_owned());
    }
    if let Some(missing) = CAPTURES.iter().find(|c| !checked.contains(**c)) {
        return Err(format!("the web smoke checked nothing in {missing}"));
    }
    let installs = lines
        .iter()
        .filter(|l| l.starts_with("smoke: web install:"))
        .count();
    if installs < INSTALL_CHECKS {
        return Err(format!(
            "the web smoke passed {installs} of {INSTALL_CHECKS} install checks\n{}",
            lines.join("\n")
        ));
    }
    let inputs = lines
        .iter()
        .filter(|l| l.starts_with("smoke: web input:"))
        .count();
    if inputs < INPUT_CHECKS {
        return Err(format!(
            "the web smoke passed {inputs} of {INPUT_CHECKS} input checks\n{}",
            lines.join("\n")
        ));
    }
    let apps = lines
        .iter()
        .filter(|l| l.starts_with("smoke: web app:"))
        .count();
    if apps < APP_CHECKS {
        return Err(format!(
            "the web smoke passed {apps} of {APP_CHECKS} web app checks\n{}",
            lines.join("\n")
        ));
    }
    let browser = lines
        .iter()
        .filter(|l| l.starts_with("smoke: web browser:"))
        .count();
    if browser < BROWSER_CHECKS {
        return Err(format!(
            "the web smoke passed {browser} of {BROWSER_CHECKS} browser checks\n{}",
            lines.join("\n")
        ));
    }
    let colors = lines
        .iter()
        .filter(|l| l.starts_with("smoke: web color:"))
        .count();
    let colors_not_run = lines
        .iter()
        .filter(|l| l.starts_with("smoke: web color not run:"))
        .count();
    if colors + colors_not_run < COLOR_CHECKS || colors == 0 {
        return Err(format!(
            "the web smoke passed {colors} of {COLOR_CHECKS} color checks ({colors_not_run} not run)\n{}",
            lines.join("\n")
        ));
    }
    let inspect = lines
        .iter()
        .filter(|l| l.starts_with("smoke: web inspect:"))
        .count();
    if inspect < INSPECT_CHECKS {
        return Err(format!(
            "the web smoke passed {inspect} of {INSPECT_CHECKS} agent checks\n{}",
            lines.join("\n")
        ));
    }
    // What the agent typed reached the site, and its screenshot is the
    // form's page: its color nearly everywhere.
    let sent = site::sent();
    if sent.last().map(String::as_str) != Some(SENT_NAME) {
        return Err(format!(
            "the site got the form with {sent:?}, not {SENT_NAME:?}"
        ));
    }
    lines.push(format!(
        "web inspect: the site got the form with {SENT_NAME:?}"
    ));
    for (file, color) in [
        ("web-inspect.png", site::FORM),
        // The example web app's page is dark.
        ("app-page.png", 0x1e_1e_1e),
    ] {
        let path = out.join(file);
        let (w, h) = png_size(&path)?;
        let (got, main) = crate::scenes::share_in(
            &path,
            color,
            f64::from(w),
            (0.0, 0.0, f64::from(w), f64::from(h)),
        )?;
        let said = format!(
            "{}: {w}x{h}, #{color:06x} covers {:.1}% (most common #{main:06x})",
            path.display(),
            got * 100.0
        );
        if w < 200 || h < 100 || got < 0.5 {
            return Err(format!("{said}: not the page\n{}", lines.join("\n")));
        }
        lines.push(said);
    }
    Ok(Smoke::Passed(lines))
}

/// A PNG file's width and height.
fn png_size(path: &Path) -> Result<(u32, u32), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let info = reader.info();
    Ok((info.width, info.height))
}

/// The browser's test site, served on 127.0.0.1 for the web smoke only: a
/// page that counts its loads and has a link, a link that wants a new
/// window, a download, a slow download, and words to find; a second page;
/// a login that sets a session cookie; a page that says whether the cookie
/// came; and a form that records the names it is sent ([`site::sent`]).
pub mod site {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    static LOADS: AtomicU32 = AtomicU32::new(0);
    /// The names the form was sent with, in order.
    static SENT: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// The form page's background.
    pub const FORM: u32 = 0x3f_a3_4d;

    /// What `/report.bin` holds; the smoke checks the file it saved
    /// (`DOWNLOAD_BODY` in `crates/app/src/workspace/smoke/web.rs`).
    const DOWNLOAD_BODY: &str = "dreamer web smoke: a saved download\n";

    /// `/slow.bin`'s length, which it never finishes sending: 64 KiB at
    /// once, then a little at a time for a minute, until the client goes.
    const SLOW_LENGTH: usize = 4 << 20;

    fn slow(stream: &TcpStream) {
        let mut out = stream;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=\"slow.bin\"\r\n\
             Content-Length: {SLOW_LENGTH}\r\nConnection: close\r\n\r\n"
        );
        if out.write_all(head.as_bytes()).is_err() || out.write_all(&[b's'; 64 << 10]).is_err() {
            return;
        }
        for _ in 0..600 {
            std::thread::sleep(Duration::from_millis(100));
            if out
                .write_all(&[b's'; 1024])
                .and_then(|()| out.flush())
                .is_err()
            {
                return;
            }
        }
    }

    /// The names the form was sent with so far.
    pub fn sent() -> Vec<String> {
        SENT.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// A form field's value as a query string carries it.
    fn unescape(v: &str) -> String {
        let b = v.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'+' => out.push(b' '),
                b'%' => match v
                    .get(i + 1..i + 3)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(c) => {
                        out.push(c);
                        i += 2;
                    }
                    None => out.push(b'%'),
                },
                c => out.push(c),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn page(title: &str, body: &str) -> String {
        format!(
            "<html><head><title>{title}</title></head>\
             <body style='margin:0;font:16px sans-serif;background:#fff'>{body}</body></html>"
        )
    }

    fn answer(stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new(&stream);
        let mut first = String::new();
        if reader.read_line(&mut first).is_err() {
            return;
        }
        let mut cookie = String::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line.trim().is_empty() => break,
                Ok(_) => {
                    if let Some(v) = line
                        .split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("cookie"))
                    {
                        cookie = v.1.trim().to_owned();
                    }
                }
            }
        }
        let path = first.split(' ').nth(1).unwrap_or("/");
        let link = |id: &str, href: &str, extra: &str, text: &str| {
            format!(
                "<p><a id={id} href='{href}' {extra} style='display:inline-block;padding:8px 16px'>{text}</a></p>"
            )
        };
        let (status, headers, body) = match path {
            "/" => {
                let n = LOADS.fetch_add(1, Ordering::Relaxed) + 1;
                let body = [
                    link("next", "/b", "", "next"),
                    link("pop", "/c", "target=_blank", "new window"),
                    link("dl", "/report.bin", "", "download"),
                    link("slow", "/slow.bin", "", "slow download"),
                    "<p>a needle, and another needle</p>".to_owned(),
                ]
                .concat();
                (
                    "200 OK",
                    String::new(),
                    page(&format!("browser a {n}"), &body),
                )
            }
            "/b" => ("200 OK", String::new(), page("browser b", "<p>b</p>")),
            "/c" => ("200 OK", String::new(), page("browser c", "<p>c</p>")),
            "/report.bin" => (
                "200 OK",
                "Content-Type: application/octet-stream\r\n\
                 Content-Disposition: attachment; filename=\"report.bin\"\r\n"
                    .to_owned(),
                DOWNLOAD_BODY.to_owned(),
            ),
            "/slow.bin" => return slow(&stream),
            // A session cookie: no expiry, so it survives a restart only
            // because the browser's profile keeps session cookies.
            "/login" => (
                "200 OK",
                "Set-Cookie: dreamer_smoke=in; Path=/; HttpOnly\r\n".to_owned(),
                page("logged in", "<p>logged in</p>"),
            ),
            // A form for an agent to fill and submit (`web.type`), on a
            // known color for its screenshot.
            "/form" => (
                "200 OK",
                String::new(),
                page(
                    "form",
                    &format!(
                        "<div style='background:#{FORM:06x};min-height:100vh;padding:24px'>\
                         <form action='/sent'><label>Your name <input name=name></label> \
                         <button>Send</button></form></div>"
                    ),
                ),
            ),
            p if p.starts_with("/sent?") => {
                let name = p
                    .split_once("name=")
                    .map(|(_, v)| unescape(v.split('&').next().unwrap_or("")))
                    .unwrap_or_default();
                if let Ok(mut s) = SENT.lock() {
                    s.push(name.clone());
                }
                // Shown without markup: letters, digits, and spaces.
                let shown: String = name
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
                    .collect();
                (
                    "200 OK",
                    String::new(),
                    page(
                        &format!("sent {shown}"),
                        &format!("<p>sent {shown}</p><p><a id=again href='/form'>again</a></p>"),
                    ),
                )
            }
            "/who" => {
                let who = if cookie.contains("dreamer_smoke=in") {
                    "in"
                } else {
                    "out"
                };
                ("200 OK", String::new(), page(&format!("who {who}"), who))
            }
            _ => ("404 Not Found", String::new(), "not found".to_owned()),
        };
        let mut out = &stream;
        let _ = write!(
            out,
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    /// Serve the site on a free port of 127.0.0.1, for as long as the
    /// process runs; its origin.
    pub fn start() -> Result<String, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || answer(stream));
            }
        });
        Ok(format!("127.0.0.1:{port}"))
    }
}
