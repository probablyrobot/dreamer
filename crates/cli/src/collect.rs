//! `dream collect`: collectors, standing rules that turn pane output into
//! rows (see docs/collectors.md).

use api::{Request, Response};

use crate::daemon::{DaemonArgs, call};

pub const USAGE: &str = "usage:
  dream collect create -f FILE
      a definition in YAML or JSON (see docs/collectors.md)
  dream collect create NAME FROM WHEN [CAPTURE] [KEEP]
      FROM:    --all | --session NAME | --window @N | --pane %N... | --command REGEX
      WHEN:    --line REGEX [--ignore-case] | --command-end | --idle MS | --bell
               | --exit | --every SECONDS
      CAPTURE: [--field NAME=TEMPLATE]... [--json-line] [--screen | --lines N]
               [--before N] [--after N]
      KEEP:    [--keep-rows N] [--keep-for DURATION] [--per-second N]
  dream collect list [--json]
  dream collect query NAME [--pane %N]... [--field NAME=VALUE]... [--since DURATION]
                           [--after ID] [--limit N] [--group-by FIELD] [--json]
  dream collect stop NAME [--delete]
DURATION is a number with s, m, h, or d (30s, 10m, 7d).";

fn rt() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

/// Seconds in a duration like `30s`, `10m`, `2h`, or `7d`.
fn seconds(text: &str) -> Result<u64, String> {
    let bad = || format!("`{text}` is not a duration (30s, 10m, 2h, 7d)");
    let (n, unit) = text.split_at(text.len().saturating_sub(1));
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(bad()),
    };
    n.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(bad)
}

fn pane(text: &str) -> Result<u32, String> {
    text.trim_start_matches('%')
        .parse()
        .map_err(|_| format!("`{text}` is not a pane (%N)"))
}

