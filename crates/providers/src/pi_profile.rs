//! Dreamer's pi profile: a pi agent directory (`PI_CODING_AGENT_DIR`) that
//! Dreamer lays out, versions with git, and checks before pi starts.
//!
//! - **Base layer.** The files Dreamer ships (its extension, skills, prompt
//!   templates, `APPEND_SYSTEM.md`, `settings.json`) are embedded in the
//!   binary from `pi-profile/` and written by [`Profile::sync_base`]. A
//!   manifest (`.dreamer/base.json`) remembers what Dreamer last wrote, so
//!   an update replaces only files nobody changed since. A file the agent
//!   changed is kept, and reported when Dreamer's version changed too.
//! - **History.** The directory is a git repository (when `git` is
//!   installed). Every change is a commit: Dreamer's as "dreamer base
//!   VERSION", the agent's naming its pi session. Commits that pi loaded
//!   are marked good (`refs/dreamer/good/<commit>`).
//! - **Check.** [`Profile::check`] starts `pi --mode rpc` on the profile and
//!   asks for its commands; pi exits with an error when an extension fails
//!   to load. [`Profile::ensure`] restores the last good commit when the
//!   profile no longer loads.
//! - **Credentials.** The repository ignores everything but the profile's
//!   resources, so `auth.json` and pi's sessions and caches are never
//!   committed. Nothing here reads them.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

macro_rules! base {
    ($($p:literal),* $(,)?) => {
        &[$(($p, include_str!(concat!("../pi-profile/", $p)))),*]
    };
}

/// The shipped base layer: path in the profile, and contents.
pub const BASE: &[(&str, &str)] = base![
    "APPEND_SYSTEM.md",
    "settings.json",
    "extensions/dreamer/index.ts",
    "extensions/dreamer/daemon.ts",
    "prompts/panes.md",
    "prompts/collect.md",
    "prompts/apps.md",
    "skills/dreamer-apps/SKILL.md",
    "skills/dreamer-config/SKILL.md",
    "skills/dreamer-manage/SKILL.md",
    "skills/dreamer-pi-profile/SKILL.md",
];

/// Skills that are about pi's profile itself, not for other agents.
const PI_ONLY_SKILLS: [&str; 1] = ["dreamer-pi-profile"];

