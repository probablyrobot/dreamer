//! Downloads in web panes: the folder they are saved to, and the file
//! each one becomes there.
//!
//! - **The folder.** `web.downloads.folder` when it is set (`~` is the
//!   home directory). Otherwise `~/Downloads` on macOS; on Linux the XDG
//!   download directory (`XDG_DOWNLOAD_DIR` in `user-dirs.dirs`, in
//!   `$XDG_CONFIG_HOME` or `~/.config`), else `~/Downloads`
//!   ([`folder`]).
//! - **The file.** The name the page suggests, made safe for a file name
//!   ([`file_name`]), and unique in the folder: `report.bin`, then
//!   `report (1).bin` ([`unique`]). The file is written as `NAME.part`,
//!   reserved before the download starts ([`reserve`]), and renamed to
//!   its name only once complete ([`finish`]), so a partial file never
//!   has the name of a finished one.
//!
//! Everything that touches the disk here reads or writes files: never on
//! a UI thread.

use std::path::{Path, PathBuf};

/// The suffix a file has while it downloads.
pub const PART: &str = ".part";

/// The longest file name given, in bytes (file systems allow 255).
const NAME_MAX: usize = 200;

/// Where downloads are saved: `configured` (`web.downloads.folder`) if
/// set, else the platform's download folder. `None` without a home
/// directory to find it in. On Linux this reads `user-dirs.dirs`.
pub fn folder(configured: Option<&str>) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute());
    let user_dirs = if configured.is_some() || cfg!(target_os = "macos") {
        None
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|h| h.join(".config")))
            .and_then(|c| std::fs::read_to_string(c.join("user-dirs.dirs")).ok())
    };
    resolve(configured, home.as_deref(), user_dirs.as_deref())
}

/// [`folder`], from the configured folder, the home directory, and the
/// text of `user-dirs.dirs` (Linux only; `None` on macOS).
fn resolve(
    configured: Option<&str>,
    home: Option<&Path>,
    user_dirs: Option<&str>,
) -> Option<PathBuf> {
    if let Some(c) = configured {
        return expand_home(c, home);
    }
    let home = home?;
    Some(
        user_dirs
            .and_then(|text| xdg_download_dir(text, home))
            .unwrap_or_else(|| home.join("Downloads")),
    )
}

/// `~` and `~/…` under `home`; an absolute path as it is; anything else
/// (a relative path) is not a folder.
pub fn expand_home(path: &str, home: Option<&Path>) -> Option<PathBuf> {
    if path == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.map(|h| h.join(rest.trim_start_matches('/')));
    }
    let p = PathBuf::from(path);
    p.is_absolute().then_some(p)
}

/// `XDG_DOWNLOAD_DIR` from the text of `user-dirs.dirs`: a line
/// `XDG_DOWNLOAD_DIR="$HOME/Downloads"` (or an absolute path), quoted and
/// with shell escapes, as `xdg-user-dirs-update` writes it. The last valid
/// line wins; `None` if there is none.
fn xdg_download_dir(text: &str, home: &Path) -> Option<PathBuf> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "XDG_DOWNLOAD_DIR" {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        let mut plain = String::new();
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                plain.extend(chars.next());
            } else {
                plain.push(c);
            }
        }
        let dir = match plain.strip_prefix("$HOME") {
            Some("") => Some(home.to_path_buf()),
            Some(rest) if rest.starts_with('/') => Some(home.join(rest.trim_start_matches('/'))),
            Some(_) => None,
            None if plain.starts_with('/') => Some(PathBuf::from(&plain)),
            None => None,
        };
        if dir.is_some() {
            found = dir;
        }
    }
    found
}

/// A name a page suggested, as a file name: its last path component,
/// without control characters, path separators, or leading and trailing
/// dots and spaces (no hidden files), at most 200 bytes with its
/// extension kept; `download` when nothing is left.
pub fn file_name(suggested: &str) -> String {
    let last = suggested.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = last
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == ':' { '_' } else { c })
        .collect();
    let clean = clean.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if clean.is_empty() {
        return "download".into();
    }
    if clean.len() <= NAME_MAX {
        return clean.to_owned();
    }
    let (stem, ext) = match split(clean) {
        (stem, ext) if ext.len() <= 20 => (stem, ext),
        _ => (clean, ""),
    };
    let mut cut = NAME_MAX - ext.len();
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{ext}", &stem[..cut])
}

