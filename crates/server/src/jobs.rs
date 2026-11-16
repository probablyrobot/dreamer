//! Jobs (spec 9, 12.1): exact programs run in their own tmux windows
//! through the `dream job-run` wrapper.
//!
//! A job belongs to tmux, not to the daemon: it keeps running, and its
//! wrapper still records its exit status, when the daemon stops or
//! crashes. Its record lives in `<state dir>/jobs/<id>/`:
//!
//! - `job.json`: what was started, where, and when (written by the daemon);
//! - `pid`: the program's pid (written by the wrapper);
//! - `status.json`: its exit status or signal (written by the wrapper);
//! - `cancelled`: present once `jobs.cancel` was asked;
//! - `artifacts/`: given to the job as `DREAMER_JOB_ARTIFACTS`.
//!
//! The daemon only reads these, so it knows every job again after a
//! restart. This differs from supervised extensions, which are the
//! daemon's own children and stop with it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use api::{JobRecord, JobState, RpcError, code};
use serde_json::{Value, json};

use crate::log;
use crate::service::State;

static COUNTER: AtomicU64 = AtomicU64::new(0);
/// Files listed per artifacts request.
const MAX_FILES: usize = 1000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn root() -> PathBuf {
    platform::state_dir().join("jobs")
}

fn dir_of(id: &str) -> Result<PathBuf, RpcError> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(RpcError::new(
            code::INVALID_PARAMS,
            format!("`{id}` is not a job id"),
        ));
    }
    let d = root().join(id);
    if !d.join("job.json").is_file() {
        return Err(RpcError::new(code::NOT_FOUND, format!("no job `{id}`")));
    }
    Ok(d)
}

/// The `dream` executable that runs the wrapper: `DREAMER_DREAM`, or the
/// one beside this daemon.
fn dream() -> Result<PathBuf, RpcError> {
    if let Some(p) = std::env::var_os("DREAMER_DREAM") {
        return Ok(PathBuf::from(p));
    }
    let p = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("dream")))
        .filter(|p| p.is_file())
        .ok_or_else(|| {
            RpcError::new(
                code::UNAVAILABLE,
                "the job wrapper (`dream` beside dreamerd) is missing",
            )
        })?;
    Ok(p)
}

fn read_json(path: &Path) -> Option<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

/// The record of the job in `dir`, from its files.
pub fn record(dir: &Path) -> Option<JobRecord> {
    let job = read_json(&dir.join("job.json"))?;
    let status = read_json(&dir.join("status.json"));
    let read_pid = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .and_then(|p| p.trim().parse::<u32>().ok())
    };
    let pid = read_pid("pid");
    // The wrapper writes the status just after its program exits.
    let finishing = read_pid("wrapper").is_some_and(platform::process_alive);
    let int = |v: &Value, k: &str| v.get(k).and_then(Value::as_i64);
    let (state, exit_code, signal, ended) = match &status {
        Some(s) => {
            let code = int(s, "exit_code").map(|c| c as i32);
            let sig = int(s, "signal").map(|c| c as i32);
            let state = match (code, sig) {
                (Some(0), _) => JobState::Succeeded,
                (Some(_), _) => JobState::Failed,
                (None, Some(_)) => JobState::Killed,
                _ => JobState::Failed,
            };
            (
                state,
                code,
                sig,
                s.get("ended_unix_ms").and_then(Value::as_u64),
            )
        }
        // No status yet: running while its program is, right after start
        // before the wrapper wrote the pid, or while the wrapper writes the
        // exit status. Lost only when both are gone with no status.
        None => match pid {
            Some(p) if !platform::process_alive(p) && !finishing => {
                (JobState::Lost, None, None, None)
            }
            _ => (JobState::Running, None, None, None),
        },
    };
    Some(JobRecord {
        id: job.get("id")?.as_str()?.to_owned(),
        argv: job
            .get("argv")?
            .as_array()?
            .iter()
            .filter_map(|a| a.as_str().map(str::to_owned))
            .collect(),
        cwd: job.get("cwd").and_then(Value::as_str).map(str::to_owned),
        pane: job.get("pane").and_then(Value::as_u64).unwrap_or_default() as u32,
        state: state as i32,
        exit_code,
        signal,
        started_unix_ms: job
            .get("started_unix_ms")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        ended_unix_ms: ended,
        artifacts: dir.join("artifacts").display().to_string(),
        cancelled: dir.join("cancelled").exists(),
        pid,
    })
}

