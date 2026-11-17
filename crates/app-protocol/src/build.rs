//! Where a script app's JavaScript comes from. An `app.ts` has its types
//! stripped by `dream app check` and `dream app install`, which write the
//! JavaScript to `build/<name>.js` beside the manifest; the app host runs
//! that. Its last line records which source it was built from, so the
//! host refuses JavaScript that no longer matches the TypeScript beside
//! it. Stripping keeps every line and column where it was, so positions
//! in the JavaScript are positions in the TypeScript.
//!
//! A `.js` source runs as it is.

/// The directory, beside the manifest, that built JavaScript goes in.
pub const DIR: &str = "build";

/// What a source file is, by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    TypeScript,
    JavaScript,
}

/// The kind of `source`, or `None` for anything but `.ts` and `.js`.
pub fn kind(source: &str) -> Option<Kind> {
    let name = source.rsplit('/').next().unwrap_or(source);
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    match ext {
        "ts" => Some(Kind::TypeScript),
        "js" => Some(Kind::JavaScript),
        _ => None,
    }
}

/// The JavaScript the host runs for `source`, relative to the package:
/// `build/<stem>.js` for TypeScript, the source itself for JavaScript.
pub fn output(source: &str) -> String {
    match kind(source) {
        Some(Kind::TypeScript) => {
            let name = source.rsplit('/').next().unwrap_or(source);
            let stem = name.strip_suffix(".ts").unwrap_or(name);
            format!("{DIR}/{stem}.js")
        }
        _ => source.to_owned(),
    }
}

const MARK: &str = "//# dreamer-built-from ";

/// A fingerprint of the source text (FNV-1a, 64 bits, and the length).
fn fingerprint(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}-{}", text.len())
}

/// `js`, built from `source_text`, with its last line saying so.
pub fn stamp(js: &str, source_text: &str) -> String {
    let mut out = js.to_owned();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(MARK);
    out.push_str(&fingerprint(source_text));
    out.push('\n');
    out
}

/// Whether `js` was built from `source_text`.
pub fn built_from(js: &str, source_text: &str) -> bool {
    js.trim_end()
        .rsplit('\n')
        .next()
        .and_then(|l| l.strip_prefix(MARK))
        .is_some_and(|f| f == fingerprint(source_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typescript_runs_from_its_stamped_build() {
        assert_eq!(kind("app.ts"), Some(Kind::TypeScript));
        assert_eq!(kind("src/app.js"), Some(Kind::JavaScript));
        assert_eq!(kind("app.tsx"), None);
        assert_eq!(kind(".ts"), None);
        assert_eq!(output("src/app.ts"), "build/app.js");
        assert_eq!(output("app.js"), "app.js");
        let js = stamp("let a = 1;", "let a: number = 1;");
        assert!(built_from(&js, "let a: number = 1;"));
        assert!(!built_from(&js, "let a: number = 2;"));
        assert!(!built_from("let a = 1;", "let a: number = 1;"));
    }
}
