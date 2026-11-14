//! The host process registry (spec 13.1): the user's processes, which
//! tmux pane each belongs to, and which are agent runs, each with the
//! evidence behind it.
//!
//! A process is named by its birth identity, its pid and when it started,
//! so a reused pid is a different process. Nothing here is guessed: a
//! process is in a pane because it descends from the pane's process (or,
//! more weakly, shares its terminal); it is a run of a provider because its
//! program says so, with a confidence; and a field the system did not give
//! is absent, not invented. Pane titles, CPU use, and output are not
//! evidence of what a run is doing.
//!
//! The snapshot comes from `ps` (POSIX fields that read the same on Linux
//! and macOS), with the working directory from `/proc` on Linux and from
//! `lsof` on macOS, only for runs.

use std::collections::HashMap;

/// One process, as `ps` reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub uid: u32,
    /// The controlling terminal, when there is one.
    pub tty: Option<String>,
    /// When it started, from its elapsed time, to within a second.
    pub started_unix_s: u64,
    /// CPU use, in tenths of a percent of one core, as `ps` averages it.
    pub cpu_permille: u32,
    /// Resident memory in KiB.
    pub rss_kib: u64,
    /// Its command line, as `ps` shows it.
    pub args: String,
}

impl Proc {
    /// Birth identity: two processes with one pid are different when they
    /// started at different times. `ps` gives seconds, and a start computed
    /// from elapsed time can differ by one between looks.
    pub fn same_process(&self, other: &Proc) -> bool {
        self.pid == other.pid && self.started_unix_s.abs_diff(other.started_unix_s) <= 1
    }

    /// The program's file name.
    pub fn command(&self) -> &str {
        let argv0 = self.args.split_whitespace().next().unwrap_or_default();
        argv0.rsplit('/').next().unwrap_or(argv0)
    }
}

/// `[[dd-]hh:]mm:ss` as seconds.
fn elapsed(e: &str) -> Option<u64> {
    let (days, rest) = match e.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, e),
    };
    let parts: Vec<u64> = rest
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let (h, m, s) = match parts.as_slice() {
        [m, s] => (0, *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    Some(((days * 24 + h) * 60 + m) * 60 + s)
}

/// Parse `ps -o pid=,ppid=,pgid=,uid=,tty=,%cpu=,rss=,etime=,args=` output taken at
/// `now_s`. Lines that do not parse are skipped.
pub fn parse_ps(text: &str, now_s: u64) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let pgid = it.next()?.parse().ok()?;
            let uid = it.next()?.parse().ok()?;
            let tty = it.next()?;
            let cpu: f64 = it.next()?.replace(',', ".").parse().ok()?;
            let rss: u64 = it.next()?.parse().ok()?;
            let started = now_s.checked_sub(elapsed(it.next()?)?)?;
            let args = it.collect::<Vec<_>>().join(" ");
            Some(Proc {
                pid,
                ppid,
                pgid,
                uid,
                tty: (!tty.starts_with('?') && tty != "-").then(|| tty.to_owned()),
                started_unix_s: started,
                cpu_permille: (cpu * 10.0).round().max(0.0) as u32,
                rss_kib: rss,
                args,
            })
        })
        .collect()
}

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The current user's processes.
pub fn snapshot() -> Result<Vec<Proc>, String> {
    let out = std::process::Command::new("ps")
        .env("LC_ALL", "C")
        .args([
            "-axww",
            "-o",
            "pid=,ppid=,pgid=,uid=,tty=,%cpu=,rss=,etime=,args=",
        ])
        .output()
        .map_err(|e| format!("ps: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ps: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let me = platform::uid();
    Ok(parse_ps(&String::from_utf8_lossy(&out.stdout), now_s())
        .into_iter()
        .filter(|p| p.uid == me)
        .collect())
}

/// A process's working directory, when the system says (`None` otherwise).
pub fn cwd(pid: u32) -> Option<String> {
    if cfg!(target_os = "linux") {
        return std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(|p| p.display().to_string());
    }
    let out = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n').map(str::to_owned))
}

/// Why a process is counted in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneEvidence {
    /// It descends from the pane's process.
    Ancestry,
    /// It has the pane's terminal but does not descend from its process
    /// (it was reparented, or started there another way).
    Terminal,
}

/// How sure the registry is that a process is a provider's run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// An agent CLI the registry recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

/// Whether `p` is a provider's CLI, and how sure that is, with the
/// evidence.
pub fn provider_of(p: &Proc) -> Option<(Provider, Confidence, &'static str)> {
    let words: Vec<&str> = p.args.split_whitespace().collect();
    let base = |w: &str| w.rsplit('/').next().unwrap_or(w).to_owned();
    let argv0 = words.first().map(|w| base(w)).unwrap_or_default();
    for (provider, name, package) in [
        (Provider::Claude, "claude", "@anthropic-ai/claude-code"),
        (Provider::Codex, "codex", "@openai/codex"),
    ] {
        // The program itself is the CLI.
        if argv0 == name {
            return Some((
                provider,
                Confidence::High,
                "the program is the provider's CLI",
            ));
        }
        // A runtime (node, bun) running the CLI's package or script.
        let runtime = ["node", "bun", "deno"].contains(&argv0.as_str());
        if runtime && words.iter().skip(1).any(|w| w.contains(package)) {
            return Some((
                provider,
                Confidence::High,
                "a runtime runs the provider's package",
            ));
        }
        if runtime && words.get(1).is_some_and(|w| base(w) == name) {
            return Some((
                provider,
                Confidence::Medium,
                "a runtime runs a script named like the CLI",
            ));
        }
    }
    None
}

/// A pane: its id and the pid of its program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pane {
    pub id: u32,
    pub pid: u32,
}

