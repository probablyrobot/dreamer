//! `dream memory`: durable memory through the daemon (see docs/memory.md).

use std::io::Read;

use api::{Request, Response};

use crate::daemon::{DaemonArgs, call};

pub const USAGE: &str = "usage:
  dream memory recall [QUERY...] [--scope SCOPE] [--limit N]
  dream memory remember --kind KIND --scope SCOPE [--title T] [--description D]
                        [--link L]... TEXT... | -
  dream memory list [--scope SCOPE] [--kind KIND]
  dream memory get ID
  dream memory update ID [--text TEXT | --text -] [--kind KIND] [--title T]
                         [--description D] [--link L]... [--clear-links]
  dream memory forget ID
  dream memory import [--scope SCOPE]
  dream memory export [--scope SCOPE]
import copies the files provider's memories into the provider in force
(memory.provider), export copies the other way; both keep ids and times.
SCOPE: user | host | project | session.  KIND: user | feedback | project | reference.
Text after `--` is never read as an option. Every command takes [--json], [--cwd DIR] (default: this directory; it finds
the project), and [--session ID] (default: $DREAMER_SESSION_ID). `-` reads
the text from standard input.";

fn rt() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

fn scope(text: &str) -> Result<i32, String> {
    api::MemoryScope::from_name(text)
        .filter(|s| *s != api::MemoryScope::Unspecified)
        .map(|s| s as i32)
        .ok_or_else(|| format!("`{text}` is not a scope: user, host, project, or session"))
}

fn kind(text: &str) -> Result<i32, String> {
    api::MemoryKind::from_name(text)
        .filter(|k| *k != api::MemoryKind::Unspecified)
        .map(|k| k as i32)
        .ok_or_else(|| format!("`{text}` is not a kind: user, feedback, project, or reference"))
}

fn stdin_text() -> Result<String, String> {
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .map_err(|e| format!("standard input: {e}"))?;
    Ok(s)
}

/// The options every command shares, and the words and flags left.
#[derive(Debug, Default)]
struct Parsed {
    json: bool,
    context: api::MemoryContext,
    words: Vec<String>,
    scope: i32,
    kind: i32,
    limit: Option<u32>,
    title: Option<String>,
    description: Option<String>,
    text: Option<String>,
    links: Vec<String>,
    clear_links: bool,
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut p = Parsed::default();
    let mut cwd = std::env::current_dir()
        .ok()
        .map(|d| d.display().to_string());
    let mut session = std::env::var("DREAMER_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let mut it = args.iter();
    let mut words_only = false;
    while let Some(a) = it.next() {
        // After `--` every argument is text.
        if words_only || a == "--" {
            if words_only {
                p.words.push(a.clone());
            }
            words_only = true;
            continue;
        }
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n{USAGE}"))
        };
        match a.as_str() {
            "--json" => p.json = true,
            "--cwd" => cwd = Some(value()?),
            "--session" => session = Some(value()?),
            "--scope" => p.scope = scope(&value()?)?,
            "--kind" => p.kind = kind(&value()?)?,
            "--limit" => {
                let v = value()?;
                p.limit = Some(
                    v.parse()
                        .map_err(|_| format!("--limit takes a number, not `{v}`"))?,
                );
            }
            "--title" => p.title = Some(value()?),
            "--description" => p.description = Some(value()?),
            "--text" => p.text = Some(value()?),
            "--link" => p.links.push(value()?),
            "--clear-links" => p.clear_links = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other}\n{USAGE}"));
            }
            word => p.words.push(word.to_owned()),
        }
    }
    // The daemon resolves the project from an absolute directory.
    p.context.cwd = cwd.map(|d| std::path::absolute(&d).map_or(d, |a| a.display().to_string()));
    p.context.session = session;
    Ok(p)
}

fn one(words: &[String]) -> Result<String, String> {
    match words {
        [id] => Ok(id.clone()),
        _ => Err(USAGE.into()),
    }
}