fn pair(text: &str) -> Result<api::CollectField, String> {
    let (name, value) = text
        .split_once('=')
        .ok_or_else(|| format!("`{text}` is not NAME=VALUE"))?;
    Ok(api::CollectField {
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

fn number<T: std::str::FromStr>(flag: &str, text: &str) -> Result<T, String> {
    text.parse()
        .map_err(|_| format!("{flag} takes a number, not `{text}`"))
}

/// A definition from flags.
fn from_flags(name: &str, args: &[String]) -> Result<api::CollectorSpec, String> {
    let mut from = api::CollectFrom::default();
    let mut when = api::CollectWhen::default();
    let mut capture = api::CollectCapture::default();
    let mut keep = api::CollectKeep::default();
    let mut spec = api::CollectorSpec {
        name: name.to_owned(),
        ..api::CollectorSpec::default()
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n{USAGE}"))
        };
        match a.as_str() {
            "--all" => from.all = true,
            "--session" => from.session = Some(value()?),
            "--window" => {
                let v = value()?;
                from.window = Some(number("--window", v.trim_start_matches('@'))?);
            }
            "--pane" => from.panes.push(pane(&value()?)?),
            "--command" => from.command = Some(value()?),
            "--line" => when.line = Some(value()?),
            "--ignore-case" => when.ignore_case = true,
            "--command-end" => when.command_end = true,
            "--idle" => when.idle_ms = Some(number("--idle", &value()?)?),
            "--bell" => when.bell = true,
            "--exit" => when.exit = true,
            "--every" => when.every_s = Some(number("--every", &value()?)?),
            "--field" => capture.fields.push(pair(&value()?)?),
            "--json-line" => capture.json = true,
            "--screen" => capture.screen = true,
            "--lines" => capture.lines = Some(number("--lines", &value()?)?),
            "--before" => capture.before = number("--before", &value()?)?,
            "--after" => capture.after = number("--after", &value()?)?,
            "--keep-rows" => keep.rows = Some(number("--keep-rows", &value()?)?),
            "--keep-for" => keep.max_age_s = Some(seconds(&value()?)?),
            "--per-second" => spec.per_second = Some(number("--per-second", &value()?)?),
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    spec.from = Some(from);
    spec.when = Some(when);
    spec.capture = Some(capture);
    spec.keep = Some(keep);
    Ok(spec)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn ago(unix_ms: u64) -> String {
    let s = now_ms().saturating_sub(unix_ms) / 1000;
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

fn describe_when(w: &api::CollectWhen) -> String {
    if let Some(l) = &w.line {
        format!("line /{l}/")
    } else if w.command_end {
        "command end".into()
    } else if let Some(ms) = w.idle_ms {
        format!("idle {ms} ms")
    } else if w.bell {
        "bell".into()
    } else if w.exit {
        "exit".into()
    } else if let Some(s) = w.every_s {
        format!("every {s} s")
    } else {
        "-".into()
    }
}

pub fn run(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let json = args.iter().any(|a| a == "--json");
    let args: Vec<String> = args.iter().filter(|a| *a != "--json").cloned().collect();
    let request = match args.as_slice() {
        [v, f, file] if v == "create" && (f == "-f" || f == "--file") => {
            let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
            // JSON is YAML too.
            let spec: api::CollectorSpec =
                serde_norway::from_str(&text).map_err(|e| format!("{file}: {e}"))?;
            Request::CollectCreate(spec)
        }
        [v, name, rest @ ..] if v == "create" && !name.starts_with('-') => {
            Request::CollectCreate(from_flags(name, rest)?)
        }
        [v] if v == "list" => Request::CollectList(api::Empty {}),
        [] => Request::CollectList(api::Empty {}),
        [v, name] if v == "stop" => Request::CollectStop(api::CollectRef {
            name: name.clone(),
            delete: false,
        }),
        [v, name, d] if v == "stop" && d == "--delete" => Request::CollectStop(api::CollectRef {
            name: name.clone(),
            delete: true,
        }),
        [v, name, rest @ ..] if v == "query" => {
            let mut q = api::CollectQuery {
                name: name.clone(),
                ..api::CollectQuery::default()
            };
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                let value = it
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{a} needs a value\n{USAGE}"))?;
                match a.as_str() {
                    "--pane" => q.panes.push(pane(&value)?),
                    "--field" => q.fields.push(pair(&value)?),
                    "--since" => {
                        q.since_unix_ms =
                            Some(now_ms().saturating_sub(seconds(&value)?.saturating_mul(1000)));
                    }
                    "--after" => q.after = Some(number("--after", &value)?),
                    "--limit" => q.limit = Some(number("--limit", &value)?),
                    "--group-by" => q.group_by = Some(value),
                    other => return Err(format!("unknown option {other}\n{USAGE}")),
                }
            }
            Request::CollectQuery(q)
        }
        _ => return Err(USAGE.into()),
    };
    rt()?.block_on(async {
        let mut c = crate::remote::session(d).await?;
        match call(&mut c, request).await? {
            Response::CollectList(l) if json => println!("{}", pretty(&l)?),
            Response::CollectList(l) => {
                if l.collectors.is_empty() {
                    println!("no collectors");
                }
                for s in l.collectors {
                    let state = api::CollectorState::try_from(s.state)
                        .map_or("?", api::CollectorState::name);
                    let when = s
                        .spec
                        .as_ref()
                        .and_then(|s| s.when.as_ref())
                        .map(describe_when)
                        .unwrap_or_default();
                    println!(
                        "{:<20} {:<8} {:>6} rows  {:>7} collected  {:>6} dropped  {} pane(s)  {when}",
                        s.name,
                        state,
                        s.rows,
                        s.collected,
                        s.dropped,
                        s.panes.len()
                    );
                    if let Some(e) = s.last_error {
                        println!("{:<20} note: {e}", "");
                    }
                }
            }
            Response::CollectQuery(r) if json => println!("{}", pretty(&r)?),
            Response::CollectQuery(r) => {
                for g in &r.groups {
                    let value = if g.value.is_empty() { "(none)" } else { &g.value };
                    println!("{:>7}  {value}  (latest {})", g.count, ago(g.latest_unix_ms));
                }
                for row in &r.rows {
                    let fields: Vec<String> = row
                        .fields
                        .iter()
                        .map(|f| format!("{}={}", f.name, f.value))
                        .collect();
                    println!(
                        "#{:<6} {:>8}  %{:<4} {:<12} {}{}{}",
                        row.id,
                        ago(row.time_unix_ms),
                        row.pane,
                        row.trigger,
                        fields.join(" "),
                        if fields.is_empty() || row.line.is_empty() {
                            ""
                        } else {
                            "  "
                        },
                        row.line
                    );
                }
                if r.truncated {
                    println!("({} matched; use --limit or --after for more)", r.matched);
                }
                if r.dropped > 0 {
                    println!("({} rows were dropped by the rate cap)", r.dropped);
                }
            }
            Response::CollectCreate(s) | Response::CollectStop(s) if json => {
                println!("{}", pretty(&s)?);
            }
            Response::CollectCreate(s) => println!(
                "collector `{}` running on {} pane(s)",
                s.name,
                s.panes.len()
            ),
            Response::CollectStop(s) => println!("collector `{}` stopped", s.name),
            other => return Err(format!("unexpected reply {other:?}")),
        }
        Ok(())
    })
}

/// Pretty JSON for the replies above.
fn pretty<T: serde::Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string_pretty(v).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_files_make_the_same_definition() {
        let flags: Vec<String> = [
            "--all",
            "--line",
            r"^FAILED (\S+)",
            "--field",
            "test=$1",
            "--after",
            "2",
            "--keep-for",
            "7d",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let a = from_flags("fails", &flags).expect("flags");
        let yaml = r#"
name: fails
from: { all: true }
when: { line: "^FAILED (\\S+)" }
capture: { fields: [{ name: test, value: "$1" }], after: 2 }
keep: { max_age_s: 604800 }
"#;
        let b: api::CollectorSpec = serde_norway::from_str(yaml).expect("yaml");
        assert_eq!(a, b);
        assert_eq!(seconds("10m"), Ok(600));
        assert!(seconds("10").is_err());
    }
}
