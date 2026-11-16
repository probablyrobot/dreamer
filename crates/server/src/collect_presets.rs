//! Shipped collector definitions, used by name (`preset` in
//! `collect.create`; see `docs/collectors.md`). Each is a plain definition:
//! a part the caller gives replaces the preset's part.
//!
//! One collector has one line pattern, and the `regex` engine refuses a
//! group name used twice, so the formats are alternatives with numbered
//! groups, and each field's template joins the groups that can hold it:
//! only one alternative takes part in a match, so the others add nothing,
//! and a template that comes out empty adds no field.

use serde_json::json;

/// The presets' names, in the order the documentation lists them.
pub const NAMES: [&str; 5] = ["errors", "test-failures", "commands", "urls", "paths"];

/// Errors and warnings: gcc and clang, tsc (both formats), rustc, eslint
/// (stylish), Python and JavaScript exceptions, Go build errors and
/// panics, and log lines with an upper-case level.
const ERRORS: &str = concat!(
    // 1 file, 2 line, 3 col, 4 severity, 5 message: gcc, clang
    r"^([^\s:][^:]*?):(\d+):(?:(\d+):)?\s*(fatal error|error|warning):\s*(.*)$",
    // 6 file, 7 line, 8 col, 9 severity, 10 code, 11 message: tsc
    r"|^([^\s(][^(]*?)\((\d+),(\d+)\):\s*(error|warning)\s+(TS\d+):\s*(.*)$",
    // 12 file, 13 line, 14 col, 15 severity, 16 code, 17 message: tsc --pretty
    r"|^([^\s:][^:]*?):(\d+):(\d+)\s+-\s+(error|warning)\s+(TS\d+):\s*(.*)$",
    // 18 severity, 19 code, 20 message: rustc (the location is on a
    // following line, kept in `after`)
    r"|^(error|warning)(?:\[(E\d+)\])?:\s*(.*)$",
    // 21 line, 22 col, 23 severity, 24 message, 25 rule: eslint (stylish;
    // the file is the heading above)
    r"|^\s+(\d+):(\d+)\s+(error|warning)\s+(.+?)\s{2,}(\S+)\s*$",
    // 26 name before, 27 severity, 28 message: Python and JavaScript
    // exceptions (ValueError: ..., pytest's `E   ...`, Error: ...)
    r"|^(?:E\s+)?((?:[A-Za-z_]\w*\.)*[A-Za-z_]\w*?)?(Error|Exception|Warning):\s*(.*)$",
    // 29 file, 30 line, 31 col, 32 message: Go build errors
    r"|^(\S+\.go):(\d+)(?::(\d+))?:\s*(.*)$",
    // 33 severity, 34 message: Go panics and fatal errors
    r"|^(panic|fatal error):\s*(.*)$",
    // 35 severity, 36 message: a log line's level
    r"|\b(FATAL|CRITICAL|ERROR|WARNING|WARN)\b[\]:]*\s*(.*)$",
);

/// Failed tests: cargo test, go test, pytest, jest, and vitest.
const TEST_FAILURES: &str = concat!(
    // 1 test: cargo test
    r"^test (\S+) \.\.\. FAILED",
    // 2 test: go test
    r"|^\s*--- FAIL: (\S+)",
    // 3 test, 4 message: pytest's summary
    r"|^FAILED (\S+)(?: - (.*))?$",
    // 5 test: pytest -v
    r"|^(\S+::\S+) FAILED\b",
    // 6 test: jest and vitest marks
    r"|^\s*(?:✕|×|✗)\s+(.+?)(?:\s+\(?[\d.]+\s?m?s\)?)?\s*$",
    // 7 file, 8 test: jest's and vitest's FAIL lines, and go test's
    // failed package
    r"|^\s*FAIL\s+(\S+)(?:\s+>\s+(.*?))?(?:\s+\(?[\d.]+\s?m?s\)?)?\s*$",
);

const URLS: &str = r#"(?P<url>https?://[^\s<>"'`]*[^\s<>"'`.,;:!?)\]}])"#;

const PATHS: &str = r#"(?:^|[\s('"\[=])(?P<file>(?:~|\.{1,2})?/?(?:[\w.\-]+/)*[\w\-][\w.\-]*\.[A-Za-z]\w*):(?P<line>\d+)(?::(?P<col>\d+))?"#;

fn field(name: &str, value: &str) -> serde_json::Value {
    json!({ "name": name, "value": value })
}