pub fn run(d: &DaemonArgs, args: &[String]) -> Result<(), String> {
    let (verb, rest) = args.split_first().ok_or(USAGE)?;
    let p = parse(rest)?;
    let context = Some(p.context.clone());
    let request = match verb.as_str() {
        "recall" => Request::MemoryRecall(api::MemoryRecall {
            query: p.words.join(" "),
            scope: p.scope,
            limit: p.limit,
            context,
        }),
        "remember" => {
            let text = match p.words.as_slice() {
                [dash] if dash == "-" => stdin_text()?,
                [] => return Err(format!("remember needs the text\n{USAGE}")),
                words => words.join(" "),
            };
            if p.kind == 0 || p.scope == 0 {
                return Err(format!("remember needs --kind and --scope\n{USAGE}"));
            }
            Request::MemoryRemember(api::MemoryRemember {
                text,
                kind: p.kind,
                scope: p.scope,
                title: p.title,
                description: p.description,
                links: p.links,
                context,
            })
        }
        "list" if p.words.is_empty() => Request::MemoryList(api::MemoryListQuery {
            scope: p.scope,
            kind: p.kind,
            context,
        }),
        "get" => Request::MemoryGet(api::MemoryRef {
            id: one(&p.words)?,
            context,
        }),
        "update" => Request::MemoryUpdate(api::MemoryUpdate {
            id: one(&p.words)?,
            text: match p.text.as_deref() {
                Some("-") => Some(stdin_text()?),
                _ => p.text,
            },
            kind: p.kind,
            title: p.title,
            description: p.description,
            links: p.links,
            clear_links: p.clear_links,
            context,
        }),
        "forget" => Request::MemoryForget(api::MemoryRef {
            id: one(&p.words)?,
            context,
        }),
        "import" if p.words.is_empty() => Request::MemoryImport(api::MemoryTransfer {
            scope: p.scope,
            context,
        }),
        "export" if p.words.is_empty() => Request::MemoryExport(api::MemoryTransfer {
            scope: p.scope,
            context,
        }),
        _ => return Err(USAGE.into()),
    };
    let json = p.json;
    rt()?.block_on(async {
        let mut c = crate::remote::session(d).await?;
        // Run by an agent, it holds the agent's grant, not the owner's.
        crate::mcp::as_agent(&c).await?;
        let reply = call(&mut c, request).await?;
        if json {
            let text = match &reply {
                Response::MemoryRecall(r) | Response::MemoryList(r) => pretty(r),
                Response::MemoryRemember(m)
                | Response::MemoryGet(m)
                | Response::MemoryUpdate(m)
                | Response::MemoryForget(m) => pretty(m),
                Response::MemoryImport(t) | Response::MemoryExport(t) => pretty(t),
                other => return Err(format!("unexpected reply {other:?}")),
            }?;
            println!("{text}");
            return Ok(());
        }
        match reply {
            Response::MemoryRecall(r) => {
                if r.items.is_empty() {
                    println!("nothing recalled");
                }
                for m in &r.items {
                    println!(
                        "{} ({}, score {})",
                        m.id,
                        kind_name(m.kind),
                        m.score.unwrap_or(0)
                    );
                    println!("  {}", headline(m));
                    for l in m.text.lines() {
                        println!("  {l}");
                    }
                }
                problems(&r.problems);
            }
            Response::MemoryList(r) => {
                if r.items.is_empty() {
                    println!("no memories");
                }
                for m in &r.items {
                    println!("{:<36} {:<9} {}", m.id, kind_name(m.kind), headline(m));
                }
                problems(&r.problems);
            }
            Response::MemoryGet(m) => {
                println!("id:          {}", m.id);
                println!("kind:        {}", kind_name(m.kind));
                println!("title:       {}", m.title);
                println!("description: {}", m.description);
                for l in &m.links {
                    println!("link:        {l}");
                }
                if let Some(p) = &m.path {
                    println!("file:        {p}");
                }
                println!();
                println!("{}", m.text);
            }
            Response::MemoryRemember(m) => {
                println!("remembered {}{}", m.id, at(&m));
            }
            Response::MemoryUpdate(m) => println!("updated {}{}", m.id, at(&m)),
            Response::MemoryForget(m) => println!("forgot {}{}", m.id, at(&m)),
            Response::MemoryImport(t) | Response::MemoryExport(t) => {
                println!(
                    "copied {} {} from {} to {}",
                    t.ids.len(),
                    if t.ids.len() == 1 {
                        "memory"
                    } else {
                        "memories"
                    },
                    t.from,
                    t.to
                );
                for id in &t.ids {
                    println!("  {id}");
                }
                problems(&t.problems);
            }
            other => return Err(format!("unexpected reply {other:?}")),
        }
        Ok(())
    })
}

fn kind_name(k: i32) -> &'static str {
    api::MemoryKind::try_from(k).map_or("?", api::MemoryKind::name)
}

fn headline(m: &api::MemoryItem) -> String {
    if m.description.is_empty() || m.description == m.title {
        m.title.clone()
    } else {
        format!("{}: {}", m.title, m.description)
    }
}

fn at(m: &api::MemoryItem) -> String {
    m.path.as_ref().map_or(String::new(), |p| format!(" ({p})"))
}

fn problems(ps: &[api::MemoryProblem]) {
    for p in ps {
        eprintln!("note: cannot read {}: {}", p.path, p.message);
    }
}

fn pretty<T: serde::Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string_pretty(v).map_err(|e| e.to_string())
}
