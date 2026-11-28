//! An app package: an app's folder as one file, `<id>-<version>.dreamerapp`,
//! whose SHA-256 is the version's identity.
//!
//! The file is a POSIX ustar archive written so that the same files always
//! give the same bytes, on every platform (see "Packages" in
//! `docs/termapp.md`, which a tool of its own can follow to the byte):
//!
//! - one entry per regular file, in byte order of their paths; no entries
//!   for directories, links or devices, and no pax or GNU extensions;
//! - each header: the path (in `name`, or split at a `/` into `prefix` and
//!   `name`), mode `0644`, or `0755` when the file is executable, uid and
//!   gid 0, no user or group names, modification time 0, type `0`, magic
//!   `ustar\0` and version `00`; numbers in zero-padded octal ending in a
//!   NUL, the checksum as six octal digits, a NUL and a space;
//! - the data, padded with zeros to 512 bytes, and the archive ends with
//!   two zero blocks and nothing after them.
//!
//! [`unpack`] reads only that form: it regenerates each header from the
//! path, size and mode it states and requires the same bytes, so any
//! archive it accepts is the one [`pack`] writes for those files.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The package file's extension.
pub const EXTENSION: &str = "dreamerapp";
/// The manifest every package has at its root.
pub const MANIFEST: &str = "app.toml";
/// Patterns of files to leave out, at the package's root.
pub const IGNORE_FILE: &str = ".dreamerignore";
/// The largest file a package holds.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// The largest package.
pub const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
/// The most files a package holds.
pub const MAX_FILES: usize = 4096;
/// The longest path in a package, in bytes.
pub const MAX_PATH_BYTES: usize = 255;

const BLOCK: usize = 512;

/// `<id>-<version>.dreamerapp`.
pub fn file_name(id: &str, version: &str) -> String {
    format!("{id}-{version}.{EXTENSION}")
}

/// Lowercase hexadecimal SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A package, in memory.
#[derive(Debug, Clone)]
pub struct Packed {
    pub bytes: Vec<u8>,
    pub sha256: String,
    /// The paths in it, in order.
    pub files: Vec<String>,
}

/// What [`unpack`] wrote.
#[derive(Debug, Clone)]
pub struct Unpacked {
    pub sha256: String,
    pub files: Vec<String>,
    /// The text of `app.toml`.
    pub manifest: String,
}

/// One file of a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub executable: bool,
    pub data: Vec<u8>,
}

