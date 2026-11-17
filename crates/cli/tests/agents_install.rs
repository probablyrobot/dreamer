//! `dream agents update|status|rollback` against a release server on disk:
//! a stand-in pi, packed and listed the way upstream publishes releases,
//! fetched through `file://` URLs. Nothing is downloaded from the network,
//! and every directory is private to the test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DREAM: &str = env!("CARGO_BIN_EXE_dream");

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let root = PathBuf::from(format!("/tmp/drm-agents-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).expect("root");
        Env { root }
    }

    fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    fn pi_root(&self) -> PathBuf {
        self.data().join("agents/pi")
    }

    /// Publish `version` of the stand-in pi; `bad_sum` lists a wrong
    /// checksum for it.
    fn publish(&self, version: &str, bad_sum: bool) {
        let asset = providers::pi::asset().expect("a platform pi publishes for");
        let pkg = self.root.join(format!("pkg-{version}/pi"));
        std::fs::create_dir_all(&pkg).expect("pkg");
        // `auth check` answers as pi does, and shows what it was given: the
        // agent directory, and whether an API key reached it.
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo {version} ;;\nauth) printf '{{\"status\":\"not_ready\",\"provider\":\"%s\",\"reason\":\"dir=%s key=%s\"}}\\n' \"$4\" \"$PI_CODING_AGENT_DIR\" \"${{OPENAI_API_KEY:+set}}\"; exit 1 ;;\nesac\n"
        );
        let exe = pkg.join("pi");
        std::fs::write(&exe, script).expect("pi");
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let rel = self.root.join(format!("releases/v{version}"));
        std::fs::create_dir_all(&rel).expect("rel");
        let archive = rel.join(&asset);
        let ok = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(pkg.parent().expect("parent"))
            .arg("pi")
            .status()
            .expect("tar");
        assert!(ok.success());
        let sum = if bad_sum {
            "0".repeat(64)
        } else {
            sha256(&archive)
        };
        std::fs::write(
            rel.join("SHA256SUMS"),
            format!("{sum}  {asset}\n{}  pi-other.tar.gz\n", "1".repeat(64)),
        )
        .expect("sums");
    }

    fn set_latest(&self, version: &str) {
        std::fs::write(
            self.root.join("latest.json"),
            format!("{{\"schemaVersion\":1,\"version\":\"{version}\"}}"),
        )
        .expect("latest");
    }

    fn settings(&self, yaml: &str) {
        std::fs::write(self.root.join("config/dreamer.yaml"), yaml).expect("settings");
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(DREAM)
            .args(args)
            .env("DREAMER_DATA_DIR", self.data())
            .env("DREAMER_CONFIG_DIR", self.root.join("config"))
            .env("DREAMER_RUNTIME_DIR", self.root.join("rt"))
            .env("DREAMER_STATE_DIR", self.root.join("st"))
            .env("DREAMER_FORBID_DEFAULT_TMUX", "1")
            .env(
                "DREAMER_PI_LATEST_URL",
                format!("file://{}", self.root.join("latest.json").display()),
            )
            .env(
                "DREAMER_PI_DOWNLOAD_URL",
                format!("file://{}", self.root.join("releases").display()),
            )
            .env("OPENAI_API_KEY", "sk-not-for-pi")
            // The system tools only: no real claude, codex, or pi is found.
            .env("PATH", "/usr/bin:/bin")
            .env_remove("DREAMER_CLAUDE")
            .env_remove("DREAMER_CODEX")
            .output()
            .expect("dream")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "dream {args:?} failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn status(&self) -> serde_json::Value {
        serde_json::from_str(&self.ok(&["agents", "status", "--json"])).expect("json")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn sha256(path: &Path) -> String {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .or_else(|_| Command::new("sha256sum").arg(path).output())
        .expect("shasum or sha256sum");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .expect("sum")
        .to_owned()
}

#[test]
fn pi_is_installed_updated_rolled_back_and_a_bad_download_changes_nothing() {
    let e = Env::new("install");
    for v in ["1.0.0", "1.1.0", "1.2.1"] {
        e.publish(v, false);
    }
    e.publish("1.2.0", true);
    e.set_latest("1.1.0");

    // Nothing installed yet.
    let s = e.status();
    assert_eq!(s["installed"], serde_json::json!([]), "{s}");
    assert_eq!(s["runs"], false);
    assert!(e.ok(&["agents", "status"]).contains("dream agents update"));

    // The newest release.
    let out = e.ok(&["agents", "update"]);
    assert!(out.contains("pi 1.1.0 is active"), "{out}");
    let s = e.status();
    assert_eq!(
        (s["active"].as_str(), s["runs"].as_bool()),
        (Some("1.1.0"), Some(true)),
        "{s}"
    );
    assert_eq!(s["version"], "1.1.0");
    assert_eq!(s["default"], "pi");
    // Login comes from pi itself, in Dreamer's agent directory, and the
    // API key in the environment never reaches it.
    let profile = e.data().join("pi-profile").display().to_string();
    for l in s["logins"].as_array().expect("logins") {
        assert_eq!(l["status"], "not_ready", "{s}");
        assert_eq!(l["reason"], format!("dir={profile} key="), "{s}");
    }
    assert_eq!(
        std::fs::read_link(e.pi_root().join("current")).expect("current"),
        PathBuf::from("1.1.0")
    );

    // A fixed release; the one before is kept.
    let out = e.ok(&["agents", "update", "--version", "1.0.0"]);
    assert!(out.contains("1.1.0 is kept"), "{out}");
    let s = e.status();
    assert_eq!(
        (s["active"].as_str(), s["previous"].as_str()),
        (Some("1.0.0"), Some("1.1.0"))
    );

    // Rollback returns to it, and keeps the other.
    e.ok(&["agents", "rollback", "--harness", "pi"]);
    let s = e.status();
    assert_eq!(
        (s["active"].as_str(), s["previous"].as_str()),
        (Some("1.1.0"), Some("1.0.0"))
    );

    // A download that fails its checksum changes nothing.
    let out = e.run(&["agents", "update", "--version", "1.2.0"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("checksum"), "{err}");
    let s = e.status();
    assert_eq!(
        (s["active"].as_str(), s["previous"].as_str()),
        (Some("1.1.0"), Some("1.0.0"))
    );
    assert!(!e.pi_root().join("1.2.0").exists());

    // A release pinned in the settings; only two releases stay on disk.
    e.settings("agents:\n  pi:\n    version: 1.2.1\n");
    e.ok(&["agents", "update"]);
    let s = e.status();
    assert_eq!(
        (s["active"].as_str(), s["previous"].as_str()),
        (Some("1.2.1"), Some("1.1.0"))
    );
    assert_eq!(s["installed"], serde_json::json!(["1.1.0", "1.2.1"]));
    assert_eq!(s["version_setting"], "1.2.1");

    // Updating to the active release is a no-op.
    assert!(
        e.ok(&["agents", "update"])
            .contains("already the active release")
    );

    // Only pi is managed.
    assert!(
        !e.run(&["agents", "update", "--harness", "claude"])
            .status
            .success()
    );
}

#[test]
fn config_check_reports_each_harness_and_refuses_a_bad_block() {
    let e = Env::new("check");
    e.settings("agents:\n  default: mine\n  mine:\n    protocol: acp\n    command: [/bin/sh, -c, 'true']\n  codex:\n    command: [no-such-codex-here]\n");
    let out = e.ok(&["config", "check"]);
    assert!(out.contains("agent harnesses (default: mine)"), "{out}");
    assert!(out.contains("✓ mine (default): /bin/sh"), "{out}");
    assert!(out.contains("· codex: not found"), "{out}");
    assert!(
        out.contains("dream agents update"),
        "pi is not installed: {out}"
    );

    e.settings("agents:\n  pi:\n    modle: x\n");
    let out = e.run(&["config", "check"]);
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("agents.pi") && text.contains("modle"),
        "{text}"
    );
}

/// `dream agents pi` with no login shows the login step and does not start
/// pi; with pi's own login reported, it starts pi, sending it nothing.
#[test]
fn dream_agents_pi_shows_the_login_step_before_starting_pi() {
    let e = Env::new("login");
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("../providers/testdata/fake-pi");
    let agent = e.root.join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::write(agent.join("fake-pi-logins"), "").expect("logins");
    e.settings(&format!(
        "agents:\n  pi:\n    command: [/bin/sh, {}]\n    agent_dir: {}\n",
        fake.display(),
        agent.display()
    ));
    let log = || std::fs::read_to_string(agent.join("fake-pi.log")).unwrap_or_default();

    let out = e.run(&["agents", "pi"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "pi started without a login: {err}");
    assert!(err.contains("auth login openai-codex"), "{err}");
    assert!(
        log().contains("auth:openai-codex:not_ready:offline=1"),
        "{}",
        log()
    );
    assert!(!log().contains("args:"), "pi was started:\n{}", log());

    std::fs::write(agent.join("fake-pi-logins"), "openai-codex\n").expect("logins");
    let out = e.run(&["agents", "pi"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(log().contains("args:"), "pi was not started:\n{}", log());
    assert!(log().contains("key:none"), "{}", log());
    assert!(!log().contains("turn:"), "a turn was started:\n{}", log());
}
