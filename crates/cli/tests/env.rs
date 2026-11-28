//! `dream env` on a private configuration directory. git runs with no
//! global or system configuration and an explicit identity, so nothing
//! here reads or changes the user's git setup.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DREAM: &str = env!("CARGO_BIN_EXE_dream");

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let root = PathBuf::from(format!("/tmp/drm-env-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        std::fs::create_dir_all(root.join("home")).expect("home");
        Env { root }
    }

    fn config(&self) -> PathBuf {
        self.root.join("config")
    }

    fn isolate(&self, c: &mut Command) {
        c.env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .env("DREAMER_CONFIG_DIR", self.config())
            .env("DREAMER_STATE_DIR", self.root.join("state"))
            .env("DREAMER_DATA_DIR", self.root.join("data"))
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("USER", "tester");
    }

    fn dream(&self, args: &[&str]) -> Output {
        let mut c = Command::new(DREAM);
        c.args(args);
        self.isolate(&mut c);
        c.output().expect("dream")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.dream(args);
        assert!(
            out.status.success(),
            "dream {args:?} failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.dream(args);
        assert!(
            !out.status.success(),
            "dream {args:?} should have failed:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    /// git in `dir`, isolated like `dream`.
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut c = Command::new("git");
        c.arg("-C").arg(dir).args(args);
        self.isolate(&mut c);
        let out = c.output().expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.config().join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
        std::fs::write(p, text).expect("write");
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const WORKSPACE: &str = "version: 1\nwindows:\n  - name: main\n    layout:\n      shell: true\n";

#[test]
fn init_edit_status_save_log_diff() {
    let e = Env::new("cycle");
    e.write("dreamer.yaml", "font:\n  size: 14\n");
    e.write("hosts.yaml", "hosts: []\n");
    e.write("memory/note.md", "private\n");

    let out = e.ok(&["env", "init"]);
    assert!(out.contains("is now an environment"), "{out}");
    let env_toml = std::fs::read_to_string(e.config().join("env.toml")).expect("env.toml");
    assert!(env_toml.contains("name = \"tester\""), "{env_toml}");
    let tracked = e.git(&e.config(), &["ls-files"]);
    assert!(
        tracked.contains("dreamer.yaml") && tracked.contains("env.toml"),
        "{tracked}"
    );
    assert!(
        !tracked.contains("hosts.yaml") && !tracked.contains("memory/"),
        "private files are never saved:\n{tracked}"
    );
    // Again: nothing to do.
    assert!(e.ok(&["env", "init"]).contains("already an environment"));

    assert!(
        e.ok(&["env", "status"])
            .contains("no changes since the last save")
    );
    e.write("dreamer.yaml", "font:\n  size: 15\n");
    e.write("workspaces/dev.yaml", WORKSPACE);
    let status = e.ok(&["env", "status"]);
    assert!(
        status.contains("settings") && status.contains("~ dreamer.yaml"),
        "{status}"
    );
    assert!(
        status.contains("+ workspaces/dev.yaml") && status.contains("validates"),
        "{status}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&e.ok(&["env", "status", "--json"])).expect("json");
    assert_eq!(json["valid"], true);
    assert_eq!(json["changes"].as_array().map(Vec::len), Some(2), "{json}");

    let diff = e.ok(&["env", "diff"]);
    assert!(
        diff.contains("~ dreamer.yaml") && diff.contains("+ workspaces/dev.yaml"),
        "{diff}"
    );
    let patch = e.ok(&["env", "diff", "--patch"]);
    assert!(
        patch.contains("+  size: 15") && patch.contains("+windows:"),
        "{patch}"
    );

    let saved = e.ok(&["env", "save"]);
    assert!(
        saved.contains("settings; workspace dev"),
        "generated message: {saved}"
    );
    assert!(e.ok(&["env", "save"]).contains("nothing changed"));

    e.write("themes/mine.toml", "x");
    e.fails(&["env", "save"]);
    std::fs::remove_file(e.config().join("themes/mine.toml")).expect("rm");
    e.write("workspaces/dev.yaml", "windows: [\n");
    let refused = e.fails(&["env", "save", "-m", "broken"]);
    assert!(
        refused.contains("does not validate") && refused.contains("dev.yaml"),
        "{refused}"
    );
    assert!(
        e.ok(&["env", "save", "-m", "broken on purpose", "--force"])
            .contains("saved")
    );

    let log = e.ok(&["env", "log"]);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3, "{log}");
    assert!(lines[0].ends_with("broken on purpose"), "{log}");
    assert!(lines[2].ends_with("Start the environment"), "{log}");
    let first = lines[2].split_whitespace().next().expect("rev");
    let since = e.ok(&["env", "diff", first]);
    assert!(
        since.contains("dreamer.yaml") && since.contains("workspaces/dev.yaml"),
        "{since}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&e.ok(&["env", "log", "-n", "1", "--json"])).expect("json");
    assert_eq!(parsed.as_array().map(Vec::len), Some(1));
    assert!(e.fails(&["env", "diff", "nope"]).contains("not a save"));
}

#[test]
fn an_existing_repository_is_adopted_with_its_history() {
    let e = Env::new("adopt");
    e.write("dreamer.yaml", "font:\n  size: 13\n");
    e.write(".gitignore", "secret.txt\n");
    e.git(&e.config(), &["init", "-q", "-b", "trunk"]);
    e.git(&e.config(), &["add", "-A"]);
    e.git(&e.config(), &["commit", "-q", "-m", "mine"]);
    e.write("themes/wip.toml", "unsaved work");

    let out = e.ok(&["env", "init", "--name", "laptop"]);
    assert!(out.contains("adopted"), "{out}");
    let log = e.git(&e.config(), &["log", "--format=%s"]);
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        ["Version this directory as a Dreamer environment", "mine"],
        "history kept, one commit added"
    );
    let ignore = std::fs::read_to_string(e.config().join(".gitignore")).expect("ignore");
    assert!(
        ignore.starts_with("secret.txt\n") && ignore.contains("/hosts.yaml"),
        "{ignore}"
    );
    let status = e.ok(&["env", "status"]);
    assert!(
        status.contains("on trunk") && status.contains("+ themes/wip.toml"),
        "the user's work is left alone: {status}"
    );
}

#[test]
fn a_directory_inside_another_repository_needs_nested() {
    let e = Env::new("nested");
    e.git(&e.root, &["init", "-q"]);
    let refused = e.fails(&["env", "init"]);
    assert!(refused.contains("--nested"), "{refused}");
    assert!(!e.config().join("env.toml").exists(), "nothing written");
    assert!(
        e.ok(&["env", "init", "--nested"])
            .contains("now an environment")
    );
    assert!(e.config().join(".git").exists());
}

#[test]
fn a_merge_in_progress_or_a_detached_head_stops_a_save() {
    let e = Env::new("blocked");
    e.ok(&["env", "init"]);
    let head = e.git(&e.config(), &["rev-parse", "HEAD"]);
    e.git(&e.config(), &["checkout", "-q", head.trim()]);
    e.write("dreamer.yaml", "font:\n  size: 15\n");
    let detached = e.fails(&["env", "save"]);
    assert!(detached.contains("detached"), "{detached}");
    e.git(&e.config(), &["checkout", "-q", "main"]);
    let git_dir = e.config().join(".git");
    std::fs::write(git_dir.join("MERGE_HEAD"), head).expect("merge head");
    let merging = e.fails(&["env", "save"]);
    assert!(merging.contains("merge is under way"), "{merging}");
    std::fs::remove_file(git_dir.join("MERGE_HEAD")).expect("rm");
    assert!(e.ok(&["env", "save"]).contains("saved"));
}

#[test]
fn commands_before_init_say_how_to_start() {
    let e = Env::new("fresh");
    let out = e.fails(&["env", "status"]);
    assert!(out.contains("dream env init"), "{out}");
}

#[test]
fn restore_brings_back_an_earlier_save_and_can_be_undone() {
    let e = Env::new("restore");
    e.write("dreamer.yaml", "font:\n  size: 14\n");
    e.write("workspaces/old.yaml", WORKSPACE);
    e.write("hosts.yaml", "hosts: []\n");
    e.ok(&["env", "init"]);
    let first = e.git(&e.config(), &["rev-parse", "--short", "HEAD"]);
    let first = first.trim();

    e.write("dreamer.yaml", "font:\n  size: 18\n");
    std::fs::remove_file(e.config().join("workspaces/old.yaml")).expect("rm");
    e.write("workspaces/new.yaml", WORKSPACE);
    e.ok(&["env", "save", "-m", "bigger font"]);
    e.write("dreamer.yaml", "font:\n  size: 20\n");
    let private = "hosts:\n  - name: secret-box\n";
    e.write("hosts.yaml", private);

    let dry = e.ok(&["env", "restore", first, "--dry-run"]);
    assert!(
        dry.contains("~ dreamer.yaml") && dry.contains("+ workspaces/old.yaml"),
        "{dry}"
    );
    assert!(
        std::fs::read_to_string(e.config().join("dreamer.yaml"))
            .expect("read")
            .contains("20"),
        "a dry run changes nothing"
    );

    let out = e.ok(&["env", "restore", first]);
    assert!(out.contains("saved what was there first"), "{out}");
    let settings = std::fs::read_to_string(e.config().join("dreamer.yaml")).expect("read");
    assert!(settings.contains("size: 14"), "{settings}");
    assert!(
        e.config().join("workspaces/old.yaml").exists(),
        "a removed file comes back"
    );
    assert!(
        !e.config().join("workspaces/new.yaml").exists(),
        "a file the save lacks goes"
    );
    assert_eq!(
        std::fs::read_to_string(e.config().join("hosts.yaml")).expect("hosts"),
        private,
        "ignored files are never touched"
    );
    let log = e.ok(&["env", "log"]);
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines[0].contains(&format!("Restore {first}")), "{log}");
    assert!(
        lines[1].contains(&format!("Before restoring {first}")),
        "{log}"
    );
    assert_eq!(
        lines.len(),
        4,
        "history is added to, never rewritten:\n{log}"
    );

    // Undo: restore the save made before.
    let before = lines[1].split_whitespace().next().expect("rev");
    e.ok(&["env", "restore", before]);
    let settings = std::fs::read_to_string(e.config().join("dreamer.yaml")).expect("read");
    assert!(settings.contains("size: 20"), "undone: {settings}");
    assert!(e.config().join("workspaces/new.yaml").exists());
}

#[test]
fn restore_refuses_an_unknown_or_invalid_save_and_changes_nothing() {
    let e = Env::new("restore-bad");
    e.write("dreamer.yaml", "font:\n  size: 14\n");
    e.ok(&["env", "init"]);
    e.write("workspaces/dev.yaml", "windows: [\n");
    e.ok(&["env", "save", "-m", "broken", "--force"]);
    let broken = e.git(&e.config(), &["rev-parse", "--short", "HEAD"]);
    e.write("workspaces/dev.yaml", WORKSPACE);
    e.ok(&["env", "save", "-m", "fixed"]);
    e.write("dreamer.yaml", "font:\n  size: 16\n");

    let head = e.git(&e.config(), &["rev-parse", "HEAD"]);
    let refused = e.fails(&["env", "restore", broken.trim()]);
    assert!(
        refused.contains("does not validate") && refused.contains("workspaces/dev.yaml"),
        "{refused}"
    );
    assert!(e.fails(&["env", "restore", "nope"]).contains("not a save"));
    assert_eq!(
        e.git(&e.config(), &["rev-parse", "HEAD"]),
        head,
        "nothing saved"
    );
    assert!(
        std::fs::read_to_string(e.config().join("dreamer.yaml"))
            .expect("read")
            .contains("16"),
        "unsaved work untouched"
    );
    let siblings: Vec<String> = std::fs::read_dir(&e.root)
        .expect("root")
        .flatten()
        .map(|d| d.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("restore"))
        .collect();
    assert!(
        siblings.is_empty(),
        "the scratch copy is removed: {siblings:?}"
    );
}
