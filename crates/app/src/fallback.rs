//! The fallback for what independent panes cannot show:
//! floating-pane layouts and tmux-drawn modes. The user gets a safely
//! quoted `tmux attach` command for the same server and window, to copy,
//! and optionally a launch of their configured terminal with it. Either way
//! is an ordinary tmux client, clearly apart from Dreamer's own rendering.

/// An attach command: exact argv to run, and the same quoted for a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attach {
    pub argv: Vec<String>,
    pub display: String,
}

/// Quote one word for a POSIX shell: as is when it is plainly safe,
/// otherwise in single quotes with any `'` closed, escaped, and reopened.
pub(crate) fn quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if safe {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// `tmux [-L name | -S path] attach-session -t $SESSION[:@WINDOW]` for the
/// tmux the daemon was started with (its `--tmux`, `--tmux-socket`, and
/// `--tmux-socket-path` options), or with none the server `tmux.server`
/// chooses (Dreamer's own, `-L dreamer`, by default). Ids, never names:
/// names can contain anything, and ids are exact for this server.
pub(crate) fn attach_command(
    daemon_args: &[String],
    server: &workspace_config::TmuxServerChoice,
    session: u32,
    window: Option<u32>,
) -> Attach {
    use workspace_config::TmuxServerChoice;
    let opt = |name: &str| {
        daemon_args
            .iter()
            .position(|a| a == name)
            .and_then(|i| daemon_args.get(i + 1))
            .cloned()
    };
    let mut argv = vec![opt("--tmux").unwrap_or_else(|| "tmux".into())];
    if let Some(name) = opt("--tmux-socket") {
        if name != "default" {
            argv.extend(["-L".into(), name]);
        }
    } else if let Some(path) = opt("--tmux-socket-path") {
        argv.extend(["-S".into(), path]);
    } else {
        match server {
            TmuxServerChoice::Own => argv.extend(["-L".into(), "dreamer".into()]),
            TmuxServerChoice::Default => {}
            TmuxServerChoice::Name(n) => argv.extend(["-L".into(), n.clone()]),
            TmuxServerChoice::Path(p) => {
                argv.extend(["-S".into(), p.display().to_string()]);
            }
        }
    }
    let target = match window {
        Some(w) => format!("${session}:@{w}"),
        None => format!("${session}"),
    };
    argv.extend(["attach-session".into(), "-t".into(), target]);
    let display = argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    Attach { argv, display }
}

/// The user's terminal launch template with `{command}` replaced by the
/// attach argv, element for element: never a shell string.
pub(crate) fn launch_argv(template: &[String], attach: &Attach) -> Option<Vec<String>> {
    if !template.iter().any(|a| a == "{command}") {
        return None;
    }
    Some(
        template
            .iter()
            .flat_map(|a| {
                if a == "{command}" {
                    attach.argv.clone()
                } else {
                    vec![a.clone()]
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_config::TmuxServerChoice;

    #[test]
    fn words_are_quoted_only_when_needed_and_quotes_survive() {
        assert_eq!(quote("tmux"), "tmux");
        assert_eq!(quote("/opt/homebrew/bin/tmux"), "/opt/homebrew/bin/tmux");
        assert_eq!(quote("$1:@3"), "'$1:@3'");
        assert_eq!(quote("my socket"), "'my socket'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a;rm -rf ~"), "'a;rm -rf ~'");
    }

    #[test]
    fn the_attach_command_names_the_same_server_and_window_by_id() {
        let own = TmuxServerChoice::Own;
        let a = attach_command(&[], &own, 0, Some(3));
        assert_eq!(
            a.argv,
            ["tmux", "-L", "dreamer", "attach-session", "-t", "$0:@3"]
        );
        assert_eq!(a.display, "tmux -L dreamer attach-session -t '$0:@3'");
        let a = attach_command(&[], &TmuxServerChoice::Default, 0, Some(3));
        assert_eq!(a.argv, ["tmux", "attach-session", "-t", "$0:@3"]);
        let default: Vec<String> = ["--tmux-socket", "default"].map(String::from).to_vec();
        assert_eq!(
            attach_command(&default, &own, 0, None).argv,
            ["tmux", "attach-session", "-t", "$0"]
        );
        let args: Vec<String> = ["--tmux-socket", "dev box", "--tmux", "/usr/bin/tmux"]
            .map(String::from)
            .to_vec();
        let a = attach_command(&args, &own, 2, None);
        assert_eq!(
            a.display,
            "/usr/bin/tmux -L 'dev box' attach-session -t '$2'"
        );
        let args: Vec<String> = ["--tmux-socket-path", "/tmp/t s"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            attach_command(&args, &own, 1, Some(4)).argv,
            ["tmux", "-S", "/tmp/t s", "attach-session", "-t", "$1:@4"]
        );
    }

    #[test]
    fn a_launch_template_splices_the_argv_and_needs_the_placeholder() {
        let a = attach_command(&[], &TmuxServerChoice::Default, 0, Some(1));
        let t: Vec<String> = ["kitty", "-e", "{command}"].map(String::from).to_vec();
        assert_eq!(
            launch_argv(&t, &a).expect("launch"),
            ["kitty", "-e", "tmux", "attach-session", "-t", "$0:@1"]
        );
        assert_eq!(launch_argv(&["kitty".into()], &a), None);
    }
}