/// The preset `name`'s definition.
pub fn preset(name: &str) -> Option<api::CollectorSpec> {
    let v = match name {
        "errors" => json!({
            "from": { "all": true },
            "when": { "line": ERRORS },
            "capture": {
                "fields": [
                    field("severity", "${4}${9}${15}${18}${23}${27}${33}${35}"),
                    field("code", "${10}${16}${19}${25}${26}${27}"),
                    field("file", "${1}${6}${12}${29}"),
                    field("line", "${2}${7}${13}${21}${30}"),
                    field("col", "${3}${8}${14}${22}${31}"),
                    field("message", "${5}${11}${17}${20}${24}${28}${32}${34}${36}"),
                ],
                "before": 5,
                "after": 3,
            },
        }),
        "test-failures" => json!({
            "from": { "all": true },
            "when": { "line": TEST_FAILURES },
            "capture": {
                "fields": [
                    field("test", "${1}${2}${3}${5}${6}${8}"),
                    field("file", "${7}"),
                    field("message", "${4}"),
                ],
                "after": 5,
            },
        }),
        "commands" => json!({
            "from": { "all": true },
            "when": { "command_end": true },
            "capture": { "lines": 20 },
        }),
        "urls" => json!({
            "from": { "all": true },
            "when": { "line": URLS },
        }),
        "paths" => json!({
            "from": { "all": true },
            "when": { "line": PATHS },
        }),
        _ => return None,
    };
    let mut spec: api::CollectorSpec = serde_json::from_value(v).ok()?;
    spec.name = name.to_owned();
    spec.preset = Some(name.to_owned());
    Some(spec)
}

