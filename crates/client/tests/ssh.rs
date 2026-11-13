//! Both escaping layers end to end: the argv from `client::ssh::command`
//! is run as a process (layer 1) by a stand-in `ssh` that does what sshd
//! does with the remote command, hand it to `/bin/sh -c` (layer 2). The
//! remote `dream` must receive exactly the original words.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use client::ssh::{Mode, Remote, command};

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