/// Pack the app folder `dir`: every regular file except dot files and
/// folders, `node_modules/`, the build output `build/` at the root, and
/// what `.dreamerignore` names. A link or other special file is refused,
/// unless it is left out.
pub fn pack(dir: &Path) -> Result<Packed, String> {
    let ignore = match std::fs::read_to_string(dir.join(IGNORE_FILE)) {
        Ok(text) => Ignore::parse(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ignore::default(),
        Err(e) => return Err(format!("{}: {e}", dir.join(IGNORE_FILE).display())),
    };
    let mut found = Vec::new();
    collect(dir, "", &ignore, &mut found)?;
    found.sort_by(|a, b| a.0.cmp(&b.0));
    if !found.iter().any(|(p, _)| p == MANIFEST) {
        return Err(format!("{}: no {MANIFEST}", dir.display()));
    }
    let mut entries = Vec::with_capacity(found.len());
    for (path, file) in found {
        let meta = std::fs::metadata(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        if meta.len() > MAX_FILE_BYTES {
            return Err(format!(
                "{path}: {} bytes; a package file holds at most {MAX_FILE_BYTES}",
                meta.len()
            ));
        }
        let data = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        entries.push(Entry {
            path,
            executable: executable(&meta),
            data,
        });
    }
    let bytes = write(&entries)?;
    Ok(Packed {
        sha256: sha256_hex(&bytes),
        files: entries.into_iter().map(|e| e.path).collect(),
        bytes,
    })
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

/// Walk `dir` (at `rel` inside the package), adding `(path, file)` for
/// each file packed.
fn collect(
    dir: &Path,
    rel: &str,
    ignore: &Ignore,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    for entry in std::fs::read_dir(dir).map_err(|e| fail(dir, e))? {
        let entry = entry.map_err(|e| fail(dir, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(format!(
                "{}: a package's paths are UTF-8",
                entry.path().display()
            ));
        };
        let path = if rel.is_empty() {
            name.to_owned()
        } else {
            format!("{rel}/{name}")
        };
        let kind = entry.file_type().map_err(|e| fail(&entry.path(), e))?;
        let is_dir = kind.is_dir();
        if name.starts_with('.')
            || (is_dir && name == "node_modules")
            || (is_dir && path == "build")
            || ignore.matches(&path, is_dir)
        {
            continue;
        }
        check_path(&path)?;
        if is_dir {
            collect(&entry.path(), &path, ignore, out)?;
        } else if kind.is_file() {
            out.push((path, entry.path()));
            if out.len() > MAX_FILES {
                return Err(format!("a package holds at most {MAX_FILES} files"));
            }
        } else if kind.is_symlink() {
            return Err(format!(
                "{path}: a symbolic link; a package holds only files (copy it in, or list it in {IGNORE_FILE})"
            ));
        } else {
            return Err(format!("{path}: not a file or folder"));
        }
    }
    Ok(())
}

/// A path a package may hold: relative, `/`-separated, each part a name
/// (not empty, `.` or `..`), no `\`, no control characters, at most
/// [`MAX_PATH_BYTES`].
fn check_path(path: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("`{path}`: {why}"));
    if path.is_empty() {
        return bad("an empty path");
    }
    if path.starts_with('/') {
        return bad("an absolute path");
    }
    if path.len() > MAX_PATH_BYTES {
        return bad(&format!("longer than {MAX_PATH_BYTES} bytes"));
    }
    if path.contains('\\') || path.chars().any(char::is_control) {
        return bad("a `\\` or a control character");
    }
    for part in path.split('/') {
        match part {
            "" => return bad("an empty part"),
            "." | ".." => return bad("a `.` or `..` part"),
            _ => {}
        }
    }
    Ok(())
}

/// Write `entries` (already in order) as a package.
pub fn write(entries: &[Entry]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut last: Option<&str> = None;
    for e in entries {
        check_path(&e.path)?;
        if last.is_some_and(|l| l >= e.path.as_str()) {
            return Err(format!("`{}`: out of order or repeated", e.path));
        }
        last = Some(&e.path);
        if e.data.len() as u64 > MAX_FILE_BYTES {
            return Err(format!("{}: larger than {MAX_FILE_BYTES} bytes", e.path));
        }
        out.extend_from_slice(&header(&e.path, e.data.len() as u64, e.executable)?);
        out.extend_from_slice(&e.data);
        out.resize(out.len().next_multiple_of(BLOCK), 0);
        if out.len() as u64 > MAX_PACKAGE_BYTES {
            return Err(format!("a package holds at most {MAX_PACKAGE_BYTES} bytes"));
        }
    }
    out.resize(out.len() + 2 * BLOCK, 0);
    Ok(out)
}

/// `prefix` and `name` for `path`: all of it in `name` when it fits in
/// 100 bytes, otherwise split at the first `/` that leaves a `prefix` of at
/// most 155 bytes and a `name` of at most 100.
fn split(path: &str) -> Result<(&str, &str), String> {
    if path.len() <= 100 {
        return Ok(("", path));
    }
    path.match_indices('/')
        .map(|(i, _)| (&path[..i], &path[i + 1..]))
        .find(|(p, n)| p.len() <= 155 && n.len() <= 100 && !n.is_empty())
        .ok_or_else(|| format!("`{path}`: no `/` splits it into a ustar prefix and name"))
}

fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(&text.as_bytes()[text.len() - digits..]);
    field[digits] = 0;
}

fn header(path: &str, size: u64, executable: bool) -> Result<[u8; BLOCK], String> {
    let (prefix, name) = split(path)?;
    let mut h = [0u8; BLOCK];
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], if executable { 0o755 } else { 0o644 });
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 0);
    h[148..156].fill(b' ');
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
    octal(&mut h[148..155], sum);
    h[155] = b' ';
    Ok(h)
}

/// Read a NUL-terminated octal field.
fn read_octal(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field.split(|&b| b == 0).next()?).ok()?;
    u64::from_str_radix(text.trim(), 8).ok()
}

/// A NUL-padded text field.
fn read_text(field: &[u8]) -> &[u8] {
    field.split(|&b| b == 0).next().unwrap_or_default()
}