/// `spec` with its preset filled in: each part it gives replaces the
/// preset's, and its name defaults to the preset's. A definition without a
/// preset is returned as it is.
pub fn apply(spec: api::CollectorSpec) -> Result<api::CollectorSpec, String> {
    let Some(name) = spec.preset.clone().filter(|p| !p.is_empty()) else {
        return Ok(spec);
    };
    let Some(mut out) = preset(&name) else {
        return Err(format!(
            "preset: no preset `{name}`; the presets are {}",
            NAMES.join(", ")
        ));
    };
    if !spec.name.is_empty() {
        out.name = spec.name;
    }
    if spec.from.is_some() {
        out.from = spec.from;
    }
    if spec.when.is_some() {
        out.when = spec.when;
    }
    if spec.capture.is_some() {
        out.capture = spec.capture;
    }
    if spec.keep.is_some() {
        out.keep = spec.keep;
    }
    if spec.per_second.is_some() {
        out.per_second = spec.per_second;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_is_a_valid_definition() {
        for name in NAMES {
            let spec = preset(name).expect("a preset");
            crate::collectors::check(&spec).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        assert!(preset("nope").is_none());
    }

    #[test]
    fn a_given_part_replaces_the_presets() {
        let spec = apply(api::CollectorSpec {
            preset: Some("errors".into()),
            from: Some(api::CollectFrom {
                panes: vec![3],
                ..api::CollectFrom::default()
            }),
            ..api::CollectorSpec::default()
        })
        .expect("applied");
        assert_eq!(spec.name, "errors");
        assert_eq!(spec.from.expect("from").panes, [3]);
        assert!(spec.when.expect("when").line.is_some());
        let spec = apply(api::CollectorSpec {
            name: "mine".into(),
            preset: Some("commands".into()),
            capture: Some(api::CollectCapture::default()),
            ..api::CollectorSpec::default()
        })
        .expect("applied");
        assert_eq!(spec.name, "mine");
        assert_eq!(spec.capture.expect("capture").lines, None);
        let e = apply(api::CollectorSpec {
            preset: Some("nope".into()),
            ..api::CollectorSpec::default()
        })
        .expect_err("unknown");
        assert!(e.contains("test-failures"), "{e}");
    }

    /// The fields `preset` records for `line`, sorted; `None` when the
    /// line makes no row.
    fn fields(name: &str, line: &str) -> Option<Vec<(String, String)>> {
        crate::collectors::sample(&preset(name).expect("preset"), line)
    }

    fn has(got: &Option<Vec<(String, String)>>, want: &[(&str, &str)]) {
        let got = got.as_ref().expect("a row");
        for (k, v) in want {
            assert!(
                got.iter().any(|(gk, gv)| gk == k && gv == v),
                "want {k}={v:?} in {got:?}"
            );
        }
    }

    #[test]
    fn errors_and_warnings_of_common_formats() {
        let f = |l: &str| fields("errors", l);
        // gcc and clang
        has(
            &f("src/main.c:12:5: error: expected ';' before 'return'"),
            &[
                ("file", "src/main.c"),
                ("line", "12"),
                ("col", "5"),
                ("severity", "error"),
                ("message", "expected ';' before 'return'"),
            ],
        );
        has(
            &f("lib.c:3: warning: implicit declaration of function 'f'"),
            &[("file", "lib.c"), ("line", "3"), ("severity", "warning")],
        );
        // rustc
        has(
            &f("error[E0308]: mismatched types"),
            &[
                ("severity", "error"),
                ("code", "E0308"),
                ("message", "mismatched types"),
            ],
        );
        has(
            &f("warning: unused variable: `x`"),
            &[("severity", "warning"), ("message", "unused variable: `x`")],
        );
        // tsc, both formats
        has(
            &f("src/app.ts(4,7): error TS2322: Type 'string' is not assignable to type 'number'."),
            &[
                ("file", "src/app.ts"),
                ("line", "4"),
                ("col", "7"),
                ("severity", "error"),
                ("code", "TS2322"),
            ],
        );
        has(
            &f("src/app.ts:4:7 - error TS2322: Type 'string' is not assignable."),
            &[("file", "src/app.ts"), ("code", "TS2322"), ("col", "7")],
        );
        // eslint (stylish)
        has(
            &f("  12:5  error  'x' is not defined  no-undef"),
            &[
                ("line", "12"),
                ("col", "5"),
                ("severity", "error"),
                ("message", "'x' is not defined"),
                ("code", "no-undef"),
            ],
        );
        // Python and JavaScript
        has(
            &f("ValueError: invalid literal for int() with base 10: 'x'"),
            &[("severity", "Error"), ("code", "ValueError")],
        );
        has(
            &f("E   AssertionError: assert 1 == 2"),
            &[("code", "AssertionError"), ("message", "assert 1 == 2")],
        );
        has(
            &f("requests.exceptions.ConnectionError: refused"),
            &[("code", "requests.exceptions.ConnectionError")],
        );
        has(
            &f("TypeError: undefined is not a function"),
            &[("code", "TypeError")],
        );
        // Go
        has(
            &f("./main.go:12:2: undefined: foo"),
            &[
                ("file", "./main.go"),
                ("line", "12"),
                ("col", "2"),
                ("message", "undefined: foo"),
            ],
        );
        has(
            &f("panic: runtime error: index out of range [3] with length 3"),
            &[("severity", "panic")],
        );
        // Log levels
        has(
            &f("2026-11-16 12:00:01 ERROR db: connection lost"),
            &[("severity", "ERROR"), ("message", "db: connection lost")],
        );
        has(&f("[WARN] disk 91% full"), &[("severity", "WARN")]);
        // Not errors.
        for l in [
            "test result: ok. 3 passed; 0 failed",
            "Compiling dreamer v0.1.0",
            "no errors found",
            "ERROR_COUNT=0",
        ] {
            assert_eq!(f(l), None, "{l}");
        }
        // A format without a part adds no field for it.
        let got = f("error: could not compile `x`").expect("a row");
        assert!(!got.iter().any(|(k, _)| k == "file"), "{got:?}");
    }

    #[test]
    fn test_failures_of_common_runners() {
        let f = |l: &str| fields("test-failures", l);
        has(
            &f("test parse::tests::rejects_empty ... FAILED"),
            &[("test", "parse::tests::rejects_empty")],
        );
        has(
            &f("    --- FAIL: TestParse/empty (0.00s)"),
            &[("test", "TestParse/empty")],
        );
        has(
            &f("FAIL\texample.com/m/parse\t0.012s"),
            &[("file", "example.com/m/parse")],
        );
        has(
            &f("FAILED tests/test_parse.py::test_empty - AssertionError: assert 1 == 2"),
            &[
                ("test", "tests/test_parse.py::test_empty"),
                ("message", "AssertionError: assert 1 == 2"),
            ],
        );
        has(
            &f("tests/test_parse.py::test_empty FAILED                    [ 50%]"),
            &[("test", "tests/test_parse.py::test_empty")],
        );
        has(
            &f("    ✕ adds two numbers (5 ms)"),
            &[("test", "adds two numbers")],
        );
        has(
            &f(" FAIL  src/sum.test.ts > sum > adds two numbers"),
            &[
                ("file", "src/sum.test.ts"),
                ("test", "sum > adds two numbers"),
            ],
        );
        has(&f(" FAIL  src/sum.test.js"), &[("file", "src/sum.test.js")]);
        has(
            &f("   × adds two numbers 3ms"),
            &[("test", "adds two numbers")],
        );
        for l in [
            "test parse::tests::ok ... ok",
            "test result: FAILED. 1 passed; 1 failed",
            "--- PASS: TestOk (0.00s)",
            "PASSED tests/test_parse.py::test_ok",
            "    ✓ adds two numbers (1 ms)",
        ] {
            assert_eq!(f(l), None, "{l}");
        }
    }

    #[test]
    fn urls_and_paths_with_line_numbers() {
        has(
            &fields("urls", "see https://example.com/a?b=1, then"),
            &[("url", "https://example.com/a?b=1")],
        );
        has(
            &fields("urls", "(http://localhost:8080/x)."),
            &[("url", "http://localhost:8080/x")],
        );
        assert_eq!(fields("urls", "no link here"), None);
        has(
            &fields("paths", "  --> crates/server/src/main.rs:41:9"),
            &[
                ("file", "crates/server/src/main.rs"),
                ("line", "41"),
                ("col", "9"),
            ],
        );
        has(
            &fields("paths", "at Object.<anonymous> (/srv/app/index.js:7:13)"),
            &[("file", "/srv/app/index.js"), ("line", "7")],
        );
        has(
            &fields("paths", "./main.go:12: undefined"),
            &[("file", "./main.go"), ("line", "12")],
        );
        for l in [
            "started at 12:30:45",
            "listening on http://localhost:8080",
            "version 1.2:3",
        ] {
            assert_eq!(fields("paths", l), None, "{l}");
        }
    }
}