pub fn list() -> api::JobList {
    let mut jobs: Vec<JobRecord> = std::fs::read_dir(root())
        .map(|d| d.flatten().filter_map(|e| record(&e.path())).collect())
        .unwrap_or_default();
    jobs.sort_by_key(|j| j.started_unix_ms);
    api::JobList { jobs }
}

pub fn get(id: &str) -> Result<JobRecord, RpcError> {
    let d = dir_of(id)?;
    record(&d).ok_or_else(|| {
        RpcError::new(
            code::INTERNAL,
            format!("job `{id}` has an unreadable record"),
        )
    })
}

pub fn cancel(id: &str) -> Result<JobRecord, RpcError> {
    let d = dir_of(id)?;
    let r = get(id)?;
    if r.state != JobState::Running as i32 {
        return Ok(r);
    }
    let _ = std::fs::write(d.join("cancelled"), "");
    if let Some(pid) = r.pid {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
    get(id)
}

pub fn artifacts(id: &str) -> Result<api::JobArtifacts, RpcError> {
    let d = dir_of(id)?.join("artifacts");
    let mut files = Vec::new();
    let mut stack = vec![d.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(e.path());
            } else if meta.is_file() && files.len() < MAX_FILES {
                let path = e
                    .path()
                    .strip_prefix(&d)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                files.push(api::JobFile {
                    path,
                    bytes: meta.len(),
                });
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(api::JobArtifacts {
        id: id.to_owned(),
        directory: d.display().to_string(),
        files,
    })
}

fn env_name_ok(n: &str) -> bool {
    let mut chars = n.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

pub async fn start(state: &Arc<State>, p: api::JobStart) -> Result<JobRecord, RpcError> {
    start_at(state, p, None).await
}

/// Start a job in a new window, or with `split`, in a new pane split from
/// that pane.
pub async fn start_at(
    state: &Arc<State>,
    p: api::JobStart,
    split: Option<(u32, tmux_backend::ops::Split)>,
) -> Result<JobRecord, RpcError> {
    if p.argv.is_empty() || p.argv[0].is_empty() {
        return Err(RpcError::new(code::INVALID_PARAMS, "a job needs a program"));
    }
    if let Some(bad) = p.env.iter().find(|e| !env_name_ok(&e.name)) {
        return Err(RpcError::new(
            code::INVALID_PARAMS,
            format!("`{}` is not an environment variable name", bad.name),
        ));
    }
    let dream = dream()?;
    let id = format!(
        "j{:x}{:x}",
        now_ms(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let dir = root().join(&id);
    let artifacts = dir.join("artifacts");
    platform::private_dir(&root()).map_err(|e| RpcError::new(code::INTERNAL, e))?;
    platform::private_dir(&dir).map_err(|e| RpcError::new(code::INTERNAL, e))?;
    std::fs::create_dir_all(&artifacts)
        .map_err(|e| RpcError::new(code::INTERNAL, e.to_string()))?;
    let mut argv = vec![
        dream.display().to_string(),
        "job-run".into(),
        dir.display().to_string(),
        "--".into(),
    ];
    argv.extend(p.argv.iter().cloned());
    let mut env: Vec<(String, String)> = p
        .env
        .iter()
        .map(|e| (e.name.clone(), e.value.clone()))
        .collect();
    env.push(("DREAMER_JOB_ID".into(), id.clone()));
    env.push((
        "DREAMER_JOB_ARTIFACTS".into(),
        artifacts.display().to_string(),
    ));
    let started = now_ms();
    let pane = {
        let guard = state.endpoint().await?;
        let ep = guard
            .as_ref()
            .ok_or_else(|| RpcError::new(code::UNAVAILABLE, "tmux is not connected"))?;
        let launch = tmux_backend::ops::Launch {
            argv,
            cwd: p.cwd.clone(),
            env,
            background: true,
        };
        let failed =
            |e| RpcError::new(code::INVALID_PARAMS, tmux_backend::ops::failure_message(&e));
        if let Some((target, direction)) = split {
            let pane = tmux_backend::ops::split_pane(
                &ep.conn,
                domain::TmuxPaneId(target),
                direction,
                None,
                &launch,
            )
            .await
            .map_err(failed)?;
            let _ = tmux_backend::ops::keep_on_exit(&ep.conn, pane).await;
            pane
        } else {
            let session = match p.session {
                Some(s) => s,
                None => workspace_host::topology(&ep.name, &ep.tracker)
                    .sessions
                    .first()
                    .map(|s| s.id)
                    .ok_or_else(|| RpcError::new(code::UNAVAILABLE, "tmux has no session"))?,
            };
            let name = p.name.clone().unwrap_or_else(|| {
                Path::new(&p.argv[0])
                    .file_name()
                    .map_or_else(|| p.argv[0].clone(), |n| n.to_string_lossy().into_owned())
            });
            let pane = tmux_backend::ops::new_window(
                &ep.conn,
                domain::TmuxSessionId(session),
                Some(&name),
                &launch,
            )
            .await
            .map_err(failed)?;
            // Its last output stays readable after it exits.
            let _ = tmux_backend::ops::keep_on_exit(&ep.conn, pane).await;
            pane
        }
    };
    let job = json!({
        "id": id,
        "argv": p.argv,
        "cwd": p.cwd,
        "pane": pane.0,
        "started_unix_ms": started,
    });
    std::fs::write(dir.join("job.json"), format!("{job}\n"))
        .map_err(|e| RpcError::new(code::INTERNAL, e.to_string()))?;
    log::info(format!(
        "jobs: started {id} in pane %{}: {:?}",
        pane.0, p.argv
    ));
    state.events.job(&id, "running", Some(pane.0));
    get(&id)
}

/// Record a `jobs.state` event for each job that ends while this daemon
/// runs, however short it was. Jobs that ended before it started (for
/// example while it was stopped) are not reported again.
pub async fn watch(state: Arc<State>) {
    let mut reported: std::collections::HashSet<String> = list()
        .jobs
        .into_iter()
        .filter(|j| j.state != JobState::Running as i32)
        .map(|j| j.id)
        .collect();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tick.tick().await;
        for j in list().jobs {
            if j.state != JobState::Running as i32 && reported.insert(j.id.clone()) {
                let name = JobState::try_from(j.state).map_or("unknown", JobState::name);
                state.events.job(&j.id, name, Some(j.pane));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job whose program has exited is still finishing while its wrapper
    /// writes the status, not lost: polling at that moment must not report
    /// a job that merely ended as lost.
    #[test]
    fn a_job_is_lost_only_when_program_and_wrapper_are_gone_without_a_status() {
        let dir = std::env::temp_dir().join(format!("drm-job-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("job.json"), r#"{"id":"j1","argv":["sh"]}"#).expect("job");
        let mut gone = std::process::Command::new("true").spawn().expect("spawn");
        let dead = gone.id();
        gone.wait().expect("wait");
        std::fs::write(dir.join("pid"), format!("{dead}\n")).expect("pid");
        std::fs::write(dir.join("wrapper"), format!("{}\n", std::process::id())).expect("wrapper");
        let state = |d: &Path| record(d).expect("record").state;
        assert_eq!(state(&dir), JobState::Running as i32, "finishing");
        std::fs::write(dir.join("wrapper"), format!("{dead}\n")).expect("wrapper");
        assert_eq!(state(&dir), JobState::Lost as i32);
        std::fs::write(dir.join("status.json"), r#"{"exit_code":3}"#).expect("status");
        assert_eq!(state(&dir), JobState::Failed as i32);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