/// A file name's stem and its extension (with its dot): `report` and
/// `.bin`, `archive` and `.tar.gz`; no extension for `README`.
fn split(name: &str) -> (&str, &str) {
    let Some(dot) = name.rfind('.').filter(|i| *i > 0) else {
        return (name, "");
    };
    let (stem, ext) = name.split_at(dot);
    match stem.len().checked_sub(4) {
        Some(i) if stem[i..].eq_ignore_ascii_case(".tar") && i > 0 => name.split_at(i),
        _ => (stem, ext),
    }
}

/// The `n`th name for `name` in a folder: `name` itself, then
/// `stem (n).ext`.
fn numbered(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    let (stem, ext) = split(name);
    format!("{stem} ({n}){ext}")
}

/// The file being written for the file at `path`: `path` with
/// [`PART`] added.
pub fn part_of(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(PART);
    PathBuf::from(p)
}

/// The first of `name`, `stem (1).ext`, `stem (2).ext`, … in `dir` that
/// `taken` says is free (neither the file nor its `.part` is there).
pub fn unique(dir: &Path, name: &str, taken: impl Fn(&Path) -> bool) -> PathBuf {
    (0..)
        .map(|n| dir.join(numbered(name, n)))
        .find(|p| !taken(p) && !taken(&part_of(p)))
        .unwrap_or_else(|| dir.join(name))
}

/// Make `dir` if it is not there, and reserve the file a download of
/// `name` becomes in it: a unique name, and its `.part` file, created
/// empty here so no other download takes it. The file's path and its
/// `.part`'s.
pub fn reserve(dir: &Path, name: &str) -> std::io::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    for _ in 0..100 {
        let file = unique(dir, name, |p| p.symlink_metadata().is_ok());
        let part = part_of(&file);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
        {
            Ok(_) => return Ok((file, part)),
            // Taken since it was looked at: the next name.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "no free name for {name} in {}",
        dir.display()
    )))
}