/// The files in package `bytes`, checked: only the form [`pack`] writes,
/// with safe paths, within the limits, and with `app.toml` at the root.
pub fn entries(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    if bytes.len() as u64 > MAX_PACKAGE_BYTES {
        return Err(format!("larger than {MAX_PACKAGE_BYTES} bytes"));
    }
    if !bytes.len().is_multiple_of(BLOCK) {
        return Err("not a package: its size is not whole 512-byte blocks".into());
    }
    let mut out: Vec<Entry> = Vec::new();
    let mut at = 0;
    loop {
        let Some(h) = bytes.get(at..at + BLOCK) else {
            return Err("not a package: it ends without its two zero blocks".into());
        };
        if h.iter().all(|&b| b == 0) {
            if bytes[at..].len() != 2 * BLOCK || bytes[at..].iter().any(|&b| b != 0) {
                return Err(
                    "not a package: anything but two zero blocks after the last file".into(),
                );
            }
            break;
        }
        let name = read_text(&h[..100]);
        let prefix = read_text(&h[345..500]);
        let path = if prefix.is_empty() {
            name.to_vec()
        } else {
            [prefix, b"/", name].concat()
        };
        let path = String::from_utf8(path)
            .map_err(|_| "not a package: a path that is not UTF-8".to_owned())?;
        match h[156] {
            b'0' => {}
            b'1' => return Err(format!("`{path}`: a hard link; a package holds only files")),
            b'2' => {
                return Err(format!(
                    "`{path}`: a symbolic link; a package holds only files"
                ));
            }
            b'3' | b'4' | b'6' => return Err(format!("`{path}`: a device or pipe")),
            b'5' => {
                return Err(format!(
                    "`{path}`: a directory entry; a package lists only files"
                ));
            }
            other => {
                return Err(format!(
                    "`{path}`: entry type `{}` (pax, GNU or other extensions are not read)",
                    char::from(other).escape_default()
                ));
            }
        }
        check_path(&path)?;
        let size = read_octal(&h[124..136]).ok_or_else(|| format!("`{path}`: bad size"))?;
        if size > MAX_FILE_BYTES {
            return Err(format!("`{path}`: larger than {MAX_FILE_BYTES} bytes"));
        }
        let mode = read_octal(&h[100..108]).ok_or_else(|| format!("`{path}`: bad mode"))?;
        let executable = match mode {
            0o644 => false,
            0o755 => true,
            m => return Err(format!("`{path}`: mode {m:o}; a package has 644 or 755")),
        };
        if header(&path, size, executable)?[..] != h[..] {
            return Err(format!(
                "`{path}`: its header is not the one Dreamer writes (owner, time, checksum or padding differ)"
            ));
        }
        if out.last().is_some_and(|l| l.path >= path) {
            return Err(format!("`{path}`: out of order or repeated"));
        }
        if out.len() == MAX_FILES {
            return Err(format!("more than {MAX_FILES} files"));
        }
        let start = at + BLOCK;
        let len = usize::try_from(size).map_err(|_| format!("`{path}`: too large"))?;
        let end = start + len;
        let padded = start + len.next_multiple_of(BLOCK);
        let Some(data) = bytes.get(start..end) else {
            return Err(format!("`{path}`: cut short"));
        };
        if bytes
            .get(end..padded)
            .is_none_or(|p| p.iter().any(|&b| b != 0))
        {
            return Err(format!("`{path}`: its padding is not zeros"));
        }
        // A path that is a file cannot also be a folder.
        if let Some(l) = out.last()
            && path.starts_with(&format!("{}/", l.path))
        {
            return Err(format!("`{path}`: inside `{}`, which is a file", l.path));
        }
        out.push(Entry {
            path,
            executable,
            data: data.to_vec(),
        });
        at = padded;
    }
    if !out.iter().any(|e| e.path == MANIFEST) {
        return Err(format!("no {MANIFEST} at the package's root"));
    }
    Ok(out)
}

/// Unpack package `bytes` into `into`, which must not exist yet. Nothing
/// is written unless the whole package checks.
pub fn unpack(bytes: &[u8], into: &Path) -> Result<Unpacked, String> {
    let files = entries(bytes)?;
    let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    if into.exists() {
        return Err(format!("{}: already exists", into.display()));
    }
    std::fs::create_dir_all(into).map_err(|e| fail(into, e))?;
    let mut manifest = String::new();
    for e in &files {
        let to = e
            .path
            .split('/')
            .fold(into.to_path_buf(), |p, part| p.join(part));
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|err| fail(parent, err))?;
        }
        std::fs::write(&to, &e.data).map_err(|err| fail(&to, err))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if e.executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode))
                .map_err(|err| fail(&to, err))?;
        }
        if e.path == MANIFEST {
            manifest =
                String::from_utf8(e.data.clone()).map_err(|_| format!("{MANIFEST}: not UTF-8"))?;
        }
    }
    Ok(Unpacked {
        sha256: sha256_hex(bytes),
        files: files.into_iter().map(|e| e.path).collect(),
        manifest,
    })
}

/// `.dreamerignore`: one pattern a line; blank lines and lines starting
/// with `#` are skipped. A pattern ending in `/` matches only folders. A
/// pattern with a `/` elsewhere matches the path from the package's root
/// (a leading `/` is dropped); one without matches a file or folder of
/// that name anywhere. `*` matches within one name, `?` one character of
/// it, and `**` any number of folders. `!` (re-including) is not read.
#[derive(Debug, Default)]
struct Ignore(Vec<Pattern>);

#[derive(Debug)]
struct Pattern {
    glob: String,
    anchored: bool,
    dir_only: bool,
}