/// What the repository keeps: the profile's resources only. Everything
/// else, `auth.json`, sessions, caches, `models.json` and `mcp.json`
/// (which can hold keys), stays out.
pub const GITIGNORE: &str = "\
# Dreamer's pi profile keeps only these in git. Credentials (auth.json),
# sessions, caches, models.json and mcp.json (which can hold keys) stay out.
/*
!/.gitignore
!/.dreamer/
!/extensions/
!/skills/
!/prompts/
!/themes/
!/settings.json
!/keybindings.json
!/APPEND_SYSTEM.md
!/SYSTEM.md
!/AGENTS.md
auth.json
node_modules/
.env
.env.*
*.pem
*.key
";

/// The manifest of the base layer, relative to the profile.
pub const MANIFEST: &str = ".dreamer/base.json";
/// Commits that pi loaded: `refs/dreamer/good/<commit>`.
const GOOD: &str = "refs/dreamer/good/";
/// The check's request id.
const CHECK_ID: &str = "dreamer-check";

/// Every file Dreamer writes: the base layer and the `.gitignore`.
pub fn base_files() -> Vec<(&'static str, &'static str)> {
    let mut v = BASE.to_vec();
    v.push((".gitignore", GITIGNORE));
    v
}

/// Skills Dreamer also offers other agents (`dream skills`): name, text.
pub fn shared_skills() -> Vec<(&'static str, &'static str)> {
    BASE.iter()
        .filter_map(|(p, t)| {
            let name = p.strip_prefix("skills/")?.strip_suffix("/SKILL.md")?;
            (!PI_ONLY_SKILLS.contains(&name)).then_some((name, *t))
        })
        .collect()
}

/// The SHA-256 of `bytes` in hex, as the manifest records files.
pub fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    /// The Dreamer version that last wrote the base layer.
    version: String,
    /// Path to the SHA-256 of the contents Dreamer wrote.
    files: BTreeMap<String, String>,
}

/// What [`Profile::sync_base`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Synced {
    pub version: String,
    /// Written: new, or shipped and unchanged since Dreamer wrote them.
    pub written: Vec<String>,
    /// Changed by the agent and changed in this Dreamer too: the agent's
    /// version is kept and Dreamer's is not applied.
    pub kept: Vec<String>,
    /// Removed by the agent: left removed.
    pub left_removed: Vec<String>,
    /// No longer shipped and unchanged: removed.
    pub dropped: Vec<String>,
    /// The commit recording it, when git is there and something changed.
    pub commit: Option<String>,
}

/// One commit of the profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub commit: String,
    pub time_unix: i64,
    pub subject: String,
    /// pi loaded the profile at this commit.
    pub good: bool,
}

/// What [`Profile::check`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Check {
    pub ok: bool,
    /// Why pi did not load, in pi's words.
    pub problem: Option<String>,
    /// Commands pi offered: extension commands, prompt templates, and
    /// `skill:NAME`.
    pub commands: Vec<String>,
    /// Skills and prompt templates in the profile that pi did not load
    /// (pi skips an invalid one with a warning and starts anyway).
    pub not_loaded: Vec<String>,
}

/// What [`Profile::ensure`] did.
#[derive(Debug, Clone, Default)]
pub struct Ensured {
    pub synced: Option<Synced>,
    /// Changes found outside any session, committed.
    pub committed: Option<String>,
    pub check: Check,
    /// The profile did not load and was restored: the commit restored to,
    /// and why.
    pub restored: Option<(String, String)>,
}

/// A pi agent directory that Dreamer manages.
#[derive(Debug, Clone)]
pub struct Profile {
    pub dir: PathBuf,
}

/// Whether the `git` command runs.
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

impl Profile {
    pub fn new(dir: PathBuf) -> Profile {
        Profile { dir }
    }

    /// The profile Dreamer's sessions use unless `agents.pi.agent_dir`
    /// names another directory.
    pub fn default_dir() -> Profile {
        Profile::new(crate::pi::profile_dir())
    }

    /// Dreamer has laid it out.
    pub fn exists(&self) -> bool {
        self.dir.join(MANIFEST).is_file()
    }

    /// It has history.
    pub fn versioned(&self) -> bool {
        self.dir.join(".git").exists()
    }

    /// The Dreamer version that last wrote the base layer.
    pub fn base_version(&self) -> Option<String> {
        Some(self.manifest()?.version).filter(|v| !v.is_empty())
    }

    /// The base layer was written by `version` with exactly the files this
    /// build ships.
    pub fn base_is_current(&self, version: &str) -> bool {
        let Some(m) = self.manifest() else {
            return false;
        };
        let shipped = base_files();
        m.version == version
            && m.files.len() == shipped.len()
            && shipped
                .iter()
                .all(|(rel, text)| m.files.get(*rel) == Some(&sha256(text.as_bytes())))
    }

    /// Shipped files whose contents differ from what Dreamer last wrote:
    /// the agent changed them (or removed them).
    pub fn edited_base_files(&self) -> Vec<String> {
        let Some(m) = self.manifest() else {
            return Vec::new();
        };
        m.files
            .iter()
            .filter(|(rel, sum)| {
                std::fs::read(self.dir.join(rel))
                    .map(|b| sha256(&b) != **sum)
                    .unwrap_or(true)
            })
            .map(|(rel, _)| rel.clone())
            .collect()
    }

    fn manifest(&self) -> Option<Manifest> {
        serde_json::from_str(&std::fs::read_to_string(self.dir.join(MANIFEST)).ok()?).ok()
    }

    /// `git` in the profile, with Dreamer's identity and no hooks, signing,
    /// or repository settings inherited from the environment.
    fn git_command(&self) -> Command {
        let mut c = Command::new("git");
        c.arg("-C")
            .arg(&self.dir)
            .args([
                "-c",
                "user.name=Dreamer",
                "-c",
                "user.email=dreamer@localhost",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.autocrlf=false",
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null());
        for v in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_PREFIX",
        ] {
            c.env_remove(v);
        }
        c
    }

    fn git(&self, args: &[&str]) -> Result<String, String> {
        let out = self
            .git_command()
            .args(args)
            .output()
            .map_err(|e| format!("git: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!(
                "git {}: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn head(&self) -> Option<String> {
        self.git(&["rev-parse", "--verify", "-q", "HEAD"])
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    /// Lay out the profile, or bring its base layer up to `version`.
    /// Starts the repository if git is installed.
    pub fn sync_base(&self, version: &str) -> Result<Synced, String> {
        self.sync_files(&base_files(), version)
    }

    /// [`Profile::sync_base`] with these files as the base layer.
    fn sync_files(&self, files: &[(&str, &str)], version: &str) -> Result<Synced, String> {
        std::fs::create_dir_all(self.dir.join(".dreamer"))
            .map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let git = git_available();
        if git && !self.versioned() {
            self.git(&["init", "-q", "-b", "main"])?;
        }
        if git {
            // The agent's own work comes first, as its own commit.
            self.commit_all("changes outside a pi session", None)?;
        }
        let old = self.manifest().unwrap_or_default();
        let mut new = Manifest {
            version: version.to_owned(),
            files: BTreeMap::new(),
        };
        let mut done = Synced {
            version: version.to_owned(),
            ..Synced::default()
        };
        for &(rel, text) in files {
            let path = self.dir.join(rel);
            let want = sha256(text.as_bytes());
            let recorded = old.files.get(rel);
            let current = std::fs::read(&path).ok().map(|b| sha256(&b));
            let write = match (&current, recorded) {
                (None, None) => true,
                (None, Some(_)) => {
                    done.left_removed.push(rel.to_owned());
                    false
                }
                (Some(c), _) if *c == want => false,
                (Some(c), Some(r)) if c == r => true,
                (Some(_), r) => {
                    if r != Some(&want) {
                        done.kept.push(rel.to_owned());
                    }
                    false
                }
            };
            if write {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
                done.written.push(rel.to_owned());
            }
            new.files.insert(rel.to_owned(), want);
        }
        for (rel, recorded) in &old.files {
            if new.files.contains_key(rel) {
                continue;
            }
            let path = self.dir.join(rel);
            if std::fs::read(&path).ok().map(|b| sha256(&b)).as_ref() == Some(recorded) {
                std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                done.dropped.push(rel.clone());
            }
        }
        let text = serde_json::to_string_pretty(&new).map_err(|e| e.to_string())?;
        let manifest = self.dir.join(MANIFEST);
        std::fs::write(&manifest, format!("{text}\n"))
            .map_err(|e| format!("{}: {e}", manifest.display()))?;
        if git {
            let mut body = String::new();
            for (what, list) in [
                ("written", &done.written),
                ("kept (changed by the agent)", &done.kept),
                ("left removed", &done.left_removed),
                ("dropped", &done.dropped),
            ] {
                if !list.is_empty() {
                    body.push_str(&format!("{what}: {}\n", list.join(", ")));
                }
            }
            done.commit = self.commit_with(&format!("dreamer base {version}"), &body)?;
        }
        Ok(done)
    }

    /// Commit every change as `subject`, with the changed files listed.
    fn commit_all(&self, subject: &str, session: Option<&str>) -> Result<Option<String>, String> {
        let mut body = String::new();
        if let Some(s) = session {
            body.push_str(&format!("Pi-Session: {s}\n"));
        }
        self.commit_with(subject, &body)
    }

    fn commit_with(&self, subject: &str, body: &str) -> Result<Option<String>, String> {
        self.git(&["add", "-A"])?;
        let changed = self.git(&["diff", "--cached", "--name-status"])?;
        if changed.trim().is_empty() {
            return Ok(None);
        }
        let files: Vec<String> = changed.lines().map(|l| l.replace('\t', " ")).collect();
        let message = format!("{subject}\n\n{}\n{body}", files.join("\n"));
        self.git(&["commit", "-q", "-m", message.trim_end()])?;
        Ok(self.head().map(|h| h.chars().take(12).collect()))
    }

    /// Commit the agent's changes: `message` (or a default naming the
    /// session), with the session id as a trailer. `None` when nothing
    /// changed.
    pub fn commit(
        &self,
        message: Option<&str>,
        session: Option<&str>,
    ) -> Result<Option<String>, String> {
        self.require_history()?;
        let default = match session {
            Some(s) => format!("pi session {s}"),
            None => "changes outside a pi session".to_owned(),
        };
        let subject = message
            .map(|m| m.lines().next().unwrap_or_default().trim().to_owned())
            .filter(|m| !m.is_empty())
            .unwrap_or(default);
        self.commit_all(&subject, session)
    }

    fn require_history(&self) -> Result<(), String> {
        if !self.versioned() {
            return Err(if git_available() {
                format!(
                    "{} has no history; run `dream agents profile init`",
                    self.dir.display()
                )
            } else {
                "git is not installed, so the profile has no history".into()
            });
        }
        Ok(())
    }

    /// The newest `limit` commits, newest first.
    pub fn log(&self, limit: usize) -> Result<Vec<LogEntry>, String> {
        self.require_history()?;
        if self.head().is_none() {
            return Ok(Vec::new());
        }
        let good = self.good_commits()?;
        let n = format!("-n{limit}");
        let out = self.git(&["log", &n, "--format=%H%x09%ct%x09%s"])?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(3, '\t');
                let commit = f.next()?.to_owned();
                let time_unix = f.next()?.parse().unwrap_or(0);
                let subject = f.next().unwrap_or_default().to_owned();
                Some(LogEntry {
                    good: good.contains(&commit),
                    commit: commit.chars().take(12).collect(),
                    time_unix,
                    subject,
                })
            })
            .collect())
    }

    fn good_commits(&self) -> Result<Vec<String>, String> {
        let out = self.git(&["for-each-ref", "--format=%(objectname)", GOOD])?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    /// The newest commit in the history that pi loaded.
    pub fn last_good(&self) -> Option<String> {
        let good = self.good_commits().ok()?;
        let all = self.git(&["rev-list", "HEAD"]).ok()?;
        all.lines()
            .find(|c| good.iter().any(|g| g == c))
            .map(str::to_owned)
    }

    /// Mark the current commit as one pi loaded, if nothing is uncommitted.
    pub fn mark_good(&self) -> Result<(), String> {
        if !self.versioned() || !self.uncommitted()?.is_empty() {
            return Ok(());
        }
        if let Some(h) = self.head() {
            self.git(&["update-ref", &format!("{GOOD}{h}"), &h])?;
        }
        Ok(())
    }

    /// Uncommitted changes, as `git status --short` lists them.
    pub fn uncommitted(&self) -> Result<Vec<String>, String> {
        self.require_history()?;
        Ok(self
            .git(&["status", "--short", "--untracked-files=all"])?
            .lines()
            .map(str::to_owned)
            .collect())
    }

    /// With `rev`, that commit's change; without, what is not committed.
    pub fn diff(&self, rev: Option<&str>) -> Result<String, String> {
        self.require_history()?;
        match rev {
            Some(r) => self.git(&["show", "--stat", "--patch", "--format=medium", r, "--"]),
            None => {
                let mut out = self.git(&["status", "--short", "--untracked-files=all"])?;
                if self.head().is_some() {
                    out.push_str(&self.git(&["diff", "HEAD"])?);
                }
                Ok(out)
            }
        }
    }

    /// Undo one commit with a new commit; the history keeps both.
    pub fn revert(&self, rev: &str) -> Result<String, String> {
        self.require_history()?;
        if rev.starts_with('-') {
            return Err(format!("{rev:?} is not a commit"));
        }
        self.commit_all("changes outside a pi session", None)?;
        if let Err(e) = self.git(&["revert", "--no-edit", rev]) {
            let _ = self.git(&["revert", "--abort"]);
            return Err(format!(
                "{e}\nnothing changed; a later commit depends on this one, revert that first"
            ));
        }
        Ok(self
            .head()
            .map(|h| h.chars().take(12).collect())
            .unwrap_or_default())
    }

    /// Make the files what they were at `rev`, as a new commit.
    pub fn restore(&self, rev: &str, why: &str) -> Result<Option<String>, String> {
        self.require_history()?;
        self.git(&["read-tree", "-u", "--reset", rev])?;
        let short: String = rev.chars().take(12).collect();
        self.commit_with(&format!("restore {short}: {why}"), "")
    }

    /// Start pi on the profile once, the way a session starts, and see
    /// whether it loads. `program` is the pi executable.
    pub fn check(&self, program: &Path) -> Result<Check, String> {
        let first = run_check(program, &self.dir)?;
        if first.ok {
            let mut c = first;
            c.not_loaded = self.not_loaded(&c.commands);
            return Ok(c);
        }
        // Tell a profile that does not load from a pi that does not run.
        let empty = scratch_dir("empty")?;
        let baseline = run_check(program, &empty);
        let _ = std::fs::remove_dir_all(&empty);
        match baseline {
            Ok(b) if b.ok => Ok(first),
            Ok(b) => Err(format!(
                "pi does not start even without a profile: {}",
                b.problem.unwrap_or_default()
            )),
            Err(e) => Err(e),
        }
    }

    /// Skills and prompt templates in the profile that pi did not offer.
    fn not_loaded(&self, commands: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(self.dir.join("skills"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().join("SKILL.md").is_file()
                && !commands.iter().any(|c| *c == format!("skill:{name}"))
            {
                out.push(format!("skills/{name}"));
            }
        }
        for e in std::fs::read_dir(self.dir.join("prompts"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let file = e.file_name().to_string_lossy().into_owned();
            if let Some(stem) = file.strip_suffix(".md")
                && !commands.iter().any(|c| c == stem)
            {
                out.push(format!("prompts/{file}"));
            }
        }
        out.sort();
        out
    }

    /// Before pi starts: lay out or update the base layer for `version`,
    /// commit what changed outside a session, check that pi loads, and
    /// restore the last commit that loaded when it does not.
    pub fn ensure(&self, program: &Path, version: &str) -> Result<Ensured, String> {
        let mut done = Ensured::default();
        if !self.base_is_current(version) {
            done.synced = Some(self.sync_base(version)?);
        }
        if self.versioned() {
            done.committed = self.commit_all("changes outside a pi session", None)?;
        }
        done.check = self.check(program)?;
        if done.check.ok {
            self.mark_good()?;
            return Ok(done);
        }
        let problem = done
            .check
            .problem
            .as_deref()
            .and_then(|p| p.lines().next())
            .unwrap_or("no answer")
            .trim_start_matches("Error: ")
            .replace(&format!("{}/", self.dir.display()), "");
        let mut why = format!("pi did not load: {problem}");
        if why.chars().count() > 160 {
            why = why.chars().take(157).collect::<String>() + "...";
        }
        let good = if self.versioned() {
            self.last_good()
        } else {
            None
        };
        if let Some(good) = good
            && Some(&good) != self.head().as_ref()
        {
            self.restore(&good, &why)?;
            done.restored = Some((good.chars().take(12).collect(), why));
            done.check = self.check(program)?;
            if done.check.ok {
                self.mark_good()?;
            }
        }
        Ok(done)
    }
}

fn scratch_dir(what: &str) -> Result<PathBuf, String> {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!(
        "dreamer-pi-check-{what}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d)
}

/// `pi --mode rpc` on `agent_dir`: ask for its commands, then end its input.
/// pi exits with an error, before answering, when an extension fails to
/// load; it warns about an unreadable settings file and starts anyway,
/// which counts as not loading here (the settings would be lost).
fn run_check(program: &Path, agent_dir: &Path) -> Result<Check, String> {
    if !program.is_file() {
        return Err(format!("{} does not exist", program.display()));
    }
    let cwd = scratch_dir("cwd")?;
    let mut c = crate::pi::pi_command(program);
    c.args(["--mode", "rpc", "--no-session", "--no-approve"])
        .current_dir(&cwd)
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("DREAMER_PI_CHECK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for v in crate::pi::SCRUBBED {
        c.env_remove(v);
    }
    let result = (|| {
        let mut child = c
            .spawn()
            .map_err(|e| format!("{}: {e}", program.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "{{\"id\":\"{CHECK_ID}\",\"type\":\"get_commands\"}}");
        }
        let pid = child.id();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(r) => r.map_err(|e| format!("{}: {e}", program.display()))?,
            Err(_) => {
                let _ = platform::terminate(pid);
                return Ok(Check {
                    problem: Some("pi did not answer within 60 s".into()),
                    ..Check::default()
                });
            }
        };
        let stderr = String::from_utf8_lossy(&out.stderr);
        let mut commands = None;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v["id"] == CHECK_ID && v["success"] == true {
                commands = Some(
                    v["data"]["commands"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c["name"].as_str().map(str::to_owned))
                        .collect::<Vec<_>>(),
                );
            }
        }
        let settings = stderr
            .lines()
            .find(|l| l.contains("Invalid settings file"))
            .map(str::to_owned);
        let problem = || {
            let text: Vec<&str> = stderr
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(12)
                .collect();
            if text.is_empty() {
                format!("pi exited with {} and no answer", out.status)
            } else {
                text.join("\n")
            }
        };
        Ok(match (commands, settings) {
            (Some(commands), None) if out.status.success() => Check {
                ok: true,
                commands,
                ..Check::default()
            },
            (_, Some(s)) => Check {
                problem: Some(s),
                ..Check::default()
            },
            _ => Check {
                problem: Some(problem()),
                ..Check::default()
            },
        })
    })();
    let _ = std::fs::remove_dir_all(&cwd);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every file under `pi-profile/` is embedded, and nothing else.
    #[test]
    fn the_embedded_base_layer_is_the_whole_directory() {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).expect("read_dir").flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(root, &p, out);
                } else {
                    let rel = p.strip_prefix(root).expect("prefix");
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("pi-profile");
        let mut on_disk = Vec::new();
        walk(&root, &root, &mut on_disk);
        on_disk.sort();
        let mut embedded: Vec<String> = BASE.iter().map(|(p, _)| (*p).to_owned()).collect();
        embedded.sort();
        assert_eq!(embedded, on_disk);
        for want in [
            "extensions/dreamer/index.ts",
            "APPEND_SYSTEM.md",
            "settings.json",
            "prompts/panes.md",
            "prompts/collect.md",
            "prompts/apps.md",
        ] {
            assert!(embedded.iter().any(|p| p == want), "{want}");
        }
        let settings: serde_json::Value = serde_json::from_str(
            BASE.iter()
                .find(|(p, _)| *p == "settings.json")
                .expect("s")
                .1,
        )
        .expect("settings.json is JSON");
        assert!(settings.is_object());
        // Skills need YAML front matter with a name matching the directory.
        for (p, text) in BASE {
            if let Some(name) = p
                .strip_prefix("skills/")
                .and_then(|s| s.strip_suffix("/SKILL.md"))
            {
                assert!(text.contains(&format!("\nname: {name}\n")), "{p}");
                let desc = text
                    .lines()
                    .find_map(|l| l.strip_prefix("description: "))
                    .expect("description");
                // A plain YAML scalar cannot hold ": "; pi skips such a skill.
                assert!(
                    !desc.contains(": ") || desc.starts_with('"'),
                    "{p}: quote the description"
                );
            }
        }
        assert_eq!(shared_skills().len(), 3);
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("drm-profile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Dreamer's update replaces only what nobody changed; the agent's
    /// edits, removals, and additions stay, and every step is a commit.
    #[test]
    fn updates_keep_the_agents_changes() {
        let dir = temp("merge");
        let p = Profile::new(dir.clone());
        let read = |rel: &str| std::fs::read_to_string(dir.join(rel)).unwrap_or_default();
        let v1 = [
            ("a.md", "a1"),
            ("b.md", "b1"),
            ("c.md", "c1"),
            ("same.md", "s"),
        ];
        let s = p.sync_files(&v1, "1").expect("v1");
        assert_eq!(s.written.len(), 4);
        assert_eq!(p.base_version().as_deref(), Some("1"));

        std::fs::write(dir.join("b.md"), "b by the agent").expect("edit");
        std::fs::write(dir.join("same.md"), "s by the agent").expect("edit");
        std::fs::remove_file(dir.join("c.md")).expect("rm");
        std::fs::write(dir.join("mine.md"), "the agent's own").expect("add");
        assert_eq!(p.edited_base_files(), ["b.md", "c.md", "same.md"]);

        let v2 = [
            ("a.md", "a2"),
            ("b.md", "b2"),
            ("c.md", "c2"),
            ("same.md", "s"),
        ];
        let s = p.sync_files(&v2, "2").expect("v2");
        assert_eq!(s.written, ["a.md"]);
        // Dreamer changed b too: reported. `same.md` it did not: silent.
        assert_eq!(s.kept, ["b.md"]);
        assert_eq!(s.left_removed, ["c.md"]);
        assert_eq!(read("a.md"), "a2");
        assert_eq!(read("b.md"), "b by the agent");
        assert_eq!(read("same.md"), "s by the agent");
        assert_eq!(read("mine.md"), "the agent's own");
        assert!(!dir.join("c.md").exists());

        // No longer shipped: removed when unchanged, kept when edited.
        let v3 = [("c.md", "c2")];
        let s = p.sync_files(&v3, "3").expect("v3");
        assert_eq!(s.dropped, ["a.md"]);
        assert!(dir.join("b.md").exists());

        if git_available() {
            let subjects: Vec<String> = p
                .log(10)
                .expect("log")
                .into_iter()
                .map(|e| e.subject)
                .collect();
            assert_eq!(
                subjects,
                [
                    "dreamer base 3",
                    "dreamer base 2",
                    "changes outside a pi session",
                    "dreamer base 1",
                ]
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Credentials and pi's own state never enter the history.
    #[test]
    fn the_history_leaves_credentials_out() {
        if !git_available() {
            eprintln!("git is not installed; skipped");
            return;
        }
        let dir = temp("ignore");
        let p = Profile::new(dir.clone());
        p.sync_base("1").expect("init");
        for f in [
            "auth.json",
            "models.json",
            "mcp.json",
            "models-store.json",
            "sessions/x.jsonl",
            "extensions/mine/auth.json",
            "extensions/mine/.env",
            "extensions/mine/node_modules/m/index.js",
        ] {
            let path = dir.join(f);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(&path, "secret").expect("write");
        }
        std::fs::write(
            dir.join("extensions/mine/index.ts"),
            "export default () => {}",
        )
        .expect("w");
        let c = p.commit(Some("add mine"), Some("s-1")).expect("commit");
        assert!(c.is_some());
        let files = p.git(&["ls-files"]).expect("ls-files");
        assert!(
            files.lines().any(|f| f == "extensions/mine/index.ts"),
            "{files}"
        );
        for secret in [
            "auth.json",
            "models.json",
            "mcp.json",
            "sessions",
            ".env",
            "node_modules",
        ] {
            assert!(!files.contains(secret), "{secret} committed:\n{files}");
        }
        let body = p.git(&["log", "-1", "--format=%B"]).expect("log");
        assert!(body.starts_with("add mine\n"), "{body}");
        assert!(body.contains("Pi-Session: s-1"), "{body}");
        assert!(body.contains("extensions/mine/index.ts"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
