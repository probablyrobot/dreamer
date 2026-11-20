//! Building a script app: its `app.ts` with the types stripped
//! (`swc_ts_fast_strip`, strip-only), written to `build/<name>.js` beside
//! the manifest for the app host to run. Types become spaces, so every
//! line and column in the JavaScript is where it was in the TypeScript.
//! TypeScript that only compiles to something else (an `enum`, a
//! `namespace` with code, parameter properties) is refused with where it
//! is: strip-only keeps the code as written.

use std::path::Path;
use std::sync::{Arc, Mutex};

use swc_common::errors::{DiagnosticBuilder, Emitter, Handler};
use swc_common::sync::Lrc;
use swc_common::{GLOBALS, Globals, SourceMap};

/// The `[script] source` a manifest names, if it is a script app.
pub(crate) fn script_source(manifest: &Path) -> Result<Option<String>, String> {
    let text =
        std::fs::read_to_string(manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    // A manifest that does not parse is reported by the check itself.
    let Ok(table) = text.parse::<toml::Table>() else {
        return Ok(None);
    };
    Ok(table
        .get("script")
        .and_then(|s| s.get("source"))
        .and_then(|s| s.as_str())
        .map(str::to_owned))
}

/// Build a script app's JavaScript from its TypeScript, when the package
/// at `manifest` is one; nothing to do otherwise.
pub fn build(manifest: &Path) -> Result<(), String> {
    let Some(source) = script_source(manifest)? else {
        return Ok(());
    };
    if app_protocol::build::kind(&source) != Some(app_protocol::build::Kind::TypeScript)
        || source.starts_with('/')
        || source.split('/').any(|p| p == "..")
    {
        // A `.js` runs as written; a bad name is the check's to report.
        return Ok(());
    }
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let src = dir.join(&source);
    let ts = std::fs::read_to_string(&src).map_err(|e| format!("{}: {e}", src.display()))?;
    let js = strip(&source, &ts).map_err(|errors| errors.join("\n"))?;
    let out = dir.join(app_protocol::build::output(&source));
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let built = app_protocol::build::stamp(&js, &ts);
    // Unchanged, it is left alone: a live preview restarts the app when
    // the file changes.
    if std::fs::read_to_string(&out).is_ok_and(|old| old == built) {
        return Ok(());
    }
    std::fs::write(&out, built).map_err(|e| format!("{}: {e}", out.display()))
}

/// A script app's type-level mistakes ([`crate::typecheck`]); none for
/// any other package, or a source that cannot be read (the check reports
/// that).
pub fn type_problems(manifest: &Path) -> Vec<crate::typecheck::Problem> {
    let Ok(Some(source)) = script_source(manifest) else {
        return Vec::new();
    };
    if app_protocol::build::kind(&source).is_none()
        || source.starts_with('/')
        || source.split('/').any(|p| p == "..")
    {
        return Vec::new();
    }
    let dir = manifest.parent().unwrap_or(Path::new("."));
    match std::fs::read_to_string(dir.join(&source)) {
        Ok(text) => {
            crate::typecheck::check_granted(&source, &text, capabilities(manifest).as_deref())
        }
        Err(_) => Vec::new(),
    }
}

/// The methods a package's calls may use: its `capabilities`, and
/// `collect.query` when a data source is a collector (the host adds it).
/// None when the manifest cannot be read, which the check reports.
fn capabilities(manifest: &Path) -> Option<Vec<String>> {
    let table = std::fs::read_to_string(manifest)
        .ok()?
        .parse::<toml::Table>()
        .ok()?;
    let mut caps: Vec<String> = table
        .get("capabilities")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let collector = table
        .get("data")
        .and_then(|d| d.as_table())
        .is_some_and(|d| d.values().any(|s| s.get("collector").is_some()));
    if collector {
        caps.push("collect.query".into());
    }
    Some(caps)
}

/// Diagnostics as `name:line:column: message`.
struct Collect {
    cm: Lrc<SourceMap>,
    name: String,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Emitter for Collect {
    fn emit(&mut self, db: &mut DiagnosticBuilder<'_>) {
        let at = db
            .span
            .primary_span()
            .map(|s| self.cm.lookup_char_pos(s.lo));
        let line = match at {
            Some(loc) => format!(
                "{}:{}:{}: {}",
                self.name,
                loc.line,
                loc.col.0 + 1,
                db.message()
            ),
            None => format!("{}: {}", self.name, db.message()),
        };
        if let Ok(mut e) = self.errors.lock() {
            e.push(line);
        }
    }
}

/// `ts` (named `name`) as JavaScript, or every error with its line and
/// column (1-based).
pub fn strip(name: &str, ts: &str) -> Result<String, Vec<String>> {
    GLOBALS.set(&Globals::new(), || {
        let cm: Lrc<SourceMap> = Lrc::default();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let handler = Handler::with_emitter(
            true,
            false,
            Box::new(Collect {
                cm: cm.clone(),
                name: name.to_owned(),
                errors: errors.clone(),
            }),
        );
        let options = swc_ts_fast_strip::Options {
            module: Some(true),
            filename: Some(name.to_owned()),
            mode: swc_ts_fast_strip::Mode::StripOnly,
            ..Default::default()
        };
        let result = swc_ts_fast_strip::operate(&cm, &handler, ts.to_owned(), options);
        let mut errors = errors.lock().map(|e| e.clone()).unwrap_or_default();
        match result {
            Ok(out) if errors.is_empty() => Ok(out.code),
            Ok(_) => Err(errors),
            Err(e) => {
                if errors.is_empty() {
                    errors.push(format!("{name}: {e}"));
                }
                Err(errors)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_become_spaces_and_errors_name_their_place() {
        let ts = "const n: number = 1;\ninterface A { x: string }\nexport const f = (a: A): string => a.x;\n";
        let js = strip("app.ts", ts).expect("stripped");
        assert_eq!(js.lines().count(), ts.lines().count());
        assert!(js.starts_with("const n         = 1;"), "{js}");
        assert!(!js.contains("interface"));
        let e = strip("app.ts", "const a = 1;\nlet b: = 2;\n").expect_err("a syntax error");
        assert!(e[0].starts_with("app.ts:2:"), "{e:?}");
        let e = strip("app.ts", "\nenum Color { Red }\n").expect_err("not strippable");
        assert!(e[0].starts_with("app.ts:2:"), "{e:?}");
    }
}