impl Ignore {
    fn parse(text: &str) -> Result<Ignore, String> {
        let mut out = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('!') {
                return Err(format!(
                    "{IGNORE_FILE}:{}: `!` patterns are not supported",
                    n + 1
                ));
            }
            let (body, dir_only) = match line.strip_suffix('/') {
                Some(b) => (b, true),
                None => (line, false),
            };
            let anchored = body.contains('/');
            out.push(Pattern {
                glob: body.trim_start_matches('/').to_owned(),
                anchored,
                dir_only,
            });
        }
        Ok(Ignore(out))
    }

    fn matches(&self, path: &str, is_dir: bool) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        self.0.iter().any(|p| {
            (!p.dir_only || is_dir)
                && if p.anchored {
                    glob(p.glob.as_bytes(), path.as_bytes())
                } else {
                    glob(p.glob.as_bytes(), name.as_bytes())
                }
        })
    }
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    match p {
        [] => s.is_empty(),
        [b'*', b'*', rest @ ..] => {
            let rest = rest.strip_prefix(b"/").unwrap_or(rest);
            (0..=s.len()).any(|i| glob(rest, &s[i..]))
        }
        [b'*', rest @ ..] => {
            let stop = s.iter().position(|&c| c == b'/').unwrap_or(s.len());
            (0..=stop).any(|i| glob(rest, &s[i..]))
        }
        [b'?', rest @ ..] => s.first().is_some_and(|&c| c != b'/') && glob(rest, &s[1..]),
        [c, rest @ ..] => s.first() == Some(c) && glob(rest, &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, data: &str) -> Entry {
        Entry {
            path: path.into(),
            executable: false,
            data: data.as_bytes().to_vec(),
        }
    }

    #[test]
    fn what_is_written_reads_back_and_one_byte_changes_the_hash() {
        let files = vec![
            entry("app.toml", "id = \"local.x\"\n"),
            entry("app.ts", "export {}\n"),
            Entry {
                path: "bin/run".into(),
                executable: true,
                data: b"#!/bin/sh\n".to_vec(),
            },
        ];
        let bytes = write(&files).expect("write");
        assert_eq!(entries(&bytes).expect("read"), files);
        let mut other = files.clone();
        other[0].data[0] ^= 1;
        assert_ne!(
            sha256_hex(&bytes),
            sha256_hex(&write(&other).expect("write"))
        );
    }

    #[test]
    fn long_paths_split_into_prefix_and_name() {
        let deep = format!("{}/{}/app.ts", "a".repeat(90), "b".repeat(60));
        let files = vec![entry(&deep, "x"), entry("app.toml", "")];
        let bytes = write(&files).expect("write");
        assert_eq!(entries(&bytes).expect("read")[0].path, deep);
        let unsplittable = format!("{}/{}", "a".repeat(10), "b".repeat(120));
        assert!(write(&[entry(&unsplittable, "")]).is_err());
    }

    #[test]
    fn links_escapes_and_foreign_headers_are_refused() {
        let good = write(&[entry("app.toml", "")]).expect("write");
        let with = |at: usize, value: &[u8]| {
            let mut b = good.clone();
            b[at..at + value.len()].copy_from_slice(value);
            // Keep the checksum right, so only the change is judged.
            b[148..156].fill(b' ');
            let sum: u64 = b[..BLOCK].iter().map(|&c| u64::from(c)).sum();
            octal(&mut b[148..155], sum);
            b[155] = b' ';
            entries(&b).expect_err("refused")
        };
        assert!(with(156, b"2").contains("symbolic link"));
        assert!(with(156, b"1").contains("hard link"));
        assert!(with(0, b"/etc/passwd\0").contains("absolute"));
        assert!(with(0, b"../app.toml\0").contains("`..`"));
        assert!(with(265, b"igor").contains("not the one Dreamer writes"));
        assert!(with(136, b"14000000000").contains("not the one Dreamer writes"));
        for bad in ["/abs", "a/../b", "a//b", "./a", "a\\b"] {
            assert!(write(&[entry(bad, "")]).is_err(), "{bad}");
        }
        let mut trailing = good.clone();
        trailing.extend_from_slice(&[0; BLOCK]);
        assert!(entries(&trailing).is_err());
        assert!(entries(&write(&[entry("other", "")]).expect("w")).is_err());
    }

    #[test]
    fn ignore_patterns() {
        let i = Ignore::parse("# notes\n*.log\n/docs/draft.md\nout/\nassets/**/*.psd\n")
            .expect("parse");
        assert!(i.matches("a.log", false) && i.matches("x/y/a.log", false));
        assert!(i.matches("docs/draft.md", false) && !i.matches("x/docs/draft.md", false));
        assert!(i.matches("out", true) && !i.matches("out", false));
        assert!(i.matches("assets/a/b/c.psd", false) && i.matches("assets/c.psd", false));
        assert!(!i.matches("app.ts", false));
        assert!(Ignore::parse("!keep\n").is_err());
    }
}