/// A finished download at `part`: give it the name `wanted` (or, if a
/// file has taken that since, the next free one), never replacing a
/// file. The path it has now.
pub fn finish(part: &Path, wanted: &Path) -> std::io::Result<PathBuf> {
    let dir = wanted.parent().unwrap_or(Path::new("."));
    let name = wanted
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    for _ in 0..100 {
        let file = unique(dir, &name, |p| p != part && p.symlink_metadata().is_ok());
        // A hard link fails when the name is taken: nothing is replaced.
        match std::fs::hard_link(part, &file) {
            Ok(()) => {
                std::fs::remove_file(part)?;
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // A file system without hard links.
            Err(_) if file.symlink_metadata().is_err() => {
                std::fs::rename(part, &file)?;
                return Ok(file);
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "no free name for {name} in {}",
        dir.display()
    )))
}

/// Remove what a download that ended unfinished left in `dir`: its
/// reserved `.part` file, and Chromium's own partial file (`at`, if it is
/// in `dir` and named after the `.part`). Nothing else is touched.
pub fn discard(part: &Path, at: Option<&Path>) {
    let _ = std::fs::remove_file(part);
    let (Some(at), Some(dir), Some(name)) = (at, part.parent(), part.file_name()) else {
        return;
    };
    let ours = at.parent() == Some(dir)
        && at
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(&*name.to_string_lossy()));
    if ours {
        let _ = std::fs::remove_file(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_is_the_setting_else_the_xdg_one_else_downloads_in_home() {
        let home = Some(Path::new("/home/ada"));
        // The setting wins, with `~` expanded.
        assert_eq!(
            resolve(Some("~/Inbox"), home, Some("XDG_DOWNLOAD_DIR=\"/x\"")),
            Some(PathBuf::from("/home/ada/Inbox"))
        );
        assert_eq!(
            resolve(Some("~"), home, None),
            Some(PathBuf::from("/home/ada"))
        );
        assert_eq!(
            resolve(Some("/srv/dl"), home, None),
            Some(PathBuf::from("/srv/dl"))
        );
        assert_eq!(resolve(Some("relative/dir"), home, None), None);
        // XDG's, as xdg-user-dirs-update writes it.
        let dirs = "# This file is written by xdg-user-dirs-update\n\
                    XDG_DESKTOP_DIR=\"$HOME/Desktop\"\n\
                    XDG_DOWNLOAD_DIR=\"$HOME/Hent\\\"er\"\n";
        assert_eq!(
            resolve(None, home, Some(dirs)),
            Some(PathBuf::from("/home/ada/Hent\"er"))
        );
        assert_eq!(
            resolve(None, home, Some("XDG_DOWNLOAD_DIR=\"/data/dl\"\n")),
            Some(PathBuf::from("/data/dl"))
        );
        assert_eq!(
            resolve(None, home, Some("XDG_DOWNLOAD_DIR=\"$HOME\"\n")),
            Some(PathBuf::from("/home/ada"))
        );
        // Missing, commented out, or not a path: ~/Downloads.
        for text in [
            "",
            "XDG_MUSIC_DIR=\"$HOME/Music\"\n",
            "# XDG_DOWNLOAD_DIR=\"$HOME/x\"\n",
            "XDG_DOWNLOAD_DIR=\"relative\"\n",
            "XDG_DOWNLOAD_DIR=\"$HOMEX/y\"\n",
        ] {
            assert_eq!(
                resolve(None, home, Some(text)),
                Some(PathBuf::from("/home/ada/Downloads")),
                "{text:?}"
            );
        }
        assert_eq!(
            resolve(None, home, None),
            Some(PathBuf::from("/home/ada/Downloads"))
        );
        assert_eq!(resolve(None, None, None), None);
    }

    #[test]
    fn a_suggested_name_becomes_a_safe_file_name() {
        assert_eq!(file_name("report.pdf"), "report.pdf");
        assert_eq!(file_name("../../etc/passwd"), "passwd");
        assert_eq!(file_name("C:\\x\\y.txt"), "y.txt");
        assert_eq!(file_name(".bashrc"), "bashrc");
        assert_eq!(file_name("a\u{1b}[2Jb.txt"), "a[2Jb.txt");
        assert_eq!(file_name("time 10:30.txt"), "time 10_30.txt");
        assert_eq!(file_name("  ..  "), "download");
        assert_eq!(file_name(""), "download");
        let long = format!("{}.zip", "é".repeat(300));
        let cut = file_name(&long);
        assert!(cut.len() <= NAME_MAX && cut.ends_with(".zip"), "{cut}");
    }

    #[test]
    fn a_name_taken_in_the_folder_gets_the_next_number() {
        let dir = Path::new("/d");
        let taken = |names: &'static [&'static str]| {
            move |p: &Path| names.iter().any(|n| p == Path::new("/d").join(n))
        };
        assert_eq!(
            unique(dir, "report.bin", taken(&[])),
            dir.join("report.bin")
        );
        assert_eq!(
            unique(dir, "report.bin", taken(&["report.bin"])),
            dir.join("report (1).bin")
        );
        // A download in progress holds its name too.
        assert_eq!(
            unique(
                dir,
                "report.bin",
                taken(&["report.bin", "report (1).bin.part"])
            ),
            dir.join("report (2).bin")
        );
        assert_eq!(
            unique(dir, "archive.tar.gz", taken(&["archive.tar.gz"])),
            dir.join("archive (1).tar.gz")
        );
        assert_eq!(
            unique(dir, "README", taken(&["README"])),
            dir.join("README (1)")
        );
    }

    #[test]
    fn a_download_is_written_as_part_and_renamed_without_replacing_a_file() {
        let dir = std::env::temp_dir().join(format!("dreamer-downloads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let folder = dir.join("made");
        let (file, part) = reserve(&folder, "report.bin").expect("reserve");
        assert_eq!(
            (file.clone(), part.clone()),
            (folder.join("report.bin"), folder.join("report.bin.part"))
        );
        // A second download of the same name while the first runs.
        let (second, second_part) = reserve(&folder, "report.bin").expect("reserve");
        assert_eq!(second, folder.join("report (1).bin"));
        std::fs::write(&part, b"one").expect("write");
        // Something took the name meanwhile: the next free one, and the
        // other file is kept.
        std::fs::write(&file, b"theirs").expect("write");
        let done = finish(&part, &file).expect("finish");
        assert_eq!(done, folder.join("report (2).bin"));
        assert_eq!(std::fs::read(&done).expect("read"), b"one");
        assert_eq!(std::fs::read(&file).expect("read"), b"theirs");
        assert!(!part.exists());
        // An unfinished one leaves nothing: its `.part`, and Chromium's
        // partial file beside it, go; another file does not.
        let partial = folder.join("report (1).bin.part.crdownload");
        std::fs::write(&partial, b"x").expect("write");
        discard(&second_part, Some(&partial));
        discard(&second_part, Some(&file));
        assert!(!second_part.exists() && !partial.exists() && file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
