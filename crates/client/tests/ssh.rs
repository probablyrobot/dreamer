//! Both escaping layers end to end: the argv from `client::ssh::command`
//! is run as a process (layer 1) by a stand-in `ssh` that does what sshd
//! does with the remote command, hand it to `/bin/sh -c` (layer 2). The
//! remote `dream` must receive exactly the original words.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use client::ssh::{Mode, Remote, command};

/// The tests here write scripts and run them. On Linux, a child another
/// test forks at that moment briefly holds the script's write handle, and
/// running it then fails with ETXTBSY ("text file busy"). One at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn nul_words(path: &Path) -> Vec<String> {
    let bytes = std::fs::read(path).expect("recorded words");
    bytes
        .split(|b| *b == 0)
        .filter(|w| !w.is_empty())
        .map(|w| String::from_utf8(w.to_vec()).expect("utf-8"))
        .collect()
}

#[test]
fn hostile_words_reach_the_remote_dream_unchanged_through_both_layers() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = PathBuf::from(format!("/tmp/drm-ssh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    // The remote executable lives in a directory whose name needs quoting.
    let bin = root.join("bin dir 'q' $HOME");
    std::fs::create_dir_all(&bin).expect("dirs");
    let local_argv = root.join("ssh-argv");
    let remote_argv = root.join("remote-argv");
    let pwned = root.join("pwned");

    let ssh = root.join("ssh");
    script(
        &ssh,
        &format!(
            r#"#!/bin/sh
# Record the local argv, then behave like ssh -T: the word after the
# destination is a command string for the remote user's shell.
printf '%s\0' "$@" > '{argv}'
[ "$1" = -T ] || exit 90
while [ "$1" != -- ]; do shift; done
shift 2
exec /bin/sh -c "$1"
"#,
            argv = local_argv.display()
        ),
    );
    let dream = bin.join("dream");
    script(
        &dream,
        &format!(
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{}'\n",
            remote_argv.display()
        ),
    );

    let hostile = format!(
        "a b'c\"$(touch {p}); `touch {p}` \\ * ~ ; &",
        p = pwned.display()
    );
    let remote = Remote {
        destination: "devbox".into(),
        dream: Some(dream.display().to_string()),
        tmux: Some("/opt/tmux with space/tmux".into()),
        tmux_socket: Some(hostile.clone()),
        tmux_socket_path: None,
    };
    let argv = command(&ssh.display().to_string(), &remote, Mode::Batch).expect("argv");
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .expect("run");
    assert!(status.success(), "{status:?}");

    assert_eq!(nul_words(&local_argv), argv[1..], "layer 1: the local argv");
    assert_eq!(
        nul_words(&remote_argv),
        [
            "bridge",
            "--stdio",
            "--tmux",
            "/opt/tmux with space/tmux",
            "--tmux-socket",
            hostile.as_str()
        ],
        "layer 2: the remote words"
    );
    assert!(!pwned.exists(), "nothing in a word was executed");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn with_no_dream_named_the_installed_server_is_preferred_then_path() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = PathBuf::from(format!("/tmp/drm-ssh-default-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let installed = home.join(".local/share/dreamer/current/bin");
    let on_path = root.join("path");
    std::fs::create_dir_all(&installed).expect("dirs");
    std::fs::create_dir_all(&on_path).expect("dirs");
    let record = root.join("which");
    let ssh = root.join("ssh");
    script(
        &ssh,
        &format!(
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nexport HOME='{}' PATH='{}':/usr/bin:/bin\nexec /bin/sh -c \"$3\"\n",
            home.display(),
            on_path.display()
        ),
    );
    for (dir, label) in [(&installed, "installed"), (&on_path, "path")] {
        script(
            &dir.join("dream"),
            &format!(
                "#!/bin/sh\nprintf '{label} %s\\n' \"$*\" > '{}'\n",
                record.display()
            ),
        );
    }
    let remote = Remote {
        destination: "devbox".into(),
        tmux_socket: Some("work space".into()),
        ..Remote::default()
    };
    let run = || {
        let argv = command(&ssh.display().to_string(), &remote, Mode::Batch).expect("argv");
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .expect("run");
        assert!(status.success());
        std::fs::read_to_string(&record).expect("recorded")
    };
    assert_eq!(run(), "installed bridge --stdio --tmux-socket work space\n");
    std::fs::remove_file(installed.join("dream")).expect("rm");
    assert_eq!(run(), "path bridge --stdio --tmux-socket work space\n");
    let _ = std::fs::remove_dir_all(&root);
}
