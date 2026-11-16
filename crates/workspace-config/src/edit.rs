//! Change one setting in `dreamer.yaml` the way a person would: only that
//! line changes, and comments, order, and every other setting stay as they
//! were. The result is read back before it is written, so a change never
//! leaves a file the settings check refuses.

use std::path::Path;

use crate::settings::{Settings, parse_settings};

/// `text` with `section.key` set to `value` (a string, quoted as YAML
/// needs), in block style: the key's line is replaced, or added at the top
/// of the section, or the section is added at the end. A section written
/// inline (`terminal: {theme: x}`) is not rewritten; that is an error.
pub fn set_string(text: &str, section: &str, key: &str, value: &str) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    fn head<'a>(l: &'a str, section: &str) -> Option<&'a str> {
        Some(l.strip_prefix(section)?.strip_prefix(':')?.trim())
    }
    let quoted = quote(value);
    let Some(at) = lines.iter().position(|l| head(l, section).is_some()) else {
        let mut out = text.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{section}:\n  {key}: {quoted}\n"));
        return Ok(out);
    };
    let after = head(lines[at], section).unwrap_or_default();
    if !(after.is_empty() || after.starts_with('#')) {
        return Err(format!(
            "`{section}` is written on one line; set {section}.{key} by hand"
        ));
    }
    // The section's lines: indented ones (and blanks or comments among
    // them) up to the next line at the left margin.
    let end = lines[at + 1..]
        .iter()
        .position(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with('#') && l.len() == t.len()
        })
        .map_or(lines.len(), |n| at + 1 + n);
    let indent = lines[at + 1..end]
        .iter()
        .find(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with('#')
        })
        .map_or(2, |l| l.len() - l.trim_start().len());
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    let line = |comment: &str| format!("{}{key}: {quoted}{comment}", " ".repeat(indent));
    let own = (at + 1..end).find(|&i| {
        let l = lines[i];
        l.len() - l.trim_start().len() == indent
            && l.trim_start()
                .strip_prefix(key)
                .is_some_and(|r| r.starts_with(':'))
    });
    match own {
        Some(i) => {
            // Keep a comment at the end of the line.
            let comment = lines[i]
                .find(" #")
                .filter(|_| !lines[i].contains(['"', '\'']))
                .map_or("", |c| {
                    let before = lines[i][..c].trim_end().len();
                    &lines[i][before..]
                });
            out[i] = line(comment);
        }
        None => out.insert(at + 1, line("")),
    }
    let mut text = out.join("\n");
    text.push('\n');
    Ok(text)
}

/// A YAML double-quoted string.
fn quote(value: &str) -> String {
    let mut q = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => q.push_str("\\\""),
            '\\' => q.push_str("\\\\"),
            c if c.is_control() => q.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

/// Set `terminal.theme` in the settings file at `path` (made if missing).
/// The new text must read as settings with that theme in force, else the
/// file is left alone and the error says why. Written whole and renamed
/// into place, so a watcher never sees half a file.
pub fn set_theme(path: &Path, name: &str) -> Result<Settings, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let new = set_string(&text, "terminal", "theme", name)?;
    let settings = parse_settings(&new, Some(path)).map_err(|e| e.to_string())?;
    let want = terminal_core::schemes::normalize(name);
    if settings.terminal.theme.map(|s| s.name) != Some(want.as_str()) {
        return Err(format!("{name}: the theme did not take"));
    }
    write(path, &new)?;
    Ok(settings)
}

/// Put `text` back at `path` exactly, or remove the file when `None` (it
/// did not exist): undoing a preview.
pub fn restore(path: &Path, text: Option<&str>) -> Result<(), String> {
    match text {
        Some(t) => write(path, t),
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        },
    }
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, text).map_err(fail)?;
    std::fs::rename(&tmp, path).map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_themes_line_changes_and_comments_stay() {
        let before = "\
# My settings
appearance: dark
terminal:
  # the one I like
  theme: nord   # for now
  cursor: bar
font:
  size: 14
";
        let after = set_string(before, "terminal", "theme", "Rosé Pine Dawn").expect("ok");
        assert_eq!(
            after,
            before.replace(
                "theme: nord   # for now",
                "theme: \"Rosé Pine Dawn\"   # for now"
            )
        );
    }

    #[test]
    fn a_missing_key_or_section_is_added_and_an_inline_one_refused() {
        let added =
            set_string("terminal:\n    cursor: bar\n", "terminal", "theme", "nord").expect("ok");
        assert_eq!(added, "terminal:\n    theme: \"nord\"\n    cursor: bar\n");
        let added = set_string("appearance: light", "terminal", "theme", "nord").expect("ok");
        assert_eq!(added, "appearance: light\nterminal:\n  theme: \"nord\"\n");
        assert_eq!(
            set_string("", "terminal", "theme", "nord").expect("ok"),
            "terminal:\n  theme: \"nord\"\n"
        );
        assert!(set_string("terminal: {theme: nord}\n", "terminal", "theme", "x").is_err());
        // A key of the same name deeper down is not this one.
        let nested = "terminal:\n  links:\n    theme: x\n";
        assert_eq!(
            set_string(nested, "terminal", "theme", "nord").expect("ok"),
            "terminal:\n  theme: \"nord\"\n  links:\n    theme: x\n"
        );
    }

    #[test]
    fn setting_a_theme_writes_only_a_file_that_reads_and_restore_puts_it_back() {
        let dir = std::env::temp_dir().join(format!("dreamer-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("dreamer.yaml");
        let s = set_theme(&path, "nord").expect("ok");
        assert_eq!(s.terminal.theme.map(|t| t.name), Some("nord"));
        assert!(set_theme(&path, "no-such-theme").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("ok"),
            "terminal:\n  theme: \"nord\"\n",
            "a refused theme leaves the file alone"
        );
        restore(&path, None).expect("ok");
        assert!(!path.exists());
        std::fs::write(&path, "font:\n  size: 15 # big\n").expect("ok");
        set_theme(&path, "nord").expect("ok");
        restore(&path, Some("font:\n  size: 15 # big\n")).expect("ok");
        assert_eq!(
            std::fs::read_to_string(&path).expect("ok"),
            "font:\n  size: 15 # big\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