/// What the registry says about one process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub proc: Proc,
    pub pane: Option<(u32, PaneEvidence)>,
    /// When it is a provider's run (the topmost process of that run).
    pub run: Option<(Provider, Confidence, &'static str)>,
    /// When it serves a run: that run's pid (a tool, an MCP server, a
    /// shell the run started). Helpers are never runs themselves.
    pub helper_of: Option<u32>,
}

/// Every process with its pane, run, and helper evidence.
pub fn registry(procs: &[Proc], panes: &[Pane]) -> Vec<Entry> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    // Each pane's terminal, from its program's own entry.
    let pane_tty: Vec<(u32, &str)> = panes
        .iter()
        .filter_map(|pn| {
            by_pid
                .get(&pn.pid)
                .and_then(|p| p.tty.as_deref())
                .map(|t| (pn.id, t))
        })
        .collect();
    // Ancestors, nearest first, stopping at a loop or after 128 steps.
    let ancestors = |p: &Proc| {
        let mut chain = Vec::new();
        let mut cur = p.ppid;
        while cur > 1 && chain.len() < 128 && !chain.contains(&cur) {
            chain.push(cur);
            cur = by_pid.get(&cur).map_or(0, |q| q.ppid);
        }
        chain
    };
    procs
        .iter()
        .map(|p| {
            let chain = ancestors(p);
            let pane = panes
                .iter()
                .find(|pn| pn.pid == p.pid || chain.contains(&pn.pid))
                .map(|pn| (pn.id, PaneEvidence::Ancestry))
                .or_else(|| {
                    let tty = p.tty.as_deref()?;
                    pane_tty
                        .iter()
                        .find(|(_, t)| *t == tty)
                        .map(|(id, _)| (*id, PaneEvidence::Terminal))
                });
            // The nearest ancestor that is itself a run makes this a helper.
            let helper_of = chain
                .iter()
                .copied()
                .find(|a| by_pid.get(a).is_some_and(|q| provider_of(q).is_some()));
            let run = if helper_of.is_none() {
                provider_of(p).map(|(prov, conf, why)| {
                    // Outside any pane it may be anything; say so.
                    let conf = if pane.is_none() {
                        conf.min(Confidence::Medium)
                    } else {
                        conf
                    };
                    (prov, conf, why)
                })
            } else {
                None
            };
            Entry {
                proc: p.clone(),
                pane,
                run,
                helper_of,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "\
  1 0 1 0 ?? 1.5 2048 10-00:00:00 /sbin/launchd
  100 1 100 501 ttys001 1.5 2048 01:00:00 -zsh
  200 100 200 501 ttys001 1.5 2048 10:00 node /Users/u/.npm/lib/node_modules/@anthropic-ai/claude-code/cli.js
  210 200 200 501 ttys001 1.5 2048 09:00 /bin/zsh -c npm test
  211 210 200 501 ttys001 1.5 2048 08:59 node /repo/node_modules/.bin/jest
  220 200 200 501 ttys001 1.5 2048 09:30 node /Users/u/mcp-server/index.js
  300 1 300 501 ttys002 1.5 2048 02:00:00 -zsh
  310 300 310 501 ttys002 1.5 2048 05:00 codex --model gpt-5
  400 1 400 501 ttys001 1.5 2048 01:00 vim notes.txt
  500 1 500 501 ?? 1.5 2048 1:00:00 claude --print hi
  600 100 600 501 ttys001 1.5 2048 00:01 grep claude
";

    fn procs() -> Vec<Proc> {
        parse_ps(PS, 1_000_000)
    }

    #[test]
    fn ps_output_parses_with_start_times_and_terminals() {
        let p = procs();
        assert_eq!(p.len(), 11);
        assert_eq!(p[0].started_unix_s, 1_000_000 - 10 * 86_400);
        assert_eq!(p[2].started_unix_s, 1_000_000 - 600);
        assert_eq!(p[0].tty, None);
        assert_eq!(p[1].tty.as_deref(), Some("ttys001"));
        assert_eq!(p[2].command(), "node");
        assert_eq!((p[2].cpu_permille, p[2].rss_kib), (15, 2048));
        assert_eq!(parse_ps("garbage\n 1 2 3\n", 10), Vec::new());
    }

    #[test]
    fn a_reused_pid_is_another_process() {
        let a = procs()[2].clone();
        let mut b = a.clone();
        b.started_unix_s += 1;
        assert!(a.same_process(&b), "a second apart is the same start");
        b.started_unix_s += 60;
        assert!(
            !a.same_process(&b),
            "same pid, later start: another process"
        );
    }

    #[test]
    fn runs_panes_and_helpers_come_from_evidence() {
        let panes = [Pane { id: 1, pid: 100 }, Pane { id: 2, pid: 300 }];
        let r = registry(&procs(), &panes);
        let get = |pid| r.iter().find(|e| e.proc.pid == pid).expect("entry");
        // Claude through node, in pane 1 by ancestry: high confidence.
        let claude = get(200);
        assert_eq!(claude.pane, Some((1, PaneEvidence::Ancestry)));
        assert_eq!(
            claude.run.map(|r| (r.0, r.1)),
            Some((Provider::Claude, Confidence::High))
        );
        // Its tools and MCP server are helpers, not runs.
        for helper in [210, 211, 220] {
            assert_eq!(get(helper).helper_of, Some(200), "{helper}");
            assert!(get(helper).run.is_none());
        }
        // Codex in pane 2.
        assert_eq!(get(310).run.map(|r| r.0), Some(Provider::Codex));
        assert_eq!(get(310).pane, Some((2, PaneEvidence::Ancestry)));
        // Reparented onto the pane's terminal: in the pane, but weaker.
        assert_eq!(get(400).pane, Some((1, PaneEvidence::Terminal)));
        // A claude outside any pane is at most medium; `grep claude` is no run.
        assert_eq!(get(500).pane, None);
        assert_eq!(get(500).run.map(|r| r.1), Some(Confidence::Medium));
        assert!(get(600).run.is_none());
        // Nothing invented: no terminal, no pane.
        assert_eq!(get(1).pane, None);
    }
}
